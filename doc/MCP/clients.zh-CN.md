# 把 Loom 接入 MCP 客户端

[English](clients.md) · **简体中文** | [MCP 使用手册](README.zh-CN.md) · [项目中文首页](../../README.zh-CN.md)

客户端把已安装的 Loom 作为 **stdio 子进程**启动。接入不需要 Loom 服务地址、API 密钥、端口或额外常驻服务。把示例中的 `C:/Tools/Loom/loom.exe` 换成可信程序的实际绝对路径，Unix 系统例如 `/opt/loom/bin/loom`。路径含空格时在终端加引号；在 JSON/TOML 里，可执行文件是**一个字符串**，每个参数是数组中的一个元素。以下 Windows 路径用正斜杠，不用额外转义。

| 客户端 | 个人登记方式 | 跳到 |
|---|---|---|
| ZCode | 用户 JSON 的 `mcp.servers` | [ZCode](#zcode) |
| Claude Code | `claude mcp add --scope user` | [Claude Code](#claude-code) |
| OpenAI Codex | `codex mcp add` 或 TOML 表 | [Codex](#codex) |
| Pi | 内置 MCP 的版本用 `pi mcp add` | [Pi](#pi) |
| dsh / DeepSeek Harness | 给指定 profile 加 Cordis 补丁 | [dsh](#dsh) |

**登记一次，每个项目用自己的库。** 通常参数只有 `serve`。从要使用的项目目录打开客户端：Loom 从服务进程的工作目录向上找最近已绑定的项目，目前不会回退到 MCP `roots` 查询。如果客户端固定从别处启动子进程，只在**项目级配置**使用 `["--root", "D:/work/atlas", "serve"]`。不要在用户级配置写死某个项目的 `--root` 或 `--db`。模型密钥与 MCP 登记无关，不要写进这些示例。

## ZCode

用户配置为 `~/.zcode/cli/config.json`，工作区配置为 `<项目>/.zcode/config.json`。先备份原文件，只在 `mcp.servers` 下**合并 `loom` 条目**，不要覆盖其他服务。ZCode 原生格式不是 Claude 的 `mcpServers`：

```json
{
  "mcp": {
    "servers": {
      "loom": {
        "type": "stdio",
        "command": "C:/Tools/Loom/loom.exe",
        "args": ["serve"],
        "enabled": true
      }
    }
  }
}
```

在**设置 → MCP**中重连，或新开会话。如果该版本把工作区服务标成“待信任”，先核对程序与参数再确认。撤销时在原作用域只移除 `loom` 或把其 `enabled` 设为 `false`；仍然出现时检查其他作用域。

## Claude Code

让自己打开的多个项目共用登记：

```sh
claude mcp add --scope user --transport stdio loom -- "C:/Tools/Loom/loom.exe" serve
claude mcp get loom
```

`--` 后面才是要启动的程序。`--scope local` 只对当前项目和当前用户生效；`--scope project` 则写项目的 `.mcp.json`，可以共享。不要把只有本机才存在的程序路径提交给团队。某个项目需要显式指定根时，用 local，不用 user。会话内用 `/mcp` 看连接。撤销用户登记用 `claude mcp remove --scope user loom`，其他作用域换成原来的 scope。[Claude Code 官方 MCP 文档](https://code.claude.com/docs/en/mcp)

## Codex

```sh
codex mcp add loom -- "C:/Tools/Loom/loom.exe" serve
codex mcp get loom --json
```

也可不运行 `add`，直接把下面这张表加入生效的 `config.toml`，两种方式二选一。用户配置默认在 `~/.codex/config.toml`；设过 `CODEX_HOME` 时，实际文件是 **`$CODEX_HOME/config.toml`**。只在版本支持项目级配置、且项目已受信任时，才使用项目自己的 `.codex/config.toml`。

```toml
[mcp_servers.loom]
command = "C:/Tools/Loom/loom.exe"
args = ["serve"]
```

`get` 只能证明登记存在，不能证明会话读的是正确的库。在目标项目打开 Codex，用 `/mcp` 查看服务，再让助手调 `resume`。撤销全局登记用 `codex mcp remove loom`；项目文件中手工加入的表只从原文件移除。[Codex 官方 MCP 文档](https://developers.openai.com/codex/mcp/)

## Pi

**先看 `pi mcp --help`。** 内置 MCP 的新版支持：

```sh
pi mcp add loom -- "C:/Tools/Loom/loom.exe" serve
pi mcp list
```

用户配置是 `~/.pi/agent/mcp.json`；`pi mcp add --local …` 写入项目的 `.pi/mcp.json`，项目条目可能要先信任。若不用 `add`，只在原 JSON 里合并以下 `loom` 条目：

```json
{
  "mcpServers": {
    "loom": { "command": "C:/Tools/Loom/loom.exe", "args": ["serve"] }
  }
}
```

支持时在会话里用 `/mcp` 查看、`/reload` 重新加载。撤销时用 `pi mcp remove` 指定原作用域（参数看 `pi mcp remove --help`），或只删对应的 JSON 条目。**没有 `pi mcp` 的旧版**不会因为创建这个文件就获得 MCP；可在项目中直接用 Loom CLI，或者用你明确选择的 MCP 扩展并遵循该扩展的配置。替换 `/mcp` 的扩展也可能接管内置配置。[Pi 官方 MCP 文档](https://docs.pi.dev/latest/mcp)

<a id="dsh"></a>
## dsh / DeepSeek Harness

给使用的 **profile 加一份配置补丁**，插入已有的 `@deepseek-ai/dsh-mcp-client`。下面按 `@deepseek-ai/dsh` 0.2.2-rc.1 随包的 MCP 模板改为 stdio，不依赖 Loom 已发布专用插件。保存为 `loom.patch.yml`：

```yaml
- insert:
    - id: loom-mcp
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: loom
        transport: stdio
        command: 'C:/Tools/Loom/loom.exe'
        args: ['serve']
```

从项目目录启动你选定的 profile。示例使用 `web` 和补丁的绝对路径：

```sh
dsh --profile web --patch "C:/Tools/Loom/loom.patch.yml"
```

找到带命名空间的 Loom 工具（例如 `mcp__loom__resume`），核对它报告的项目根。只在本次启动使用补丁时，下次去掉 `--patch` 即可撤销。如果之后需要持久化，只把该插入项合并到目标 `$DSH_HOME/profiles/<name>/cordis.patch.yml`，不要用全用户补丁绑定单个项目；撤销时删除该插入项。profile 如何组合、子进程从哪里启动，可能因版本而异。[DeepSeek Harness 上游](https://github.com/deepseek-ai/deepseek-harness)

## 验证的是项目，不只是连接

1. 在目标工作区启动客户端并调用 Loom 的 `resume`。**未绑定**也能握手，此时只有 `resume` 一个工具。若提示“身份不符”或已绑定的库丢失，先排查，不要新建空库顶替。
2. 真正的新项目照 `loom guide` 或[首次使用流程](README.zh-CN.md#workflow)执行；绑定后同一服务可提供 **17 个工具**。客户端缓存旧列表时重连。
3. 核对 `resume` 的项目标识和根目录。没收到自动开场快照时，调 `overview` 看已确认的目标和红线；再对一个实际已索引文件调 `context`，验证从客户端到项目库的完整路径。

**实测范围：** ZCode 已在本机调通工具；Claude Code 显示 `Connected`；Codex 已登记并回读配置。独立 stdio 握手核对了 17 个工具及项目身份。Pi、dsh 的例子依据上游文档或随包模板，**不是**真实宿主端到端成绩。净机和各宿主“未绑定 → 刷出工具”的实测仍列在[预期欠账](../../docs/DESIGN.md#design-debt)。
