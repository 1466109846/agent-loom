# Loom MCP 使用手册

[English](README.md) | **简体中文** · [项目 README](../../README.zh-CN.md) · [客户端接入](clients.zh-CN.md)

Loom 为编码助手提供项目本地记忆、显式更新的源码依赖快照、经确认的项目意图与跨会话任务恢复。MCP 接口通过 **stdio 子进程**提供这些能力。`loom serve` **不是 Web 服务器**：不监听端口，也不提供 HTTP 地址。

本手册说明项目选择、日常工作、工具与安全边界。各宿主的登记方法见[客户端接入](clients.zh-CN.md)；安装与完整 CLI 参考见[项目 README](../../README.zh-CN.md)。

[启动与验证](#start) · [项目隔离](#projects) · [助手工作流](#workflow) · [17 个工具](#tools) · [调用示例](#examples) · [输出预算](#budgets) · [确认边界](#confirmation) · [存储与隐私](#privacy) · [排查问题](#troubleshooting)

<a id="start"></a>
## 1. 启动与验证

### 准备真实项目

选择实际要工作的仓库或工作区，不要选择 Loom 的安装目录、用户主目录或盘符根目录。Git 项目可用 `git rev-parse --show-toplevel` 确认仓库根；非 Git 项目使用实际工作区根目录。根目录不明确时，先确认再绑定。

下面是 **PowerShell 示例**。将两个绝对路径替换为已存在的位置；`src/app.ts` 仅为示意，请换成项目内实际存在的源码文件。

```powershell
& 'C:/Tools/Loom/loom.exe' --help
Set-Location 'D:/work/acme-api'
& 'C:/Tools/Loom/loom.exe' --root . stats
# 仅在确认项目确实尚未绑定时执行：
& 'C:/Tools/Loom/loom.exe' --root . init
& 'C:/Tools/Loom/loom.exe' --root . index
& 'C:/Tools/Loom/loom.exe' --root . stats
& 'C:/Tools/Loom/loom.exe' --root . context src/app.ts --mode impact
```

- 第一次 `stats` 已输出正确的项目标识和根目录时，跳过 `init`。对同一有效根目录重复 `init` 会返回同一标识。
- 只有显式 `init` 才会创建、绑定库。查询、MCP 启动与 `resume` **不会默默创建空库**；`index` 只更新已绑定项目的结构快照。
- 库丢失、身份不符或旧库尚未认领属于修复场景，不代表可以新建替代库。应停止并按诊断处理。
- `loom guide` 只输出接入步骤，不执行接入。`init` 必须显式指定 `--root`。

### 给宿主可执行文件和独立参数

下面只有启动字段，**不是某个宿主的完整配置**；请按[客户端接入](clients.zh-CN.md)合并到对应配置中。

```json
{
  "command": "C:/Tools/Loom/loom.exe",
  "args": ["serve"]
}
```

`command` 使用已安装程序的绝对路径；Linux/macOS 可使用 `/opt/loom/loom` 这样的绝对二进制路径。不要把 `serve` 拼进 `command`，不要指向目录，也不要配置 URL 或端口。宿主管理子进程及其标准输入、输出。手动运行 `serve` 后没有提示符是正常现象：它在等待 MCP 消息，不接受交互式 CLI 命令。

### 验证连接，而不只是进程启动

1. 在宿主中连接或重连 Loom，然后调用 MCP 工具 `resume`，参数为 `{}`。
2. **未绑定**：仅公开 `resume`。诊断会引导运行 `loom guide`，并给出当前服务程序的完整路径；不会自动初始化。在目标项目内完成显式 CLI `init`、`index` 后，再调用 `resume`。
3. **已绑定**：读写项目知识前，核对返回的**项目标识与规范化根目录**。根目录必须与工作区一致。如果预览因安全或长度限制省略了根目录，应在本机用 `loom --root <项目根的绝对路径> stats` 核对，不要猜测。
4. 绑定成功后应能发现[工具目录](#tools)中的全部 **17 个工具**。同一进程可发现新绑定，并发送 `notifications/tools/list_changed`。宿主未刷新工具列表时，重连 Loom 或新开会话。

`resume` 是 MCP 工具，**不是 CLI 子命令**。终端中用 `loom stats` 核对身份，用 `loom canvas list` 查看未完成任务；不要为缺失的工具编造 CLI 命令。

<a id="projects"></a>
## 2. 项目选择与隔离

不传 `--root` 时，Loom 从**服务进程启动时的工作目录**向上查找最近的 `.loom/project.id` 或旧版 `.loom/loom.db`。这是宿主启动服务时的目录，不一定是助手后来打开终端时所在的目录。运行中的服务不会跟随后续 shell 的 `cd`；身份核对成功后，也会拒绝切换到另一个项目标识。

| 启动参数 | 选择规则与用途 |
|---|---|
| `["serve"]` | 只有宿主在各项目目录分别启动独立进程时，才能跨项目复用这条配置。找不到绑定时仅公开 `resume`。 |
| `["--root", "D:/work/acme-api", "serve"]` | 将一个进程固定到该绝对项目根；适合宿主启动目录不确定时使用。 |
| 一条共享或全局配置固定到项目 A | 使用该配置的所有工作区都会读写 A。不要用它服务互不相关的项目。 |

- **子目录**：CLI 与未固定根目录的服务可以找到父项目绑定。工具文件参数（`target`、`path`、`anchors`）始终相对**项目根**，不是子目录；在 `src/` 内仍写 `src/app.ts`，不写 `app.ts`。
- **嵌套项目**：最近的绑定优先；Loom 不会越过损坏或旧版绑定去使用更上层项目。需要独立的嵌套工作区时，显式初始化它自己的根；父项目的结构扫描会排除嵌套 Loom 项目。
- **同名但不同根**：目录名只是显示名称，不是身份。两个都叫 `api` 的项目，只要分别初始化，就有独立的标识和数据库。
- **克隆与副本**：不含 `.loom/` 的新克隆可独立初始化。把已绑定的 `.loom/` 复制到另一个根目录不会获得复用授权，根或标识不符会被拒绝。真正搬迁需在原根已不存在后，显式执行 `init --relocate <原项目标识>`；原项目仍存在时，不能用它把副本当作搬迁。
- **外置数据库**：项目会记住库的位置，`serve` 不需要另加 `--db`。显式 `--db` 会覆盖该位置，但仍必须通过同样的项目身份核对。

如果宿主跨工作区复用同一子进程，或在错误目录启动服务，应改为独立的服务实例并显式指定根目录。分别用 `resume` 验证；登记成功本身不证明项目已隔离。

<a id="workflow"></a>
## 3. 助手工作流

`resume` 恢复的是**工作现场**：身份、未完成任务、下一步、证据位置、近期记忆标题与复核提示。它不能代替提供**已确认目标、红线和置顶经验**的 `overview`。会话启动时，如果没有收到开工快照，应调用 `overview`；`refresh` 显式重选这份概览，不重建源码索引，也不复核文件职责。

下面的可选片段可复制到项目的 `AGENTS.md` 或 `CLAUDE.md`。它是工作约定，不是宿主登记或配置文件。

```markdown
## Loom 工作流
- 会话启动时调用 MCP 工具 resume，核对项目标识与根目录；不符就停。没有收到开工快照时，再调 overview 取得已确认目标和红线。
- 项目未绑定时，先读 loom guide，明确实际工作区根目录，再用 CLI init/index。库丢失或身份不符时，不新建替代库。改宿主登记前先问用户。
- 读或改源码前调用 context：target 相对项目根，mode=impact 查使用方，mode=explain 查依赖。继续核对引用的源码；结论保留覆盖缺口与不确定性。
- 诊断要求更新索引时，以及每批源码修改后，显式调用 index。查询与 refresh 都不会重建索引。
- 保存经验前先 recall 查重。remember 只存已验证、不能只靠读代码得知的决策原因、坑或根因，并用 anchors 绑定相关文件。先看相似候选再用 force=true；过时内容优先 archive，不直接删除。
- 用 canvas 保存未完成工作的目标、具体下一步和真实证据位置；推进时 update，完成时 close。近期记忆不是待办列表。
- 规则先用 intent propose 提议。CLI 确认、撤销、取代、置顶、note review、声明证据关系、改库位置或配置、启用模型、分享导入与不可恢复删除前，先取得用户明确同意。提议与助手写入的职责不等于人工确认。
- 不在提示词中索要凭据，不把凭据写进工具参数、记忆或 shell 参数。密钥由用户在本机通过 `loom config secret embedding` 或 `loom config secret rerank` 输入，或只提供存放密钥的环境变量名。
```

`code_drift`、`orphaned`、`unverified` 与时间过久等提示，意味着需要核对当前源码，不代表记忆必然错误或仍然有效。“本次快照受管范围内没有查到”不等于“整个项目不存在”。

<a id="tools"></a>
## 4. 公开 MCP 工具目录：17 个工具

下表使用服务公开的原始工具名；宿主可能加命名空间前缀。参数是 JSON 对象，不是 CLI 字符串。不要在工具参数中加入 `--root` 或 `--db`，项目选择属于服务启动参数。

**加粗参数必填**，各 action 的条件必填项另行写明。“读”表示不改变项目知识；“会话”只改变当前连接的快照交付状态；“写”会持久化变更。可选模型请求与 `recall` 的向量缓存写入在下文单独说明。

### 会话与结构

| 工具 | 参数 | 读写与用途 |
|---|---|---|
| `resume` | 可选 `hours`（整数，默认 24，限制在 1–720） | 读。核对身份、恢复未完成工作，也用于诊断未绑定项目。 |
| `overview` | `{}` | 读＋会话。每个连接交付一次已确认目标、红线和置顶经验；后续只返回已交付快照的编号。 |
| `refresh` | `{}` | 读＋会话。重选当前概览并返回新快照；不扫描源码，不复核职责。 |
| `context` | **`target`**、**`mode`**（`explain` 或 `impact`）；可选 `question`、`depth`（1–3，默认 1）、`budget`（1–6144，默认 1500） | 读。编辑前一次取得约束、依赖或使用方、职责、经验与缺口。`question` 是空格分隔的关键词，最多 512 个 UTF-8 字节；不调用模型。 |
| `index` | `{}` | 写。显式建立或更新结构快照，报告解析、未变和未覆盖文件。 |
| `explain` | **`target`**；可选 `depth`（1–3，默认 1） | 读。从已有快照查目标文件引用哪些文件。 |
| `impact` | **`target`**；可选 `depth`（1–3，默认 1） | 读。从已有快照查哪些文件引用目标文件。 |

### 经验记忆

| 工具 | 参数 | 读写与用途 |
|---|---|---|
| `remember` | **`type`**、**`title`**、**`body`**；可选 `anchors`（字符串数组）、`tags`（字符串数组）、`importance`（1–5，默认 2）、`force`（默认 false） | 写。标题为单行，最多 120 字符；正文 8–2000 字符。锚点用 `src/a.rs` 或 `src/a.rs#Foo::bar`。完全重复复用原记录；相似候选应先核对，再决定是否 `force=true`。 |
| `recall` | 可选 `query`（默认空）、`types`（字符串数组）、`top_k`（默认 8，最多 50）、`full`（默认 false）、`include_archived`（默认 false） | 读；启用混合检索时可能写向量缓存。空查询列近期记忆，不调用模型；非空查询使用空格分隔的关键词。项目启用后可走关键词＋向量及可选重排。`full=true` 请求完整正文，而不是默认预览。 |
| `manage` | **`action`**；`get`、`archive`、`activate` 提供 `ids`（字符串数组）；`supersede` 提供旧 `ids` 和新 `by`；`delete` 提供 `ids` 与 `confirm=true`；`stats` 无需编号。 | `get`、`stats` 为读，其余为状态写入或删除。可取完整记录和锚点；优先归档，不可恢复删除需谨慎。使用 Loom 实际返回的编号。 |

`type`、`types` 可用值：`DECISION`、`BUGFIX`、`GOTCHA`、`ERROR_PATTERN`、`CONFIG`、`FEATURE`、`MILESTONE`、`PROGRESS`、`CODE_PATTERN`、`SYNTHESIS`。

### 意图、文件职责、任务与证据

| 工具 | 参数 | 读写与用途 |
|---|---|---|
| `intent` | **`action`**：`list` 不要其他字段；`get` 要 `id`；`propose` 要 `kind`、`title`、`body`。`kind` 为 `goal`、`constraint`、`non_goal`、`decision`。 | list/get 为读，propose 为写。提议尚未生效；MCP 不提供确认、撤销、取代或置顶。 |
| `note` | **`path`**；可选 `action`（默认 `get`，或 `set`）。`set` 还须提供 `expected_sha`、`role`。 | get 为读，set 为写。set 前核对源码及 `note get` 返回的当前完整 SHA-256；服务不代填指纹或授予人工批准。人工复核使用 CLI `note review`。 |
| `canvas` | **`action`**：`list` 不要其他字段；`get`、`close` 要 `id`；`open` 要 `goal`、`next`、`evidence`；`update` 要 `id`、`next`、`evidence`。提供的值均为字符串。 | list/get 为读，open/update/close 为写。`evidence` 记录真实证据位置，不自动收集或虚构证据；close 保留历史。 |
| `evidence` | **`query`** | 读。汇集直接记录、明确声明的支持或冲突关系、复核提示与缺口；不推断或写入关系。 |

`intent`、`note`、`canvas` 只传所选 action 接受的字段，例如 `canvas open` 不带 `id`，`note get` 不带 `role`。

### 可选的本机增强工具

| 工具 | 参数 | 读写与用途 |
|---|---|---|
| `semantic` | **`query`** | 读。只查本项目逐条人工授权的有效记忆标题，最多 12 条。没有模型时也只在同一集合内回退关键词；不能授权或撤权。 |
| `rerank` | **`query`** | 读。只重排最多 12 个关键词候选标题；本机增强不可用时保留关键词顺序，不是全库语义召回。 |
| `extract` | **`text`** | 读。只从本次明确提交的文本提炼待审建议，不写记忆、不确认意图；区分模型停用或不可用与没有建议。 |

这三个工具与项目级 `recall` 混合检索是两套入口，见[隐私与模型授权](#privacy)。

**Schema 来源**：[公开工具声明和主要参数类型](../../crates/loom-mcp/src/lib.rs)、[各 action 的参数与校验](../../crates/loom-mcp/src/knowledge.rs)、[绑定、工具发现与会话行为](../../crates/loom-mcp/src/session.rs)。实际运行程序的接口以连接后返回的 `tools/list` 为准；旧程序可能公开更少的工具。

<a id="examples"></a>
## 5. 与宿主无关的工具调用示例

下面是 MCP `tools/call` 使用的 `name` 和 `arguments` 对象，不是终端命令，也不是某个宿主的包装语法。六个示例都在请求信息，不改变项目知识；混合检索启用时，`recall` 仍可能更新向量缓存。文件路径在 **Loom 源码仓库**内实际存在；用于其他项目时，换成该项目中已有的根相对路径。查询没有命中也是有效结果。

核对项目身份并恢复工作：

```json
{"name":"resume","arguments":{}}
```

没有收到开工快照时，读取启动约束：

```json
{"name":"overview","arguments":{}}
```

查看修改 Loom MCP 实现的影响面，并使用较大的字节预算：

```json
{"name":"context","arguments":{"target":"crates/loom-mcp/src/lib.rs","mode":"impact","depth":1,"budget":3000}}
```

保存经验前先查重：

```json
{"name":"recall","arguments":{"query":"project identity","types":["DECISION","GOTCHA"],"top_k":5}}
```

只查看文件职责与指纹，不修改：

```json
{"name":"note","arguments":{"path":"crates/loom-core/src/project.rs"}}
```

读取未完成任务列表，而不是把近期记忆当待办：

```json
{"name":"canvas","arguments":{"action":"list"}}
```

<a id="budgets"></a>
## 6. 输出预算与证据边界

预算按 **UTF-8 字节**计算，不是词元数，也不是字符数；限制涵盖完整的有界文本，包括编号与省略提示。

| 输出 | 上限 |
|---|---:|
| 宿主选择启用的自动开工快照 | 800 字节 |
| `overview`、`refresh` | 各 2000 字节 |
| `recall`、`resume`、`explain`、`impact` 的默认预览 | 各 1500 字节 |
| `evidence`、`semantic`、`rerank`、`extract` 的 MCP 预览 | 各 1500 字节 |
| `context` | 默认 1500；`budget` 范围 1–6144 字节 |

只有宿主协商了 `capabilities.experimental["loom/overview"].textBudget = "utf8-byte-upper-bound"`，才会自动发送开工文本。其他宿主应在启动时显式调用 `overview`。一个 stdio 连接就是会话边界；重复 `overview` 不重发正文。需要重新选择快照时用 `refresh`；它无法擦除已经进入宿主对话的旧结论。

预览按整行省略并报告省略情况，不截断半条证据。需要继续查看时，缩小目标或深度，按编号 `manage get`、用 `recall` 的 `full=true` 取正文，或在 CLI `explain`、`impact`、`context` 后加 `--json` 查看完整结构证据。`context` 的预算不足以容纳必要诊断时，按错误给出的最低值提高预算，上限 6144。

结构结果是静态证据，不是完整的运行时调用图；结果附带快照代次、确定程度与覆盖缺口。查询不刷新快照，有界现场核对也不是全项目扫描；新增文件可能直到 `index` 后才可见。省略、未覆盖、未确定或范围外，都不能转述为没有依赖。

<a id="confirmation"></a>
## 7. 确认边界与 CLI 专属操作

助手可以通过 MCP 读取证据、保存经验和任务、刷新索引、提议意图。这些权限**不代表**提议已经成为有效规则，也不代表写过文件职责就完成人工复核。

下列操作**不是 MCP 工具**。涉及改变权威状态、搬移数据、启用模型或分享内容时，先取得用户明确同意，再由用户或获授权的助手执行 CLI；仅运行预演不会应用变更。

| 边界 | CLI 入口 |
|---|---|
| 绑定、认领或搬迁项目 | `loom --root <根目录> init`；只有核对归属或搬迁后才用 `--adopt-legacy`、`--relocate <编号>`。普通首次 init 不带 `--yes`。 |
| 确认、撤销、取代意图；置顶或取消置顶经验 | `loom intent confirm <编号> --yes`；`revoke <编号> --yes`；`replace <旧编号> <提议编号> --yes`；`pin <记忆编号> --yes`；`unpin <记忆编号> --yes` |
| 复核文件职责 | `loom note review <路径> <当前完整SHA256> --yes` |
| 声明证据关系 | `loom evidence link <来源编号> <目标编号> <supports\|conflicts> --yes` |
| 搬库或修改本机设置 | `loom db move <目标>` 只预演，加 `--yes` 执行。`loom config set …`、`config secret …` 仅通过 CLI 提供；见 `loom guide storage`。 |
| 配置、启用模型 | `loom guide models`；`loom semantic enable` 预演，`loom semantic enable --yes` 授权并建向量。`semantic disable --yes`、`semantic index`、`semantic allow <编号> --yes`、`semantic deny <编号> --yes` 均为 CLI 专属。 |
| 导入或分享 | `loom import <wiki目录>` 预演，`--apply` 写入。`loom share preview`、`share export`、`share import` 提供内容复核流程；应用前查看对应 `--help`。 |

`manage delete` **确实通过 MCP 提供**，但不可恢复，必须先取得用户明确同意，再传 `confirm=true`。这个布尔参数只是保护开关，不是用户已经同意的证据。仅需停用旧经验时使用归档。模型测试、下载与宿主登记也不属于这 17 个工具，不要把它们当成自动连接检查。

<a id="privacy"></a>
## 8. 存储、模型授权与实际隐私

| 位置 | 用途 |
|---|---|
| `<根>/.loom/project.id` | 项目标识；加载内容前会与数据库及规范化根目录互相核对。 |
| `<根>/.loom/loom.db` | 默认 SQLite 库，保存记忆、意图、职责、任务、证据关系、结构快照及可选向量。 |
| `<根>/.loom/database.path` | 库外置时记录其绝对位置；项目元数据仍需跟随项目保留。 |
| `<根>/.loom/ignore` | 可选的额外源码排除规则，只能排除，不能重新放行。 |
| 用户配置目录中的 `config.toml`、`credentials.toml` | 本机的模型、存储偏好与凭据，独立于项目数据。`loom config path` 可显示配置位置。 |

不要把 `.loom/` 提交到 Git 或未经复核上传；可使用仓库忽略规则或本机 Git 排除规则。`loom stats` 显示当前库的位置与大小。不要手工搬动使用中的 SQLite、WAL 文件，获同意后使用 `loom db move`。文件复制备份前先结束使用该项目的会话，同时保留数据库和 `.loom` 元数据。避免把数据库放在网络共享盘或云同步目录。

### 默认：本地运行，不要求模型

可选模型功能关闭时，Loom 的本地记忆与结构操作不需要网络或模型服务。索引存储路径、符号名、导入或包含写法及行号，不复制源码中的值；必要时仍应显式排除私密源码路径。记忆、职责和任务文本也可能包含内部信息。返回给助手的数据还受**宿主及其模型**的数据处理规则影响；Loom 本地存储不等于云端助手也在本地运行。

### 可选的项目级混合检索

配置模型后，`loom semantic enable` 会预演地址、发送范围与向量存储估算，不发送模型请求。只有取得同意后，才执行 `loom semantic enable --yes` 授权该项目并建立向量。

- `recall` 可能把脱敏查询词与有效记忆的脱敏**标题、正文**发给配置的本机或云端地址；每条记忆文档最多 2000 字符，可选重排最多处理 24 条候选文档。它也可能在本地库增量写入缺少的向量。空查询列近期记忆时不调用模型。
- 编号、锚点、路径、标签、项目标识和根目录、源码、意图、职责及任务画布不会作为独立载荷字段附带发送，**但它们可能已经出现在用户写入的文本里**。脱敏不是内部信息识别器；同意前应复核文本和服务端的数据保留政策。
- 授权按项目保存，绑定配置的端点 origin。换主机或端口需要重新同意；换模型名需要重建旧向量。重排地址授权不符时不会调用该重排地址，也不会丢弃已成功取得的向量检索结果。
- **向量失败**：说明原因并回退关键词。**向量成功后重排失败**：保留关键词＋向量的融合顺序并说明重排失败，不会直接退回纯关键词。

### 独立且有界的本机增强

MCP 工具 `semantic`、`rerank` 使用单独启用的环回增强（`LOOM_EMBED_MODEL`），`extract` 使用 `LOOM_EXTRACT_MODEL`。两者默认均关闭，只连接 `127.0.0.1`，端口由 `LOOM_MODEL_PORT` 指定，默认 11434；不继承项目混合检索的地址配置。`semantic` 要逐条 CLI 授权，最多 12 个有效标题；`rerank` 只看最多 12 个关键词候选标题；`extract` 只看当次提交并脱敏的短文本。回退和查询范围见[工具目录](#tools)。

不要把凭据放进提示词、工具参数、记忆、分享导出或命令行参数。请用户在本机执行 `loom config secret embedding`、`loom config secret rerank` 输入，或只提供存放凭据的环境变量名。不要让助手读取凭据文件来排查连接。

<a id="troubleshooting"></a>
## 9. 错误与排查

CLI 失败时退出码非零，诊断写到标准错误；MCP 工具失败不等于成功返回空结果。继续操作前，检查错误文本、省略提示与回退说明。

| 现象 | 做法 |
|---|---|
| 宿主里没有 Loom 工具 | 查宿主中的服务状态和 stderr、程序绝对路径、独立的 `serve` 参数及待确认的信任提示。改配置后重连。按[客户端接入](clients.zh-CN.md)排查，不要填写 Web 服务 URL。 |
| 只有 `resume`，提示未绑定 | 核对真实项目根，在该根显式 CLI `init`、`index` 后再调 `resume`。工具列表变更通知未生效时重连。 |
| 根目录不对，或看到别的项目的记忆 | 停止调用，检查固定的 `--root` 与宿主启动进程时的 cwd。改正选择、重连并验证 `resume` 后再继续。 |
| 库丢失、身份不符、旧库尚未绑定 | 保留 `.loom/` 与数据库，按具体诊断修复或认领。不要删标记或新建替代库。手工搬过库时，核对目标后才按诊断显式 `init --db <新位置>` 重新关联。 |
| 工具少于 17 个，或缺少 `context`、`guide` | 对宿主实际使用的**同一个绝对路径程序**运行 `--help`。旧构建和 PATH 中另一个程序可能有相同版本号；选择或更换正确构建，再重启对应服务进程。 |
| 快照版本不符、文件已变、提示先索引 | 调 MCP `index` 或 CLI `loom index` 后重查；`refresh` 只刷新概览。 |
| 目标不在受管范围，或无结构命中 | 查根相对路径和忽略规则；新增文件先索引。保留未覆盖、未确定提示，缩小查询或看 CLI `--json`，不要直接宣称不存在。 |
| `context` 预算不足，或 question 查不到 | 按提示提高到最低预算，上限 6144。关键词用空格分隔，不带空格的整句算一个词。 |
| 有相似记忆，或被拒绝为代码复述 | 用 `manage get` 查看候选；复用、取代或在核对后有意 force。保存已验证的背景、结论与原因，不改写源码凑经验。 |
| `note set` 指纹过期 | 重新检查当前文件和 `note get` 的指纹，必要时先 index。不要编造 SHA，也不要把普通写入当作人工复核。 |
| 模型回退或地址授权提示 | 读具体原因并查 `loom semantic status`。重新授权前复核新地址。只有获授权才执行会发送请求的 `loom model test`；仅重排失败时保留已成功的混合融合结果。 |
| 搬库提示仍在使用 | 等当前调用结束或关闭旧服务进程。当前服务在调用之间释放库连接；不要手工删除锁或 WAL 文件。 |
| Windows 规范化根目录以 `\\?\` 开头 | 这是 Windows 的规范化路径表示，不是另一个工作区；比较实际规范化位置。 |

[返回工具目录](#tools) · [助手工作流](#workflow) · [客户端接入](clients.zh-CN.md) · [项目 README](../../README.zh-CN.md)
