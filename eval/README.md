# Loom 固定本地评测

历史 U-1/U-2/U-3 已完成固定质量、同场资源与任务成本对照，**综合比较 PASS：198 个检查项、0 个失败**。38 个问题为 36 通过、2 项旧失败，不等于全题通过。源码样本仅作离线输入，不执行样本程序，不联网、不调用模型。

## 公开副本与离线比较

本目录发布的是脱敏的历史结果副本，不是用当前公开源码新做的测量。`redacted://...` 是不可解析的历史位置标识，不是真实样本、安装路径或现存证据地址。报告内的构建/证据/脚本/冻结题定义 SHA、测量数值、字节数、判定及时间均保留历史值；路径字符串被净化后，公开 JSON 的文件摘要与历史原件不同。原回答字节数也不按脱敏后的路径重新计算。

报告中的 `definitions.questions_sha256` 指历史未脱敏题定义，`harness.run_sha256` 指当时 runner；它们不是本目录当前文件的摘要。此次只改题定义中的工作根和样本定位字段，38 题、夹具、样本版本/清单、120 条标签及 thresholds 原样保留。导出把门槛和标签文件的换行转换成了 CRLF；本公开副本恢复 LF，逐文件 SHA 与历史冻结记录精确一致，未改任何标注或门槛。新 runner 会如实记录当前题定义和 runner 的新摘要，不覆盖旧值。旧提交/源码树指纹是历史来源标识，不保证它们存在于无私有历史的公开仓库。

在公开仓库根目录打开 PowerShell，只比较现存 JSON，无需安装 Loom、样本或模型：

```powershell
$E = Join-Path (Get-Location).Path 'eval'
# 同一冻结基线自校验，不是新版本验收
python -I "$E/compare.py" "$E/baseline/rev8.json" "$E/baseline/rev8.json"
# 历史最终 quality + 同场两份资源
python -I "$E/compare.py" "$E/baseline/rev8.json" "$E/results/contract2-quality.json" --baseline-resources "$E/results/contract2-resources-old.json" --candidate-resources "$E/results/contract2-resources-new.json"
# 历史之后的构建；仍仅离线复读历史数据
python -I "$E/compare.py" "$E/baseline/rev8.json" "$E/results/d28be0a8-quality.json" --baseline-resources "$E/results/d28be0a8-resources-old.json" --candidate-resources "$E/results/d28be0a8-resources-new.json"
```

`compare.py` 对比两份报告记录的冻结题摘要，并实际核对原样保留的门槛文件，不要求用当前题文件替换历史指纹。因此以上历史报告继续可离线比较；新旧题定义摘要不一致时仍拒绝混比，没有放宽门槛。

## 新一轮实测

使用 Python 3.10+ 标准库和 `-I` 隔离模式。持久化专用根默认是 `Path.home() / '.cache' / 'loom-eval'`；可用环境变量 `LOOM_EVAL_HOME` 改位置，但必须是末级名为 `loom-eval` 的绝对路径。`--work` 只能是其 `work` 子树里的新目录，拒绝根本身、兄弟前缀、`..`、符号链接和 Windows 联接；不能指向任意磁盘目录。工作根不再固定于开发机，也不放在系统临时目录。

先自行准备可信的两个独立评测构建，以及三个冻结样本；本脚本不下载、不构建、不执行样本。默认样本位于 `LOOM_EVAL_HOME/samples/loom-rust`、`samples/vue-element-admin`、`samples/oh-memos`。`--samples` 也接受样本名到路径的 JSON、该 JSON 文件，或含同名目录的父目录。Git 样本核对 HEAD、全部源码清单及 `.git/info/exclude`，无 Git 的 Rust 导出核对清单；不匹配会拒绝，不能改清单绕过校验。

旧构建对应提交 `99c0498`，历史候选对应 `937a7fd`，源码指纹见下节。公开仓库不包含私有历史或这两个二进制，也没有打包冻结样本。新编译的二进制摘要可能不同，必须保留本轮实际摘要；不能把历史 SHA 填成当前构建结果。拿不到原冻结样本时不能宣称重现这组测量。

以下 PowerShell 在公开仓库根运行；先把已核验构建放在所示 `builds` 下，并准备好上述三个样本目录。`--samples` 可显式改样本位置。每次生成新一轮 UUID，输出与工作目录都不复用：

```powershell
$E = Join-Path (Get-Location).Path 'eval'
$env:LOOM_EVAL_HOME = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'loom-eval'
$H = $env:LOOM_EVAL_HOME
$OLD = Join-Path $H 'builds/baseline/loom.exe'
$NEW = Join-Path $H 'builds/candidate/loom.exe'
foreach ($p in @($OLD, $NEW, "$H/samples/loom-rust", "$H/samples/vue-element-admin", "$H/samples/oh-memos")) {
    if (-not (Test-Path -LiteralPath $p)) { throw '先准备可信构建与固定样本，再进行实测' }
}
$Round = [guid]::NewGuid().ToString('N')
$W = Join-Path $H "work/$Round"
$R = Join-Path $H "results/$Round"
New-Item -ItemType Directory -Path $R -ErrorAction Stop | Out-Null

# 同一 runner、同一当前冻结题定义：先新建完整基线，再跑完整候选
python -I "$E/run.py" --loom $OLD --work "$W/baseline" --out "$R/baseline.json"
if ($LASTEXITCODE -ne 0) { throw '本轮基线执行失败' }
python -I "$E/run.py" --loom $NEW --work "$W/candidate" --out "$R/candidate.json"
if ($LASTEXITCODE -ne 0) { throw '本轮候选执行失败' }
# 同一调度逐轮轮换 NEW/OLD、OLD/NEW，至少五轮
python -I "$E/run.py" --loom $NEW --peer-loom $OLD --work "$W/paired" --out "$R/new-resources.json" --peer-out "$R/old-resources.json" --only resources --rounds 5
if ($LASTEXITCODE -ne 0) { throw '本轮同场资源执行失败' }
python -I "$E/compare.py" "$R/baseline.json" "$R/candidate.json" --baseline-resources "$R/old-resources.json" --candidate-resources "$R/new-resources.json"
```

新实测不能用历史 `baseline/rev8.json` 搭配本轮新报告，应如上用同一版本 runner、compare、题定义、标签和门槛重建完整基线/候选及同场资源。将本轮 JSON 与 `$W/baseline/raw`、`$W/candidate/raw`、`$W/paired/raw` 等工作证据目录一起持久保存，不能只保留摘要。资源工作集采样仍采用 Windows PSAPI；没有把它宣称为已验证的跨系统资源测量。

`--only questions|relations|resources` 可分别运行。`--merge-from` 只保留同一二进制、runner 和冻结输入的其他部分，必须写新结果，不能与 `--peer-loom` 同用；历史摘要与当前 runner 不一致时拒绝拼接。`--relation-self-test` 只验证合成标签判定分支，不冒充真实样本成绩。

保险不做可选：所有被测进程同时指定隔离 cwd 和 `--root`，拒绝 `--db` 覆盖；副本跳过所有 `.loom`，保留真实 `.git`；清掉子进程 `LOOM_*` 环境，再指向必须不存在的评测配置哨兵。动态拒绝 PATH 中的 Loom、Cargo/用户/系统常见安装位置及其链接/硬链接；自定义生产位置用 `LOOM_EVAL_PRODUCTION_EXE` 额外保护。只允许显式指定可信独立评测构建，不读取用户配置，不借用开发库或真实样本旧库，不联网、不调用模型、不执行样本代码。

## 冻结输入与版本

- [questions.json](questions.json)：26 个结构、4 个记忆、6 个任务、2 个预算拒绝问题，共 **38 个**；夹具的字节、索引前后改动、请求及事实/契约条件固定。
- [labels](labels)：三个样本各 40 条，共 **120 条**。每份至少 20 个正例、5 个 none，带源码行号与人工依据。
- [thresholds.json](thresholds.json)：不安全否定为 0；reported exact precision ≥98%、likely ≥95%；逐条不丢旧正确项、不添新错误、逐语言召回不退步。耗时中位数增加 >10% 为必须解释的提示，不是零性能回归承诺。
- [旧基线](baseline/rev8.json)：用当时脚本与修正的指标口径重跑的 rev8-v2。exe SHA-256 `c043355f8a5619925aff193c307b74ab749e4decbdf01ac8f5fab676df8b37dd`；源码是 `cc2a954` 加当时未提交的修订 8（后来提交为 `99c0498`），原源码树 `8713ddf2f4346502aef1a0fd77564c96637434a0`。
- 候选 exe SHA-256 `4a16c8cfbc9ecd3036a621abbb0ffe8c72443e3d876a68dc41d2982802e0a3d5`，15,307,776 字节。结构修订仍为 8，报告契约为 2；工程代码后来提交为 `937a7fd`。

问题、门槛、脚本、样本清单、标签、原回答的 SHA-256 均在结果中。空范围 S19 要求 `absent` 与 `empty_scope`；S24 的 Rust 分组 use 有未确定引用，即使 found 仍保留 unresolved_refs。旧版 contract=1 的契约检查为 N/A，不算失败。

## 最终质量结果

**Precision 只问报出来的关系对不对。** 在标签覆盖的来源、行号和目标上，按工具声称的 exact/likely 计数；漏连属于召回损失，不是 precision 错误。标签之外的错边不纳入这个成绩。按标签类别的命中比例另列，不能把 Rust 26/32 写成 exact precision。

| 样本与固定版本 | reported exact | reported likely | none | 正例召回（旧 → 新） | 受管 / 关系 / 未确定 |
|---|---:|---:|---:|---:|---|
| loom-rust `cc2a954` 导出 | 26/26 | N/A | 8/8 | 26/32 → 26/32 | 80 / 82 / 374 |
| vue-element-admin `6858a9a` | 7/7 | 25/25 | 8/8 | 32/32 → 32/32 | 221 / 280 / 99 |
| oh-memos `810627e` | 8/8 | 24/24 | 8/8 | 32/32 → 32/32 | 619 / 1897 / 1799 |

逐条正确项没有丢失，没有新增错误。Rust 未连上的仍是 `rust-021/022/023/025/028/032`，保留在报告，不通过降低召回门槛掩盖。真实标签没有 ambiguous，也没有 Rust likely，记 N/A，相关判定路径只由夹具验证。

固定问题事实结果：旧版 **25 pass / 11 fail / 2 N/A**，候选 **36 pass / 2 fail**；全部 26 条结构契约通过，不安全否定 **5 → 0**。两项旧失败原样保留：

- **M01**：独立 recall 查询 `target.ts`，只锚定在文件上、正文标题不带路径的经验仍返回“没有找到相关记忆”。context 的锚点通道能取到，不意味着独立 recall 改了行为。追踪 [K-3](../docs/plan/BACKLOG.md#k-知识与分享)。
- **S26**：`src/oh_memos/log.py:16` 的 from-package 导入，实际连到 `src/oh_memos/__init__.py`，没有连到要求的 `settings.py`。追踪 [S-27](../docs/plan/BACKLOG.md#s-结构索引)。

旧版其余失败包括 S03/S13/S15/S21/S22 的五次不安全否定，S12 未披露使用方变化，S14 未披露配置变化，S17 没有省略关系计数与覆盖缺口，T03 缺使用方已变提示。这些在历史固定题中修正；事实检查保留在结果中，逐题完整原文现已缺失。

### 六个任务：同一候选版本内比较

| 问题 | 正确依据（旧入口 / context） | 请求数 | 旧入口交付字节 | context 交付字节 |
|---|---:|---:|---:|---:|
| T01 修改写入入口 | 5/5 / 5/5 | 4 → 1 | 1120 | 1043 |
| T02 解释职责 | 5/5 / 5/5 | 4 → 1 | 1115 | 1042 |
| T03 使用方改后未刷新 | 6/6 / 6/6 | 4 → 1 | 1294 | 1217 |
| T04 职责需复核 | 6/6 / 6/6 | 4 → 1 | 1319 | 1324 |
| T05 不采信归档经验 | 4/4 / 4/4 | 4 → 1 | 1047 | 853 |
| T06 提议不充当红线 | 5/5 / 5/5 | 4 → 1 | 1120 | 1043 |

31 项规定依据全部保留，context 均在默认 1500 字节内。T04 字节略增，请求数减少，满足“请求数或交付字节减少”的原门槛。B01/B02 的 CLI/MCP 必要预算拒绝通过。任务成本不含准备时的 init/index/写入；结构题的 `delivery_*` 和 JSON `diagnostic_*` 分开，MCP 协议封装另计，不换算词元。

## 正式同场资源

Windows x64，12 个逻辑处理器，两个 release 逐轮轮换先后，各样本/版本五轮。每轮新副本，不清系统文件缓存。复制和初始化不计入操作时间；查询包括进程启动与 JSON 输出。下表单位 ms，为旧 → 新中位数。

| 样本 | 全量索引 | 无改动刷新 | 改一文件 | explain | impact |
|---|---:|---:|---:|---:|---:|
| loom-rust | 260.92 → 250.74 | 34.97 → 37.94 | 42.37 → 42.10 | 10.08 → 17.32 | 10.86 → 17.29 |
| vue-element-admin | 217.17 → 234.09 | 80.21 → 112.10 | 89.94 → 93.74 | 11.03 → 15.95 | 10.51 → 17.10 |
| oh-memos | 2063.11 → 2129.33 | 170.65 → 185.06 | 193.04 → 187.23 | 10.11 → 16.86 | 9.95 → 16.11 |

| 样本 | 全量峰值 MiB（旧 → 新） | serve 空闲 MiB（旧 → 新） |
|---|---:|---:|
| loom-rust | 11.11 → 11.14 | 9.47 → 10.04 |
| vue-element-admin | 10.05 → 10.30 | 9.45 → 10.30 |
| oh-memos | 19.48 → 19.62 | 9.51 → 10.52 |

峰值取五轮最高，工作集由 Windows PSAPI 5ms 轮询及退出后进程句柄采样记录。每样本/版本另开一个 serve，四次固定调用后空闲至少 30 秒；六个窗口全部完成，新版均低于 100 MiB 空闲目标。无改刷新每轮重解析 0，单文件修改每轮只重解析 1。完整范围、轮数、每轮报告、顺序及 `pair_id` 在两份资源结果中。

查询增加约 5–7ms，来自新增的文件、祖先规则和配置现场核对，接受这项有界开销，不以撤掉安全检查换性能。Vue 无改动刷新原五轮增加 **39.8%**：新版 80.41/113.19/131.67/112.10/92.45ms，旧版 79.54/85.22/80.21/110.84/77.04ms。追加两份预热库上的 31 轮交替复测为 **95.66 → 97.16ms（+1.6%）**，范围 84.22–165.56 / 84.80–139.56ms；未复现稳定的 39.8% 开销，波动根因未定位。原五轮未被覆盖，追加诊断不替代完整资源试验。后续性能工作见 S-26。

## 之后的构建：库位置、助手接入与可选的向量检索

上面各节记的是 U-1 至 U-3 那一版（`4a16c8cf…`）。同日加入库位置、助手接入和可选向量检索之后，用同一套冻结输入对新的 release（`d28be0a8aef5704eaac996ec21461a9263d0c3dd2f38975abeba1cdd54d79abf`，18,254,336 字节）重跑了质量和五轮同场资源，仍与修订 8 的旧基线对照：**198 个检查项、0 个失败**。

- 质量与上一版相同：36 pass / 2 fail（S26、M01），26 条结构契约通过，不安全否定 0；三样本 reported precision 和召回逐条未变；六个任务的 context 仍保留全部 31 项依据，请求 4 → 1。各题的字节数比上一版少 8，是因为这次的工作目录名短了 8 个字符，项目首行里的根路径跟着变短。
- 资源（旧 → 这一版，ms 为五轮中位数）：

| 样本 | 全量索引 | 无改动刷新 | 改一文件 | explain | impact | 全量峰值 MiB | serve 空闲 MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| loom-rust | 282.65 → 268.35 | 36.43 → 41.58 | 43.30 → 44.69 | 10.15 → 19.45 | 10.38 → 22.27 | 11.20 → 12.01 | 9.42 → 11.04 |
| vue-element-admin | 213.25 → 218.90 | 81.45 → 87.75 | 95.01 → 90.42 | 11.25 → 15.59 | 11.97 → 15.90 | 10.18 → 11.14 | 9.42 → 10.98 |
| oh-memos | 2143.66 → 2099.27 | 201.60 → 194.63 | 199.92 → 208.38 | 10.15 → 18.33 | 12.08 → 18.69 | 19.43 → 20.45 | 9.57 → 11.24 |

- 与上一版相比：serve 空闲多 0.7 到 1.0 MiB，全量峰值多 0.8 到 0.9 MiB，每起一次进程多约 1 毫秒（两版在同一份样本上交替各 41 次：explain 15.83 → 16.80 ms，`--version` 5.93 → 7.20 ms）。
- `run.py` 现在给每个被测进程设 `LOOM_CONFIG` 指到一份不存在的文件：评测不读这台机器的用户配置，库位置和模型设置都按默认。向量检索不在这套评测的范围内，它的验收见[验收记录](../docs/ACCEPTANCE.md)最后一节。

## 报告与原回答

| 证据 | 仓库文件 |
|---|---|
| 旧版完整冻结结果 | [baseline/rev8.json](baseline/rev8.json) |
| 候选标签、38 问题、任务成本 | [contract2-quality.json](results/contract2-quality.json) |
| 新/旧同场资源 | [新版](results/contract2-resources-new.json) / [旧版](results/contract2-resources-old.json) |
| 综合比较 198 checks | [contract2-comparison.json](results/contract2-comparison.json) |
| 七样本 9368 次旧字段回归 | [contract2-regression.json](results/contract2-regression.json) |
| 追加 31 轮刷新诊断 | [contract2-refresh-diagnostic.json](results/contract2-refresh-diagnostic.json) |
| 之后的构建：质量与任务成本 | [d28be0a8-quality.json](results/d28be0a8-quality.json) |
| 之后的构建：新/旧同场资源 | [新版](results/d28be0a8-resources-new.json) / [旧版](results/d28be0a8-resources-old.json) |
| 之后的构建：综合比较 198 checks | [d28be0a8-comparison.json](results/d28be0a8-comparison.json) |

逐题完整原回答、七样本旧/新导出、工程日志和协议记录没有随公开副本提供。历史记录说明：原件位于临时目录，2026-11-26 被清空，原因未查明。报告只保留判定结果、不可解析的脱敏位置与原始 SHA；**不能凭摘要恢复逐题原文，也无法再逐字核验历史回答**。重跑会生成一批新的原文及其新摘要，不会恢复或替代历史证据。

下表左列是本次路径脱敏前导出 JSON 原件的文件 SHA-256，右列是公开 JSON 副本的文件 SHA-256。它们与报告内保留的历史构建、runner、题定义、原回答摘要是不同层次的指纹；公开副本的路径已变，不拿历史摘要冒充公开文件摘要。未含路径的两份 comparison 文件未改动，所以两列一致。

| JSON 文件 | 脱敏前导出原件 SHA-256 | 公开副本 SHA-256 |
|---|---|---|
| `baseline/rev8.json` | `162a6a3edd65d5dce30083ce8b99dd37cd8bdab2dd26146c8e56478f911ea6af` | `8a28eaed7392c6eea6a70881b792dfdbd5904ef1c9a26a6d68bef7cd3024c2c7` |
| `results/contract2-comparison.json` | `28907fb6241e92787feb11cbd663d35fb65f51a967002da155c50c816fbce35b` | `28907fb6241e92787feb11cbd663d35fb65f51a967002da155c50c816fbce35b` |
| `results/contract2-quality.json` | `ccec4ae4b822a70cb0e84c04b6e4b82239a4363434d73f4bf78cf7d226991f6d` | `090a306ef60cfaaba5509382a5565b7b558a7f81460d0cf1b8a9e7bd5988a5ed` |
| `results/contract2-refresh-diagnostic.json` | `efadfe8ef0f5621ca869eb82a1b1d126ff7c267dc25c838c88ff625d61692b93` | `ee6588fd6c51fe29d5414572c5b704a9acaea376fd2dcda5e6669faf84657071` |
| `results/contract2-regression.json` | `2a3c17f63080f8b2d9c07883744ea205b7f9f80726f7ec43aa5f82f14de8dc6a` | `a10e57d5e8fe6b4e3c9c38b44bc8d30aa437d77aeab0f006cb01a08e4523c12f` |
| `results/contract2-resources-new.json` | `da934b7de4c41cf853feb1040570e61b31d2d2897dad2b72eec1863ad29083af` | `4772f1f7bfdf38761339741a8c68335972eac7293ce374a490631b2460643cf9` |
| `results/contract2-resources-old.json` | `1ab54e4820da76b2798ff00782de03f0012b34c9d7d113e27603cb47277f030e` | `6cc9af902e395982ac07161f38833f8ef64159562d68ff70e51189f8f68ef168` |
| `results/d28be0a8-comparison.json` | `583c3f329deb09700a956626038f3c2e56300dfd27c774c54d6423ac69b22e5f` | `583c3f329deb09700a956626038f3c2e56300dfd27c774c54d6423ac69b22e5f` |
| `results/d28be0a8-quality.json` | `ac87a4fe594652be1ee536cf31150b2242c2f614ad8d8d404f32124815683fe1` | `10f6fb41cfef098aeb10937ebcc026194653df52db1c75010a4420ebaeaa4199` |
| `results/d28be0a8-resources-new.json` | `70490c8592967d3acf4a5a16790221a0a56cccebd290f2929190c17487476b8f` | `2687827bf58f1411fb73d9cbae2f5bcd1bc1b5f398c2a714af759e455e689733` |
| `results/d28be0a8-resources-old.json` | `f673106f70c64bbe29996f9ead93e7bd7bbe054151418e576c94163045fb1aea` | `58dcc276bd3ff5c7ddf635b1378172a40f50cca73393dc3a052581a8b289007f` |

## 已验证与未验证

- 主工作区测试在 U-1 至 U-3 那一版是 **259 pass / 0 fail / 1 ignored**，之后的构建（`d28be0a8…`）是 **289 pass / 0 fail / 1 ignored**；两版的 Clippy 与 release 构建都通过。忽略的是 Windows 文件符号链接替换测试，需要开发者模式或相应权限。新增文件格式检查通过；全仓 fmt 仍有旧文件差异（那一版 9 个，之后的构建 8 个），不记作全仓通过。
- 七样本 2342 个受管文件、9368 查询，旧字段差异/查询错误/contract 缺失均 0；快照时间只比存在性。复用库刷新时 `parsed/unchanged/relinked` 计数与旧冷索引不同，未声称 IndexReport 整份相同。
- 协议测试 17 工具、33 调用：27 成功，6 预期拒绝。开场 255 字节、典型 context 1018 字节、本次最低预算 551 字节；CLI/MCP 逐字节一致、只读、问题不持久化、开场去重、子目录找根和未绑定拒建库通过。本机 ZCode 实际 context 调用已成功。详见 [验收记录](../docs/ACCEPTANCE.md)。
- `compare.py` 不冒领预算、凭据、项目隔离与并发快照的代码测试成绩，这些由对应工作区测试和协议检查验收。固定文本正则不是语义模型，任务仅核对规定依据，未逐句评判全部自然语言；标签之外的错边不统计。
- 没有新测原有 20 条经验基线、Go 加分样本、真实模型收益、词元计量、其他宿主、全新系统或所有语言声明链。当前工具不会据本地样本成绩宣称任意仓库的准确率或资源上限。
