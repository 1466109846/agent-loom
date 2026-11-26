# 先离线构建 loom-cli，再传入该二进制。只使用新建临时项目和子进程 stdio，不监听端口。
param([Parameter(Mandatory)][string]$Loom)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $Loom).Path
$root = (New-Item -ItemType Directory -Path ([IO.Path]::Combine([IO.Path]::GetTempPath(), "loom-fold-$([guid]::NewGuid())"))).FullName
$sequence = 0
$utf8 = [Text.UTF8Encoding]::new($false)

function Send-Message($message) {
    $process.StandardInput.WriteLine(($message | ConvertTo-Json -Compress -Depth 10))
    $process.StandardInput.Flush()
}
function Invoke-Request($method, $params) {
    $script:sequence++
    Send-Message @{jsonrpc='2.0'; id=$script:sequence; method=$method; params=$params}
    $line = $process.StandardOutput.ReadLineAsync()
    if (-not $line.Wait(10000) -or -not $line.Result) { throw "MCP 未响应：$method" }
    $reply = $line.Result | ConvertFrom-Json
    if ($reply.id -ne $script:sequence -or $reply.error) { throw $line.Result }
    $reply.result
}
function Invoke-Tool($name, $arguments) {
    $r = Invoke-Request 'tools/call' @{name=$name; arguments=$arguments}
    if ($r.isError -or $r.content[0].type -ne 'text') { throw ($r | ConvertTo-Json -Depth 5) }
    $r.content[0].text
}
function Save-Example($title, $body, $importance) {
    $saved = Invoke-Tool 'remember' @{type='DECISION'; title=$title; body=$body; importance=$importance; force=$true}
    if ($saved -notmatch '^已保存 id=([0-9A-HJKMNP-TV-Z]{26})') { throw $saved }
    $Matches[1]
}

try {
    & $exe --root $root init | Out-Null
    if ($LASTEXITCODE -ne 0) { throw '临时项目初始化失败' }
    $start = [Diagnostics.ProcessStartInfo]::new($exe, 'serve')
    $start.WorkingDirectory = $root
    $start.UseShellExecute = $false
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.StandardInputEncoding = $utf8
    $start.StandardOutputEncoding = $utf8
    $process = [Diagnostics.Process]::Start($start)
    $null = Invoke-Request 'initialize' @{protocolVersion='2025-06-18'; capabilities=@{}; clientInfo=@{name='fold-test'; version='1'}}
    Send-Message @{jsonrpc='2.0'; method='notifications/initialized'}
    $title = '核对当前项目写入范围'
    $body = '只有核实当前项目后才允许写入，并保留原始记录。' * 30
    $main = Save-Example $title $body 5
    $old = Save-Example "$title。" $body 4
    $other = Save-Example '核对只读备份' '只读备份与写入授权是独立的结论。' 1

    $original = Invoke-Tool 'manage' @{action='get'; ids=@($old)}
    if (-not ($original.Contains("$title。") -and $original.Contains($old) -and $original.Contains($body))) { throw $original }
    $preview = Invoke-Tool 'recall' @{query='核'; top_k=2}
    $cli = (& $exe --root $root recall -k 2 核) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw 'CLI 检索失败' }
    foreach ($text in @($cli, $preview)) {
        if (-not ($text.Contains("id=$main") -and $text.Contains($old) -and $text.Contains("id=$other") -and $text.Contains('折叠'))) {
            throw "预览未同时标明代表、被折叠编号与独立命中：`n$text"
        }
    }
    if ($utf8.GetByteCount($preview) -gt 1500 -or -not $preview.Contains('已省略部分内容')) { throw $preview }
    Write-Output '通过：真实 CLI 和 MCP stdio 预览、总预算、top_k 与原编号全文。'
} finally {
    if ($process) {
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(5000)) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
    Remove-Item -LiteralPath $root -Recurse -Force
}
