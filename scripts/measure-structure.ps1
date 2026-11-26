#requires -Version 7.0
# 只用新建的隔离项目：500 个文件，每个 100 行有效源码。不会打开开发库。
[CmdletBinding()]
param([string]$Executable = (Join-Path $PSScriptRoot '../target/release/loom.exe'))
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $Executable).Path
$parent = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../target/structure-bench'))
[IO.Directory]::CreateDirectory($parent) | Out-Null
$fixture = Join-Path $parent ([guid]::NewGuid().ToString('N'))
New-Item -Path $fixture -ItemType Directory -ErrorAction Stop | Out-Null
$encoding = [Text.UTF8Encoding]::new($false)

function Invoke-Sample([string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($exe)
    $info.WorkingDirectory = $fixture
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in $Arguments) { $info.ArgumentList.Add($arg) }
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $p = [Diagnostics.Process]::Start($info)
    try {
        $stdout = $p.StandardOutput.ReadToEndAsync()
        $stderr = $p.StandardError.ReadToEndAsync()
        $peak = 0L
        while (-not $p.WaitForExit(5)) {
            $p.Refresh()
            try { $peak = [Math]::Max($peak, $p.PeakWorkingSet64) } catch [InvalidOperationException] { }
            if ($timer.Elapsed.TotalMinutes -gt 3) { throw '子进程超时' }
        }
        $timer.Stop()
        if ($p.ExitCode -ne 0) { throw "命令失败：$($Arguments -join ' ')；$($stderr.Result)" }
        [pscustomobject]@{ text=$stdout.Result; elapsed_ms=$timer.ElapsedMilliseconds; observed_peak_mib=[math]::Round($peak/1MB,2) }
    } finally { if (-not $p.HasExited) { $p.Kill($true); $p.WaitForExit() }; $p.Dispose() }
}

try {
    $tails = @{}
    foreach ($lang in @('rs','ts','py')) {
        $lines = foreach ($i in 1..99) {
            $name = 'v{0:D3}' -f $i
            switch ($lang) {
                rs { "pub fn $name() -> u32 { $i }" }
                ts { "export function $name() { return $i; }" }
                py { "def ${name}(): return $i" }
            }
        }
        $tails[$lang] = ($lines -join "`n") + "`n"
    }
    $cases = [Collections.Generic.List[object]]::new()
    foreach ($i in 0..124) {
        $g = 'g{0:D3}' -f $i
        switch ($i % 3) {
            0 { $lang='rs'; $paths=@('src/lib.rs','src/writer.rs','src/writer/model.rs','src/idle.rs'); $headers=@('pub mod writer;','pub mod model;','pub const ORIGIN:u32=1;','pub const IDLE:u32=0;') }
            1 { $lang='ts'; $paths=@('entry.ts','writer.ts','model.ts','idle.ts'); $headers=@("import './writer.ts';", "export { v001 as forwarded } from './model.ts';",'export const origin = 1;', 'export const idle = 0;') }
            2 { $lang='py'; $paths=@('__init__.py','writer.py','model.py','idle.py'); $headers=@('from .writer import v001','from .model import v001','origin = 1','idle = 0') }
        }
        for ($j=0;$j -lt $paths.Count;$j++) {
            $file=Join-Path $fixture "$g/$($paths[$j])"
            [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($file)) | Out-Null
            [IO.File]::WriteAllText($file, $headers[$j]+"`n"+$tails[$lang],$encoding)
        }
        if ($i -lt 30) { $cases.Add(@{src="$g/$($paths[0])"; dst="$g/$($paths[1])"}) }
    }
    Invoke-Sample @('--root','.','init') | Out-Null
    $full = Invoke-Sample @('index','--json'); $f=$full.text | ConvertFrom-Json
    if ($f.parsed -ne 500 -or $f.relations -ne 250 -or $f.uncovered -ne 0 -or $f.unresolved -ne 0) { throw "全量校验失败：$($full.text)" }
    $same = Invoke-Sample @('index','--json'); $s=$same.text | ConvertFrom-Json
    if ($s.parsed -ne 0 -or $s.unchanged -ne 500 -or $s.relinked -ne 0) { throw '未变文件被重复解析或引用被重复解析到目标' }
    $matched=0
    foreach ($pair in $cases) {
        $out=(Invoke-Sample @('explain',$pair.src,'--json')).text | ConvertFrom-Json
        $inc=(Invoke-Sample @('impact',$pair.dst,'--json')).text | ConvertFrom-Json
        if (@($out.relations).Count -eq 1 -and @($inc.relations).Count -eq 1 -and $out.relations[0].target -ceq $pair.dst -and $inc.relations[0].source -ceq $pair.src -and $out.relations[0].line -eq 1 -and $inc.relations[0].line -eq 1) { $matched++ }
    }
    if ($matched -ne $cases.Count) { throw "预设的双向证据仅命中 $matched/$($cases.Count)" }
    [IO.File]::WriteAllText((Join-Path $fixture 'g001/entry.ts'),"export const origin = 2;`n"+$tails.ts,$encoding)
    $updated=Invoke-Sample @('index','--json'); $u=$updated.text | ConvertFrom-Json
    if ($u.parsed -ne 1 -or $u.unchanged -ne 499 -or $u.relations -ne 249) { throw "增量校验失败：$($updated.text)" }
    $removed=(Invoke-Sample @('impact','g001/writer.ts','--json')).text | ConvertFrom-Json
    if (@($removed.relations).Count -ne 0) { throw '旧的被引用关系仍存在' }
    [pscustomobject]@{
        os=[Runtime.InteropServices.RuntimeInformation]::OSDescription; arch=[Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString(); cpus=[Environment]::ProcessorCount
        files=$f.seen; lines=50000; binary_mib=[math]::Round((Get-Item $exe).Length/1MB,2)
        full_ms=$full.elapsed_ms; full_observed_peak_mib=$full.observed_peak_mib; relations=$f.relations; unresolved=$f.unresolved
        baseline_pairs=$cases.Count; pairs_checked_both_directions=$matched
        no_change_ms=$same.elapsed_ms; no_change_parsed=$s.parsed; one_change_ms=$updated.elapsed_ms; one_change_parsed=$u.parsed; one_change_unchanged=$u.unchanged
    } | ConvertTo-Json -Compress
} finally {
    # 路径来自本轮新分配的唯一子目录，不能指向调用者的工程根。
    if ([IO.Path]::GetDirectoryName($fixture) -eq $parent -and (Test-Path -LiteralPath $fixture)) { Remove-Item -LiteralPath $fixture -Recurse -Force }
}
