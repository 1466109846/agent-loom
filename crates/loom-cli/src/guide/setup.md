# 把当前项目接入 Loom

读者是编码助手。下面的步骤由你执行，标了“问用户”的地方先问，其余直接做。任何一步报错：把原文告诉用户并停下，不要用删除或新建 `.loom` 里的东西来“修复”。

## 1. 确认可执行文件

下文的 `loom` 指你运行这份步骤用的那个程序。`loom --version` 报找不到命令时，它不在 PATH 里：后面的命令都改用 `resume` 的提示里给出的完整路径。

## 2. 定下项目根

- 用 Git 的项目：`git rev-parse --show-toplevel` 的输出。
- 不用 Git 的项目：用户打开的工作区目录。
- 家目录、盘符根目录、下载目录、临时目录不是项目，不绑定。拿不准哪一级是根，问用户。

## 3. 看是否已经接入

```
loom --root <项目根> stats
```

- 输出 `项目：… · 标识=… · 根=…`：已经接入，跳到第 5 步。
- 报“项目库未初始化”或“未找到已绑定项目”：还没有接入，继续。
- 报别的（如“项目身份不符”“旧库尚未绑定”）：原文告诉用户，停。

## 4. 绑定并建索引

```
loom --root <项目根> init
loom --root <项目根> index
```

`init` 输出项目标识和根目录，`index` 输出受管文件数、未覆盖数和这次用的范围。两条都不需要问用户：它们只在本机建一个小目录 `.loom`，不改源码。

用 Git 的项目：`.loom/` 是本机数据，不进仓库。`git check-ignore -q .loom` 返回非零时，把一行 `.loom/` 加到 `git rev-parse --git-path info/exclude` 指出的文件末尾。这个文件只在本机生效，不改仓库里的 `.gitignore`。

## 5. 接通会话工具

调用会话工具 `resume`。

- 首行是这个项目的标识和根目录：接通了。
- 仍然提示没有接入：请用户在宿主的 MCP 设置里重连 Loom，或新开一个会话。在那之前用同名的命令行，如 `loom recall <关键词>`、`loom context <文件> --mode impact`。
- 会话里根本没有 Loom 的工具：宿主里没有登记，见最后一节。

## 6. 汇报

用三四行告诉用户：

- 项目标识和根目录；
- `loom stats` 第二行的库位置和大小；
- 受管文件数和未覆盖数；
- 两件可选的事：库可以随时搬到别的盘（`loom guide storage`）；向量检索与重排默认关闭，需要时可以启用（`loom guide models`）。这两件不要替用户决定。

## 可选：迁入旧的文本记忆

用户提到有 memos 导出的 wiki 页时：先 `loom import <目录>` 预演，把统计给用户看；用户同意后再 `loom import <目录> --apply`。

## 在宿主里登记（只在宿主里没有 Loom 这一条时）

命令是 loom 可执行文件的绝对路径，参数只有 `serve`，不要写 `--root`。改宿主配置前先问用户，并先备份原文件；登记后要用户重连或新开会话。

- ZCode：在 `~/.zcode/cli/config.json` 的 `mcp.servers` 里加 `"loom": {"type": "stdio", "command": "<绝对路径>", "args": ["serve"], "enabled": true}`
- Claude Code：`claude mcp add --scope user loom -- <绝对路径> serve`
- Codex：`codex mcp add loom -- <绝对路径> serve`
