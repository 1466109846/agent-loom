# Connect an MCP client to Loom

**English** · [简体中文](clients.zh-CN.md) | [MCP handbook](README.md) · [Project home](../../README.md)

A client starts the installed Loom executable as a **stdio child process**. There is no Loom server URL, API key, port or background service to configure. Replace `C:/Tools/Loom/loom.exe` in the examples with the absolute path to your trusted executable (for example `/opt/loom/bin/loom` on Unix). With spaces in the path, quote it in the shell; in JSON/TOML keep the path in **one string** and each argument in its own string. Forward slashes in these Windows examples work without JSON backslash escaping.

| Client | Personal registration | Where to start |
|---|---|---|
| ZCode | `mcp.servers` in user JSON | [ZCode](#zcode) |
| Claude Code | `claude mcp add --scope user` | [Claude Code](#claude-code) |
| OpenAI Codex | `codex mcp add` or a TOML table | [Codex](#codex) |
| Pi | `pi mcp add` on versions that ship MCP | [Pi](#pi) |
| dsh / DeepSeek Harness | A Cordis patch on a chosen profile | [dsh](#dsh) |

**One registration, separate project databases.** Normally the only argument is `serve`. Open the client in the intended project's directory: Loom walks up from the child process's working directory to the nearest bound project. It does **not** ask the client for an MCP `roots` fallback. If a client starts children elsewhere, use a **project-scoped** entry with arguments `["--root", "D:/work/atlas", "serve"]`. Never put a single project's `--root` or `--db` in a user-wide entry. Keep model credentials out of host registration; they are not needed for MCP.

## ZCode

Merge **only the `loom` entry** under `mcp.servers` in `~/.zcode/cli/config.json` (user) or `<project>/.zcode/config.json` (workspace). This is ZCode's native format, not Claude's `mcpServers` format. Back up the existing file before editing it.

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

Reconnect in **Settings → MCP** or open a fresh session. If your version marks a workspace entry as pending trust, check its executable and arguments before trusting it. To undo, remove just `loom` from the same scope or set its `enabled` to `false` (check other scopes if it still appears).

## Claude Code

Use a user-scoped entry to make Loom available across your projects:

```sh
claude mcp add --scope user --transport stdio loom -- "C:/Tools/Loom/loom.exe" serve
claude mcp get loom
```

`--` separates Claude's options from the program it launches. `--scope local` means this project, for this user; `--scope project` writes shareable `.mcp.json` in the project. Do not commit a path that only exists on your machine. If your own project needs an explicit root, choose local scope instead of user scope. Use `/mcp` inside Claude Code to inspect the connection. Remove a user entry with `claude mcp remove --scope user loom`; use the original scope for a project or local entry. [Claude Code MCP documentation](https://code.claude.com/docs/en/mcp)

## Codex

```sh
codex mcp add loom -- "C:/Tools/Loom/loom.exe" serve
codex mcp get loom --json
```

Alternatively, add this table **once** to the active `config.toml` (do not register twice). The default user file is `~/.codex/config.toml`; if `CODEX_HOME` is set, it is **`$CODEX_HOME/config.toml`**. For a project-specific configuration use the project's `.codex/config.toml` only if your version supports project configuration and the project is trusted.

```toml
[mcp_servers.loom]
command = "C:/Tools/Loom/loom.exe"
args = ["serve"]
```

`get` confirms the registration, not which database a running chat opened. Start Codex in the intended project, open `/mcp` and ask for `resume`. To undo the global CLI registration, run `codex mcp remove loom`; for a manual project entry remove only its table. [Codex MCP documentation](https://developers.openai.com/codex/mcp/)

## Pi

Check **`pi mcp --help` first**. Versions with built-in MCP support accept:

```sh
pi mcp add loom -- "C:/Tools/Loom/loom.exe" serve
pi mcp list
```

The user file is `~/.pi/agent/mcp.json`; `pi mcp add --local …` writes `.pi/mcp.json` in the project. A project entry may require project trust. If editing by hand instead of using `add`, merge only this `loom` entry:

```json
{
  "mcpServers": {
    "loom": { "command": "C:/Tools/Loom/loom.exe", "args": ["serve"] }
  }
}
```

Use `/mcp` and `/reload` in a running Pi session where supported. Remove the entry at its original scope with `pi mcp remove` (see `pi mcp remove --help`), or remove just its JSON entry. **Older releases without `pi mcp`:** adding this JSON alone will not enable MCP. Use Loom's CLI in the project, or follow the instructions of an MCP extension you have explicitly chosen. An extension that replaces `/mcp` can also replace the built-in configuration. [Pi MCP documentation](https://docs.pi.dev/latest/mcp)

<a id="dsh"></a>
## dsh / DeepSeek Harness

Use a **profile patch** that inserts the existing `@deepseek-ai/dsh-mcp-client`. This example follows the MCP bundle template shipped with `@deepseek-ai/dsh` 0.2.2-rc.1, adapted to stdio. It does not require a published Loom plugin. Save it as `loom.patch.yml`:

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

Start your chosen profile from the project directory; the example uses the `web` profile and a patch at a known absolute path:

```sh
dsh --profile web --patch "C:/Tools/Loom/loom.patch.yml"
```

Look for the namespaced Loom tools (for example `mcp__loom__resume`) and check the root they report. A one-launch patch is reversible: omit `--patch` next time. If you later make it permanent, merge that one insertion into the intended `$DSH_HOME/profiles/<name>/cordis.patch.yml`, not a home-wide patch targeting a single project; remove that insertion to undo. Profile composition and working-directory behavior can vary with the dsh version. [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)

## Verify the project, not just the connection

1. Open the client in the intended workspace and call Loom's `resume`. An **unbound** workspace can connect successfully while listing **only `resume`**. Do not create a replacement database if Loom reports an identity mismatch or a missing, previously bound database.
2. For a genuinely new project, follow `loom guide` or the [first-run workflow](README.md#workflow). Once initialized, the same server can expose **17 tools**; reconnect if the client has cached the old list.
3. Confirm both the project ID and root in `resume`. If no startup overview arrived, call `overview` for confirmed goals and constraints; use `context` on a real indexed file to check the full path from client to project.

**Verification scope.** ZCode tool calls were exercised locally; Claude Code reported `Connected`; Codex registration was read back. An independent stdio handshake listed 17 tools and verified project identity. Pi and dsh examples describe upstream interfaces/templates, **not** completed end-to-end tests. The clean-install and live unbound-to-bound checks across hosts remain [tracked design debt](../../docs/DESIGN.md#design-debt).
