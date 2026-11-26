#requires -Version 5.1
<#
.SYNOPSIS
从可信 ZIP 验证首次离线使用；不调用 Cargo，不读取开发库，不修改宿主配置。
.EXAMPLE
.\scripts\verify-first-run.ps1 -Package $p.Package
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)][Alias('PackagePath')][string]$Package,
    [ValidatePattern('^[a-fA-F0-9]{64}$')][string]$ExpectedSha256,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 300,
    [switch]$IsolateEnvironment = $true
)
$clock = [Diagnostics.Stopwatch]::StartNew()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
Add-Type -AssemblyName System.IO.Compression
$limitMs = [long]$TimeoutSeconds * 1000
$utf8 = [Text.UTF8Encoding]::new($false)

function Get-RemainingMs([long]$Deadline = $limitMs) {
    $left = $Deadline - $clock.ElapsedMilliseconds
    if ($left -le 0) { throw '超时：已达到步骤或整体验收时限。' }
    return [int]$left
}

function Expand-CheckedPackage([string]$Folder) {
    $file = Get-Item -LiteralPath $Package
    if ($file -isnot [IO.FileInfo] -or $file.Length -gt 256MB) { throw '需要不超过 256 MiB 的本地 ZIP 文件。' }
    $expected = $ExpectedSha256
    if (-not $expected) {
        $side = Get-Item -LiteralPath ($file.FullName + '.sha256')
        if ($side.Length -gt 4KB) { throw 'SHA-256 旁文件过大。' }
        $line = [IO.File]::ReadAllText($side.FullName)
        if ($line -notmatch '\A([a-fA-F0-9]{64})  ([^\r\n]+)(?:\r?\n)?\z' -or $Matches[2] -cne $file.Name) {
            throw 'SHA-256 旁文件格式或包名不符。'
        }
        $expected = $Matches[1]
    }
    # 同一只读句柄完成核验和展开。只按固定名称创建普通文件，不使用归档路径作为目标。
    $stream = $file.OpenRead()
    try {
        $actual = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash
        if ($actual -ine $expected) { throw 'ZIP 的 SHA-256 不匹配，未运行程序。' }
        $stream.Position = 0
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            $names = @('loom.exe', 'LICENSE', 'WINDOWS-QUICKSTART.md')
            if ($zip.Entries.Count -ne $names.Count) { throw '包必须正好包含三个白名单文件。' }
            $total = 0L
            $buffer = [byte[]]::new(65536)
            foreach ($name in $names) {
                $entry = $zip.GetEntry($name)
                if ($null -eq $entry -or $entry.Length -le 0) { throw "缺少或为空：$name" }
                $total += $entry.Length
                if ($total -gt 256MB) { throw '包内文件超出 256 MiB 验收上限。' }
                $source = $entry.Open()
                try {
                    $dest = [IO.File]::Open((Join-Path $Folder $name), 'CreateNew', 'Write', 'None')
                    try {
                        while (($n = $source.Read($buffer, 0, $buffer.Length)) -gt 0) {
                            $null = Get-RemainingMs
                            if ($dest.Length + $n -gt $entry.Length) { throw "解压长度异常：$name" }
                            $dest.Write($buffer, 0, $n)
                        }
                        if ($dest.Length -ne $entry.Length) { throw "解压不完整：$name" }
                    } finally { $dest.Dispose() }
                } finally { $source.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $stream.Dispose() }
    Write-Host "包已核对：SHA-256 $actual；仅三个白名单文件。"
}

function Start-Loom([string[]]$Argv) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $exe
    $info.WorkingDirectory = $install  # 刻意与项目根不同，不从启动目录找库。
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $utf8
    $info.StandardErrorEncoding = $utf8
    if ($info.PSObject.Properties['StandardInputEncoding']) { $info.StandardInputEncoding = $utf8 }
    # .NET Framework 没有 ArgumentList；按 Windows argv 规则引用，不经命令解释器。
    $info.Arguments = ($Argv | ForEach-Object { '"' + (($_ -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"' }) -join ' '
    if ($IsolateEnvironment) {
        $vars = $info.EnvironmentVariables
        $vars.Clear()
        $vars['SystemRoot'] = $env:SystemRoot
        $vars['WINDIR'] = $env:SystemRoot
        $vars['PATH'] = ''
        $vars['HOME'] = $vars['USERPROFILE'] = $testHome
        $vars['APPDATA'] = $vars['LOCALAPPDATA'] = $testHome
        $vars['TEMP'] = $vars['TMP'] = $testTemp
    }
    return [Diagnostics.Process]::Start($info)
}

function Close-Loom($Process) {
    try {
        if (-not $Process.HasExited) {
            try { $Process.Kill() } catch [InvalidOperationException] { if (-not $Process.HasExited) { throw } }
            if (-not $Process.WaitForExit(2000)) { throw '限时终止本次 Loom 子进程失败。' }
        }
    } finally { $Process.Dispose() }
}

function Wait-IO($Task, [long]$Deadline) {
    if (-not $Task.Wait((Get-RemainingMs $Deadline))) { throw '超时：等待子进程管道。' }
}

function Wait-LoomExit($Process, $Stderr, [long]$Deadline) {
    if (-not $Process.WaitForExit((Get-RemainingMs $Deadline))) { throw '超时：等待 Loom 退出。' }
    Wait-IO $Stderr $Deadline
    if ($Process.ExitCode -ne 0) { throw "loom 退出码 $($Process.ExitCode)：$($Stderr.Result.Trim())" }
}

function Invoke-Loom([string[]]$Argv) {
    $null = Get-RemainingMs
    $deadline = [Math]::Min($limitMs, $clock.ElapsedMilliseconds + 30000L)
    $p = Start-Loom $Argv
    try {
        $stdout = $p.StandardOutput.ReadToEndAsync()
        $stderr = $p.StandardError.ReadToEndAsync()
        $p.StandardInput.Close()
        Wait-LoomExit $p $stderr $deadline
        Wait-IO $stdout $deadline
        return $stdout.Result
    } finally { Close-Loom $p }
}

function Send-Frame($Process, [hashtable]$Frame, [long]$Deadline) {
    $bytes = $utf8.GetBytes((ConvertTo-Json -InputObject $Frame -Compress -Depth 6) + "`n")
    $pipe = $Process.StandardInput.BaseStream
    Wait-IO ($pipe.WriteAsync($bytes, 0, $bytes.Length)) $Deadline
    Wait-IO ($pipe.FlushAsync()) $Deadline
}

function Read-Result($Process, [string]$Id, [long]$Deadline) {
    while ($true) {
        $read = $Process.StandardOutput.ReadLineAsync()
        Wait-IO $read $Deadline
        if ($null -eq $read.Result) { throw "MCP 在响应 $Id 前关闭了标准输出。" }
        $frame = ConvertFrom-Json -InputObject $read.Result -ErrorAction Stop
        if ($frame.jsonrpc -cne '2.0') { throw '不是 JSON-RPC 2.0 响应。' }
        $fields = $frame.PSObject.Properties
        if (-not $fields['id'] -and $fields['method']) { continue }  # 合法通知不占用请求 ID。
        if (-not $fields['id'] -or $frame.id -cne $Id) { throw "MCP 响应 ID 不符，应为 $Id。" }
        if ($fields['error']) { throw "MCP 返回错误：$(ConvertTo-Json $frame.error -Compress)" }
        if (-not $fields['result']) { throw "MCP 响应 $Id 没有 result。" }
        return $frame.result
    }
}

function Test-Mcp {
    $null = Get-RemainingMs
    $deadline = [Math]::Min($limitMs, $clock.ElapsedMilliseconds + 30000L)
    $p = Start-Loom @('--root', $project, 'serve')
    try {
        $stderr = $p.StandardError.ReadToEndAsync()
        $version = '2025-06-18'
        Send-Frame $p @{ jsonrpc='2.0'; id='init'; method='initialize'; params=@{ protocolVersion=$version; capabilities=@{}; clientInfo=@{ name='loom-first-run'; version='1' } } } $deadline
        $hello = Read-Result $p 'init' $deadline
        if ($hello.protocolVersion -cne $version -or $hello.serverInfo.name -cne 'loom' -or -not $hello.capabilities.PSObject.Properties['tools']) {
            throw 'MCP 握手的版本、服务身份或工具能力不符。'
        }
        Send-Frame $p @{ jsonrpc='2.0'; method='notifications/initialized' } $deadline
        Send-Frame $p @{ jsonrpc='2.0'; id='list'; method='tools/list'; params=@{} } $deadline
        $list = Read-Result $p 'list' $deadline
        $names = @($list.tools | ForEach-Object { $_.name })
        if ($names -cnotcontains 'remember' -or $names -cnotcontains 'recall') { throw 'MCP 工具列表缺少 remember 或 recall。' }
        $tail = $p.StandardOutput.ReadToEndAsync()
        $p.StandardInput.Close()  # 客户端 EOF 后，服务也必须按时退出。
        Wait-LoomExit $p $stderr $deadline
        Wait-IO $tail $deadline
        Write-Host "MCP stdio 通过：loom / $version，tools/list 返回 $($names.Count) 个工具，含 remember、recall。"
    } finally { Close-Loom $p }
}

function Get-NormalPath([string]$Path) {
    # Rust canonicalize 在 Windows 可返回扩展长度路径；比较实际根，不只比较同名目录。
    if ($Path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) { $Path = '\\' + $Path.Substring(8) }
    elseif ($Path.StartsWith('\\?\')) { $Path = $Path.Substring(4) }
    return [IO.Path]::GetFullPath($Path)
}

function Assert-Identity([string]$Text, [string]$Marker) {
    if ($Text -notmatch '(?m)^项目：[^\r\n]+ · 标识=([^\s]+) · 根=([^\r\n]+)\r?$') { throw '输出缺少项目身份行。' }
    if ($Matches[1] -cne $Marker -or (Get-NormalPath $Matches[2]) -ine (Get-NormalPath $project)) { throw '项目标识或完整根目录不符。' }
}

function Remove-OwnedTree([IO.DirectoryInfo]$Dir) {
    # 入口仅接收下方排他新建的目录。不跟随目录联接或符号链接。
    if ($Dir.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "拒绝清理重解析点：$($Dir.FullName)" }
    foreach ($item in $Dir.GetFileSystemInfos()) {
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "拒绝清理重解析点：$($item.FullName)" }
        if ($item -is [IO.DirectoryInfo]) { Remove-OwnedTree $item } else { $item.Delete() }
    }
    $Dir.Delete()
}

$ownedDir = $null
try {
    Write-Host ("环境：{0}，PowerShell {1}（{2} 位进程），子进程环境隔离={3}。" -f [Environment]::OSVersion.VersionString, $PSVersionTable.PSVersion, ([IntPtr]::Size * 8), [bool]$IsolateEnvironment)
    $unique = Join-Path ([IO.Path]::GetTempPath()) ('loom-m4-' + [Guid]::NewGuid().ToString('N'))
    $ownedDir = New-Item -Path $unique -ItemType Directory -ErrorAction Stop  # 不用 -Force；若碰撞绝不接管。
    $install = [IO.Directory]::CreateDirectory((Join-Path $unique '解压 程序')).FullName
    $project = [IO.Directory]::CreateDirectory((Join-Path $unique '首次 项目')).FullName
    $testHome = [IO.Directory]::CreateDirectory((Join-Path $unique 'home')).FullName
    $testTemp = [IO.Directory]::CreateDirectory((Join-Path $unique 'tmp')).FullName
    Expand-CheckedPackage $install
    $exe = Join-Path $install 'loom.exe'
    $help = Invoke-Loom @('--help')
    if (-not $help.Contains('--root') -or $help -notmatch '(?m)^\s+init\s') { throw '--help 未给出项目入口。' }
    Write-Host '--help 通过。'
    $init = Invoke-Loom @('--root', $project, 'init')
    $marker = [IO.File]::ReadAllText((Join-Path $project '.loom\project.id')).Trim()
    Assert-Identity $init $marker
    $stats = Invoke-Loom @('--root', $project, 'stats')
    Assert-Identity $stats $marker
    if ($stats -notmatch '(?m)^合计 0\r?$') { throw '首次 stats 不是空库。' }
    Write-Host "init / stats 通过：标识=$marker，根=$project，合计 0。"

    $word = 'loomcheck' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $title = "离线接入 $word"
    $issues = [Collections.Generic.List[string]]::new()
    try {
        $null = Invoke-Loom @('--root', $project, 'remember', 'GOTCHA', $title, '首次写入前必须核对根目录，避免把经验存到同名项目。')
        Write-Host 'CLI remember GOTCHA 已成功退出。'
    } catch { $issues.Add("CLI remember：$($_.Exception.Message)"); Write-Warning $issues[$issues.Count - 1] }
    try {
        $hits = Invoke-Loom @('--root', $project, 'recall', $word)
        if (-not $hits.Contains("GOTCHA $title")) { throw "未找回本次唯一标题：$title" }
        Write-Host "CLI recall 通过：GOTCHA $title。"
    } catch { $issues.Add("CLI recall：$($_.Exception.Message)"); Write-Warning $issues[$issues.Count - 1] }
    # 即使旧包没有 CLI remember，也真实测试协议；不拿 MCP 写入或预置数据代替 CLI。
    Test-Mcp
    if ($issues.Count -gt 0) { throw ($issues -join [Environment]::NewLine) }
} finally {
    try { if ($null -ne $ownedDir) { Remove-OwnedTree $ownedDir } }
    finally { Write-Host ('总耗时 {0:N3} 秒（含核对、解压和清理；上限 {1} 秒）。' -f $clock.Elapsed.TotalSeconds, $TimeoutSeconds) }
}
$null = Get-RemainingMs
Write-Host '首次运行验收通过（本机干净目录，不代表净机或签名验收）。'
