# Windows 离线快速入门

目标：从拿到本地 ZIP 开始，五分钟内确认项目身份、写下并找回第一条记忆。只需 Windows 自带的 PowerShell 5.1（或 PowerShell 7）和与你机器架构匹配的 Loom 包。不用管理员权限，不安装 Rust/Cargo，不改 PATH，不需要模型、数据库服务、网络或旧记忆服务。

## 1. 核对包，再解压

把可信来源的 `loom-windows-….zip` 和同名 `.zip.sha256` 放进一个空文件夹。在该文件夹打开 PowerShell，下面三段命令用**同一个窗口**执行。不要运行来源不明的可执行文件。

```powershell
$zips = @(Get-ChildItem -LiteralPath . -File -Filter 'loom-windows-*.zip')
if ($zips.Count -ne 1) { throw '此文件夹应只有一个 Loom ZIP。' }
$zip = $zips[0]
$actual = (Get-FileHash -LiteralPath $zip.FullName -Algorithm SHA256).Hash
$expected = (Get-Content -LiteralPath ($zip.FullName + '.sha256') -Raw -Encoding ASCII -ErrorAction Stop).TrimEnd()
if ($expected -ine "$actual  $($zip.Name)") { throw 'SHA-256 或文件名不符，请停止并重新取包。' }
$app = Join-Path $env:LOCALAPPDATA $zip.BaseName
if (Test-Path -LiteralPath $app) { throw '解压位置已存在；请另选空目录，不要覆盖。' }
Expand-Archive -LiteralPath $zip.FullName -DestinationPath $app -ErrorAction Stop
$exe = Join-Path $app 'loom.exe'
& $exe --help
```

解压目录应只有 `loom.exe`、`LICENSE` 和本说明。哈希能查出传输错误；**同渠道的旁文件不是代码签名**。有独立可信渠道提供的 SHA-256 时，应在启动前与 `$actual` 比较。

若 `--help` 都不能启动，例如系统提示缺少 `VCRUNTIME140.dll`，请停止并向提供方索取自包含的对应架构版本。不要从第三方网站下载单个 DLL，也不要关闭系统防护。

## 2. 建一个空项目，写入并找回

```powershell
$root = Join-Path $env:LOCALAPPDATA 'Loom 入门项目'
New-Item -Path $root -ItemType Directory -ErrorAction Stop | Out-Null
& $exe --root $root init
& $exe --root $root stats
```

`init` 和 `stats` 应显示**同一标识、同一完整根目录**，新库应为 `合计 0`。Windows 可能在路径前显示 `\\?\`。若根或标识不符，先停下核对，不要认领或迁移旧库。换用自己的项目时，把 `$root` 设为它的绝对路径，跳过 `New-Item`；不要把解压目录当项目目录。

```powershell
& $exe --root $root remember GOTCHA '离线首记：先核对身份' '同名项目也可能是不同的库，写入前必须核对标识和根目录。'
if ($LASTEXITCODE -ne 0) { throw '写入失败；若提示没有 remember，请换用已接通该 CLI 的优化包。' }
& $exe --root $root recall '离线首记'
```

看到 `GOTCHA 离线首记：先核对身份` 才算找回。换个窗口或重启后仍可使用同一程序和根；数据在项目的 `.loom` 下，不在 ZIP 里。**不要删除 `.loom` 来“重置安装”。**

## 3. 连接支持 MCP 的客户端（stdio）

在客户端的设置中新增 stdio 服务：可执行文件选 `$exe` 的实际绝对路径，参数只有 `serve`。不要填 URL、不要只写 `loom`，也不要把整条命令当成一个路径。客户端在项目目录里启动它，它从那里往上找最近的绑定；客户端的启动目录不是项目目录时，把参数写成 `--root`、`$root` 的实际绝对路径、`serve`，并且只放在这个项目自己的配置里。

支持 `mcpServers` 格式的客户端，可以运行下面一行生成带实际路径、正确转义的 JSON；它**只显示，不写配置**。只把 `loom` 条目并入已有设置，不要替换整份文件。

```powershell
@{ mcpServers = @{ loom = @{ command = $exe; args = @('serve') } } } | ConvertTo-Json -Depth 5
```

由客户端启动进程；手动运行 `serve` 后没有提示符是因为它在等标准输入。连接后，已绑定的项目里工具列表中应有 `remember`、`recall`、`context` 等 17 个工具。项目还没绑定时只有 `resume` 一个，它会说明怎么接入。工具列表里没有 Loom 时，先看客户端的 MCP 设置：有的客户端对项目级配置里的服务要经人确认信任才启动。连接失败时，先用同一个 `$exe --root` 路径组合运行 `stats` 核对身份，再查客户端日志。

**让助手替你做后面的事。** 运行 `& $exe guide agents`，把输出的那段提示词放进客户端的全局指令文件（或项目根的 `AGENTS.md`）。之后在别的项目里，助手会照 `& $exe guide` 的步骤自己绑定项目、建索引；搬库和启用模型它会先问你。库想放到别的盘：`& $exe config set storage.data_dir D:\LoomData` 设默认目录，或 `& $exe db move D:\LoomData` 搬已有的库。

## 4. 用 Loom 替代 memos

Loom 的日常运行不依赖 memos。先在每个选定的项目根绑定、运行一次 `index`，再验证 `context` 返回的项目标识和根目录；不要把 memos 的 cube 名当作已核对的项目身份。

```powershell
& $exe --root $root index
& $exe --root $root context 'src/entry.ts' --mode impact
```

把 `src/entry.ts` 换成项目里的受管文件。若在用户级配置里给多个项目共用 Loom，参数用 `serve`，由宿主传入的项目工作目录找到最近的绑定；**不能在全局条目中写死一个项目的 `--root`**。宿主不能提供正确工作目录时，改为各项目分别配置明确根目录。没有绑定的项目里服务只提供 `resume`、不建库，这是预期行为。

在 ZCode 的“设置 → MCP”启用 Loom，核对新会话能看到 `context`，再将 `oh-memos` 设为禁用（配置字段 `enabled: false`），保留其安装和原始数据。已有会话可能仍持有旧进程和旧工具列表；重新连接服务或新开会话后核对。不要为释放内存直接终止其他项目正在使用的进程。

若已有 memos 导出的 wiki 页，可以逐项目导入；导出本身仍由旧服务执行，不是 Loom 的运行依赖。

```powershell
& $exe --root $root import 'D:\memory-wiki\my-project'
# 核对预演的页数、类型、重复、跳过原因及目标项目后才执行：
& $exe --root $root import 'D:\memory-wiki\my-project' --apply
```

经验类型沿用 `DECISION`、`BUGFIX`、`GOTCHA`、`ERROR_PATTERN`、`CONFIG`、`FEATURE`、`MILESTONE`、`PROGRESS`、`CODE_PATTERN`、`SYNTHESIS`。旧文本不能自动变成已确认意图、已复核职责或未完成任务；分别用 `intent`、`note`、`canvas` 表达，确认权仍在人。仅 `--apply` 才写入；导入后查 `stats`、`recall` 并抽查正文，再停用旧入口。原库和原导出不删除。

## 维护者：从已有优化构建复验

下列命令在源码工作树根目录执行，终端用户不需要这些脚本。打包脚本**不会构建或寻找程序**；请明确传入可信的 `release\loom.exe`。它原样打包该文件：若 MSVC 构建仍依赖额外的 VC 运行库，需由构建方静态链接 CRT 并在净机复验，不要从开发机复制 DLL。

三个输入文件字节不变时，用同一版本的打包运行时可复现 ZIP 和 SHA-256；PowerShell 5.1 和 7 的压缩实现可能产生不同 ZIP。默认输出在 `target\dist`，不同内容使用不同包名，已有文件不被覆盖。

在已经加载 MSVC x64 开发环境的 PowerShell 中，为目标程序单独选择静态运行库，不给构建时的宿主工具设置此选项。下面保留并恢复此进程原来的设置：

```powershell
$oldFlags = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS
try {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --release --target x86_64-pc-windows-msvc --locked --offline -p loom-cli
    if ($LASTEXITCODE -ne 0) { throw '构建失败，不要打包旧程序。' }
} finally { $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = $oldFlags }
$p = .\scripts\package-windows.ps1 -Executable .\target\x86_64-pc-windows-msvc\release\loom.exe
$p | Format-List
.\scripts\verify-first-run.ps1 -Package $p.Package
```

验收器默认核对旁文件（或传 `-ExpectedSha256` 指定可信哈希），在唯一临时目录解压，从另一个空项目验证 CLI 和真正的 MCP 握手、工具列表，再清理。默认只给子进程空 PATH 和临时用户目录；需要对比宿主环境时可传 `-IsolateEnvironment:$false`。总时限默认 300 秒，失败或超时不算通过；输出记录系统、PowerShell、哈希及耗时。旧构建即使通过 MCP，也不能代替 CLI 写入与找回。

**验收边界：**开发机的干净目录不是全新 Windows 虚机；子进程环境隔离也不是网络沙箱或代码签名。净机离线、其他 Windows 版本或架构、具体客户端接入，都要在对应环境另行实测并记录，不能从脚本通过推定。
