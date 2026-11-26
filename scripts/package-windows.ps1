#requires -Version 5.1
<#
.SYNOPSIS
把显式指定的 release\loom.exe、许可证和中文快速入门打成离线 ZIP。
.EXAMPLE
.\scripts\package-windows.ps1 -Executable .\target\release\loom.exe
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)][Alias('ReleaseExe')][string]$Executable,
    [Alias('OutputDir', 'OutDir')][string]$OutputDirectory = (Join-Path $PSScriptRoot '..\target\dist')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
Add-Type -AssemblyName System.IO.Compression

function Get-RegularFile([string]$Path) {
    $file = Get-Item -LiteralPath $Path -Force
    if ($file -isnot [IO.FileInfo] -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "只接受普通输入文件：$Path"
    }
    return $file
}

# 同卷改名，不使用 Force 或替换；重跑仅复用逐字节哈希相同的成品。
function Publish-NewFile([string]$Staged, [string]$Final) {
    try { [IO.File]::Move($Staged, $Final) }
    catch [IO.IOException] {
        if (-not [IO.File]::Exists($Final)) { throw }
        $null = Get-RegularFile $Final
        if ((Get-FileHash -LiteralPath $Final -Algorithm SHA256).Hash -ne
            (Get-FileHash -LiteralPath $Staged -Algorithm SHA256).Hash) {
            throw "已有同名文件但内容不同，未覆盖：$Final"
        }
    }
}

$exe = Get-RegularFile $Executable
if ($exe.Name -ine 'loom.exe' -or $exe.Directory.Name -ine 'release') {
    throw '请显式传入可信优化构建的 release\loom.exe；不打包 debug、PATH 命令或其他文件。'
}
$repo = Split-Path -Parent $PSScriptRoot
# 顺序、条目名和 DOS 时间固定；不传递本机路径、权限或源文件的时间戳。
$files = [ordered]@{
    'loom.exe'             = $exe
    'LICENSE'              = Get-RegularFile (Join-Path $repo 'LICENSE')
    'WINDOWS-QUICKSTART.md' = Get-RegularFile (Join-Path $repo 'docs\WINDOWS-QUICKSTART.md')
}
$stamp = [DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
$out = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
$null = [IO.Directory]::CreateDirectory($out)
$prefix = Join-Path $out ('.loom-' + [Guid]::NewGuid().ToString('N'))
$zipTemp = "$prefix.zip.tmp"
$sumTemp = "$prefix.sha256.tmp"
$owned = [Collections.Generic.List[string]]::new()
try {
    $stream = [IO.File]::Open($zipTemp, 'CreateNew', 'Write', 'None')
    $owned.Add($zipTemp)
    try {
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($name in $files.Keys) {
                $source = $files[$name].OpenRead()
                try {
                    if ($name -eq 'loom.exe') {
                        if ($source.ReadByte() -ne 0x4d -or $source.ReadByte() -ne 0x5a) { throw '不是 Windows 可执行文件。' }
                        $source.Position = 0
                    }
                    $entry = $zip.CreateEntry($name, [IO.Compression.CompressionLevel]::Optimal)
                    $entry.LastWriteTime = $stamp
                    $dest = $entry.Open()
                    try { $source.CopyTo($dest) } finally { $dest.Dispose() }
                } finally { $source.Dispose() }
            }
        } finally { $zip.Dispose() }
    } finally { $stream.Dispose() }

    $digest = (Get-FileHash -LiteralPath $zipTemp -Algorithm SHA256).Hash.ToLowerInvariant()
    $name = 'loom-windows-' + $digest.Substring(0, 16) + '.zip'
    $package = Join-Path $out $name
    $checksum = "$package.sha256"
    $bytes = [Text.Encoding]::ASCII.GetBytes("$digest  $name`n")
    $stream = [IO.File]::Open($sumTemp, 'CreateNew', 'Write', 'None')
    $owned.Add($sumTemp)
    try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }

    Publish-NewFile $zipTemp $package
    Publish-NewFile $sumTemp $checksum
    [pscustomobject]@{ Package = $package; ChecksumFile = $checksum; SHA256 = $digest }
} finally {
    # 不清空输出目录；即使出错，也只删除本次成功 CreateNew 的两个暂存文件。
    foreach ($file in $owned) { if ([IO.File]::Exists($file)) { [IO.File]::Delete($file) } }
}
