# Loom MCP manual

**English** | [简体中文](README.zh-CN.md) · [Project README](../../README.md) · [Client setup](clients.md)

Loom gives coding assistants project-local memory, explicit source-dependency snapshots, confirmed project intent, and resumable tasks. The MCP interface exposes these capabilities through a **stdio subprocess**. `loom serve` is not a web server: it does not listen on a port or provide an HTTP endpoint.

Use this manual for project selection, daily work, tools, and safety boundaries. Use [Client setup](clients.md) for host-specific registration; installation and the broader CLI reference are in the [project README](../../README.md).

[Start and verify](#start) · [Project isolation](#projects) · [Assistant workflow](#workflow) · [17 tools](#tools) · [Examples](#examples) · [Budgets](#budgets) · [Confirmation](#confirmation) · [Storage and privacy](#privacy) · [Troubleshooting](#troubleshooting)

<a id="start"></a>
## 1. Start and verify

### Prepare the actual project

Choose the repository/workspace you intend to work on, not Loom's installation directory, your home directory, or a drive root. For a Git project, `git rev-parse --show-toplevel` identifies the repository root; for a non-Git project, use the actual workspace root. Resolve any ambiguity before binding.

The following is a **PowerShell example**. Replace both absolute paths with existing locations. `src/app.ts` is illustrative: use an existing source file in your project.

```powershell
& 'C:/Tools/Loom/loom.exe' --help
Set-Location 'D:/work/acme-api'
& 'C:/Tools/Loom/loom.exe' --root . stats
# Only for a genuinely unbound project:
& 'C:/Tools/Loom/loom.exe' --root . init
& 'C:/Tools/Loom/loom.exe' --root . index
& 'C:/Tools/Loom/loom.exe' --root . stats
& 'C:/Tools/Loom/loom.exe' --root . context src/app.ts --mode impact
```

- If the first `stats` already reports the expected project ID and root, skip `init`. Repeating `init` at the same valid root returns the same identity.
- Only an explicit `init` creates/binds the database. Queries, MCP startup, and `resume` do not silently create one. `index` updates an already bound project's structure snapshot.
- A missing database, identity mismatch, or unadopted legacy database is a repair case, not permission to create a replacement. Stop and follow the diagnostic.
- `loom guide` prints setup instructions without performing setup. `init` requires an explicit `--root`.

### Give the host an executable and separate arguments

These are launcher fields, **not a complete host configuration**; merge them using [Client setup](clients.md).

```json
{
  "command": "C:/Tools/Loom/loom.exe",
  "args": ["serve"]
}
```

Use the absolute path to the installed executable; on Linux/macOS, use an absolute binary path such as `/opt/loom/loom`. Do not put `serve` inside `command`, point `command` at a directory, or configure a URL/port. The host owns the subprocess and its stdin/stdout. Running `serve` by hand and seeing no prompt is normal: it is waiting for MCP messages, not accepting interactive CLI commands.

### Verify the connection, not just process startup

1. Connect or reconnect Loom in the host, then call the MCP tool `resume` with `{}`.
2. **Unbound:** only `resume` is exposed. Its diagnostic directs you to `loom guide` and includes the running executable's path. Nothing is initialized automatically. Complete the explicit CLI `init`/`index` steps in the intended project, then call `resume` again.
3. **Bound:** verify the returned **project ID and canonical root** before reading or writing project knowledge. The root must match the workspace. If the root is omitted because it cannot fit safely in the preview, verify it locally with `loom --root <absolute-project-root> stats` rather than guessing.
4. A successful binding exposes all **17 tools** in the [catalogue](#tools). The same process can discover the new binding and send `notifications/tools/list_changed`. If the host does not refresh its tool list, reconnect Loom or start a new session.

`resume` is an MCP tool, **not a CLI subcommand**. In a terminal, use `loom stats` for identity and `loom canvas list` for pending tasks. Do not substitute a fabricated CLI command for a missing tool.

<a id="projects"></a>
## 2. Project selection and isolation

Without `--root`, Loom searches **upward from the server process's launch working directory** for the nearest `.loom/project.id` or legacy `.loom/loom.db`. This is the host's launch directory, not necessarily the directory shown in a later assistant terminal. A running server does not follow subsequent shell `cd` commands; after validating an identity, it refuses a different project identity.

| Launch arguments | Selection and use |
|---|---|
| `["serve"]` | Reusable across projects only when the host launches a separate process in each project's directory. An unbound directory exposes only `resume`. |
| `["--root", "D:/work/acme-api", "serve"]` | Pins one process to that absolute project root; use when the host's working directory is uncertain. |
| One shared/global entry pinned to project A | Every workspace using it reads/writes A. Do not use this to serve unrelated projects. |

- **Subdirectories:** CLI commands and an unpinned server can find a parent binding. All tool file paths (`target`, `path`, `anchors`) remain relative to the **project root**, not the subdirectory: from `src/`, still use `src/app.ts`, not `app.ts`.
- **Nested projects:** the nearest binding wins; Loom does not skip a broken/legacy binding to use a more distant parent. To make a nested workspace independent, explicitly initialize its own root. A parent's structure scan excludes nested Loom projects.
- **Same names, different roots:** folder names are display labels, not identities. Independently initialized projects have separate IDs and databases, even if both are named `api`.
- **Clones and copies:** a fresh clone without `.loom/` can be initialized independently. Copying a bound `.loom/` into another root does not authorize reuse: a root/ID mismatch is rejected. A real relocation requires explicit `init --relocate <original-project-id>` after the old root is gone; it is not a way to bless a duplicate while the original still exists.
- **External database:** the project remembers its location; `serve` does not need an extra `--db`. An explicit `--db` overrides that location but must still pass the same project identity checks.

If the host reuses one subprocess across workspaces or starts it outside the intended project, use separate server instances with explicit roots. Verify each with `resume`; successful registration alone does not prove isolation.

<a id="workflow"></a>
## 3. Assistant workflow

`resume` restores **work state**: identity, pending tasks, next steps, evidence locations, recent memory titles, and review warnings. It is not a substitute for `overview`, which supplies **confirmed goals, constraints, and pinned memories**. At startup, call `overview` if no opening snapshot has been delivered. `refresh` explicitly reselects that overview; it neither reindexes source nor reviews file responsibilities.

The following optional block can be copied into a project's `AGENTS.md` or `CLAUDE.md`; it is guidance, not a registration/configuration file.

```markdown
## Loom workflow
- At startup, call the MCP tool resume. Verify its project ID and root; stop on a mismatch. If no opening snapshot was delivered, call overview for confirmed goals and constraints.
- If unbound, use loom guide and explicitly select the actual workspace root before CLI init/index. Never initialize a replacement for a missing or mismatched database. Ask before changing host registration.
- Before reading/editing a source file, call context with its project-root-relative target and mode=impact (files that reference it) or mode=explain (files it references). Read the cited source; preserve coverage gaps and uncertainty in conclusions.
- Refresh structure explicitly with index when requested by diagnostics and after a batch of source edits. Queries and refresh do not reindex.
- Before saving a lesson, use recall to check duplicates. Use remember only for verified rationale, pitfalls, or root causes not recoverable by just reading code; anchor it to relevant files. Inspect similar candidates before force=true; archive obsolete lessons rather than deleting them.
- Use canvas to record unfinished work: goal, concrete next step, and real evidence locations. Update it as work advances and close it when complete. Recent memories are not a task list.
- Use intent propose for proposed rules. Ask for explicit user confirmation before CLI confirmation/revocation/replacement, pinning, note review, evidence links, storage/config changes, model enablement, sharing/import, or irreversible deletion. A proposal and an assistant-written file role are not human approval.
- Never request credentials in prompts or put them in tool arguments, memory, or shell arguments. The user enters secrets locally with `loom config secret embedding` or `loom config secret rerank`, or supplies only an environment-variable name.
```

Treat `code_drift`, `orphaned`, `unverified`, and age warnings as reasons to inspect the current source, not proof that a memory is wrong or still valid. “Not found in this snapshot's managed scope” does not mean “absent from the project.”

<a id="tools"></a>
## 4. Public MCP tool catalogue — 17 tools

Names below are the server's public names; a host may display a namespace prefix. Arguments are JSON objects, not CLI strings. Do not add `--root` or `--db` to tool arguments: project selection belongs to the server launcher.

**Bold arguments are required.** Conditional requirements are spelled out per action. `Read` means no change to project knowledge; `Session` changes only the connection's delivered-snapshot state; `Write` persists changes. Optional model calls and `recall` vector-cache writes are called out separately below.

### Session and structure

| Tool | Arguments | Effect and use |
|---|---|---|
| `resume` | Optional `hours` (integer; default 24, clamped to 1–720) | Read. Verify identity and recover unfinished work; also diagnoses an unbound project. |
| `overview` | `{}` | Read + Session. Deliver confirmed goals, constraints, and pinned memories once per connection; later calls return the delivered snapshot ID. |
| `refresh` | `{}` | Read + Session. Reselect current overview and return a new snapshot. No source scan or responsibility review. |
| `context` | **`target`**, **`mode`** (`explain` or `impact`); optional `question`, `depth` (1–3, default 1), `budget` (1–6144, default 1500) | Read. Get constraints, file references in the selected direction, the recorded file role, memories, and coverage gaps before editing. `question` is space-separated keywords, at most 512 UTF-8 bytes; no model call. |
| `index` | `{}` | Write. Explicitly rebuild/update the structural snapshot; reports parsed, unchanged, and uncovered files. |
| `explain` | **`target`**; optional `depth` (1–3, default 1) | Read. Files referenced by the target, from the existing snapshot. |
| `impact` | **`target`**; optional `depth` (1–3, default 1) | Read. Files referencing the target, from the existing snapshot. |

### Memory

| Tool | Arguments | Effect and use |
|---|---|---|
| `remember` | **`type`**, **`title`**, **`body`**; optional `anchors` (string array), `tags` (string array), `importance` (1–5, default 2), `force` (default false) | Write. Title: one line, at most 120 characters; body: 8–2000 characters. Anchors use `src/a.rs` or `src/a.rs#Foo::bar`. Exact duplicates reuse the existing record; similar candidates require inspection before `force=true`. |
| `recall` | Optional `query` (default empty), `types` (string array), `top_k` (default 8, capped at 50), `full` (default false), `include_archived` (default false) | Read; may write vector cache when hybrid retrieval is enabled. Empty query lists recent memories without a model call. Otherwise use space-separated keywords; enabled projects may use keyword + vector retrieval and optional reranking. `full=true` requests full bodies instead of the default preview. |
| `manage` | **`action`**; `get`, `archive`, `activate`: supply `ids` (string array). `supersede`: supply old `ids` and new `by`. `delete`: supply `ids` and `confirm=true`. `stats`: no IDs needed. | Read for `get`/`stats`; Write for status changes/deletion. Get full records and anchors; prefer archive to irreversible deletion. Use actual IDs returned by Loom. |

`type` / `types` values: `DECISION`, `BUGFIX`, `GOTCHA`, `ERROR_PATTERN`, `CONFIG`, `FEATURE`, `MILESTONE`, `PROGRESS`, `CODE_PATTERN`, `SYNTHESIS`.

### Intent, file responsibilities, tasks, and evidence

| Tool | Arguments | Effect and use |
|---|---|---|
| `intent` | **`action`**: `list` needs no other fields; `get` needs `id`; `propose` needs `kind`, `title`, `body`. `kind`: `goal`, `constraint`, `non_goal`, `decision`. | Read for list/get; Write for propose. A proposal is inactive. Confirmation, revocation, replacement, and pinning are not exposed through MCP. |
| `note` | **`path`**; optional `action` (`get` by default, or `set`). `set` additionally requires `expected_sha` and `role`. | Read for get; Write for set. Inspect the file and current full SHA-256 from `note get` before set; the server does not supply approval or silently fill the fingerprint. Human re-review uses CLI `note review`. |
| `canvas` | **`action`**: `list` needs nothing else; `get`/`close` need `id`; `open` needs `goal`, `next`, `evidence`; `update` needs `id`, `next`, `evidence`. All supplied values are strings. | Read for list/get; Write for open/update/close. `evidence` records a real evidence location; it does not fabricate or collect evidence. Close preserves history. |
| `evidence` | **`query`** | Read. Package direct records, explicitly declared support/conflict links, review warnings, and gaps. Does not infer or write relationships. |

For `intent`, `note`, and `canvas`, send only fields for the chosen action; for example, do not attach `id` to `canvas open` or `role` to `note get`.

### Optional local enhancement tools

| Tool | Arguments | Effect and use |
|---|---|---|
| `semantic` | **`query`** | Read. Search only the project's individually authorized active-memory titles, at most 12. Without a model, keyword fallback stays inside the same set. Cannot grant/revoke consent. |
| `rerank` | **`query`** | Read. Rerank at most 12 keyword-candidate titles; unavailable local enhancement preserves keyword order. This is not whole-library semantic recall. |
| `extract` | **`text`** | Read. Extract reviewable suggestions from only the explicitly submitted text; never writes memories or confirms intent. A disabled/unavailable model is distinguished from no suggestions. |

These three tools are separate from the project-level hybrid `recall` pipeline; see [privacy and model consent](#privacy).

**Schema sources:** [public tool declarations and main argument types](../../crates/loom-mcp/src/lib.rs), [action-specific arguments and validation](../../crates/loom-mcp/src/knowledge.rs), [binding, discovery, and session behaviour](../../crates/loom-mcp/src/session.rs). The connected server's `tools/list` is authoritative for the executable actually running; an old executable may expose fewer tools.

<a id="examples"></a>
## 5. Host-independent tool calls

These are the `name` and `arguments` objects for MCP `tools/call`, not terminal commands or a particular host's wrapper syntax. All six examples request information rather than changes to project knowledge; hybrid `recall` can still update its vector cache. Paths below exist in the **Loom source repository**; substitute an existing root-relative file when using another project. No match from a query is a valid result.

Verify identity and recover work:

```json
{"name":"resume","arguments":{}}
```

Get startup constraints if no opening snapshot was delivered:

```json
{"name":"overview","arguments":{}}
```

Inspect the impact of editing Loom's MCP implementation, allowing a larger byte budget:

```json
{"name":"context","arguments":{"target":"crates/loom-mcp/src/lib.rs","mode":"impact","depth":1,"budget":3000}}
```

Check for an existing lesson before writing one:

```json
{"name":"recall","arguments":{"query":"project identity","types":["DECISION","GOTCHA"],"top_k":5}}
```

Inspect a file's recorded role and fingerprint without changing it:

```json
{"name":"note","arguments":{"path":"crates/loom-core/src/project.rs"}}
```

Read the unfinished-task list rather than treating recent memories as tasks:

```json
{"name":"canvas","arguments":{"action":"list"}}
```

<a id="budgets"></a>
## 6. Output budgets and evidence limits

Budgets count **UTF-8 bytes**, not tokens or characters, and cover the complete bounded text, including IDs and omission notices.

| Output | Limit |
|---|---:|
| Opt-in automatic opening snapshot | 800 bytes |
| `overview`, `refresh` | 2000 bytes each |
| Default previews from `recall`, `resume`, `explain`, `impact` | 1500 bytes each |
| MCP previews from `evidence`, `semantic`, `rerank`, `extract` | 1500 bytes each |
| `context` | Default 1500; `budget` range 1–6144 bytes |

Automatic opening text is sent only when the host negotiates `capabilities.experimental["loom/overview"].textBudget = "utf8-byte-upper-bound"`. Other hosts should explicitly call `overview` at startup. A stdio connection is the session boundary; repeating `overview` does not resend its body. Use `refresh` to obtain a newly selected snapshot; it cannot erase old statements already in the host's conversation.

Previews omit complete lines and report omissions rather than cut evidence in half. To investigate further, narrow the target/depth, use `manage get` with IDs or `recall` with `full=true`, or run CLI `explain`/`impact`/`context` with `--json` for complete structural evidence. If a `context` budget cannot fit mandatory diagnostics, increase it to the minimum reported, up to 6144.

Structural results are static evidence, not a complete runtime call graph. They report snapshot generation, certainty, and coverage gaps. Queries do not refresh the snapshot; bounded live checks are not a whole-project scan, and newly added files can remain invisible until `index`. Do not turn an omitted, uncovered, unresolved, or out-of-scope result into a claim of no dependencies.

<a id="confirmation"></a>
## 7. Confirmation and CLI-only operations

An assistant can read evidence, write lessons/tasks, refresh the index, and propose intent through MCP. These permissions do **not** turn a proposal into a confirmed rule or a file-role edit into human review.

The following operations are **not MCP tools**. Obtain explicit user approval before an authority-changing, data-moving, model-enabling, or sharing action; then a user or an authorized assistant can execute the CLI command. Preview commands alone do not apply changes.

| Boundary | CLI entry points |
|---|---|
| Bind/adopt/relocate a project | `loom --root <root> init`; use `--adopt-legacy` or `--relocate <id>` only after verifying ownership/movement. Ordinary first-time init has no `--yes` flag. |
| Confirm/revoke/replace intent; pin/unpin memory | `loom intent confirm <id> --yes`; `revoke <id> --yes`; `replace <old-id> <proposal-id> --yes`; `pin <memory-id> --yes`; `unpin <memory-id> --yes` |
| Re-review a file responsibility | `loom note review <path> <current-full-sha256> --yes` |
| Declare evidence relationships | `loom evidence link <from-id> <to-id> <supports\|conflicts> --yes` |
| Move storage or change machine settings | `loom db move <destination>` previews; `--yes` applies. `loom config set …` / `config secret …` are CLI-only; see `loom guide storage`. |
| Configure/enable model use | `loom guide models`; `loom semantic enable` previews, `loom semantic enable --yes` authorizes and builds vectors. `semantic disable --yes`, `semantic index`, `semantic allow <id> --yes`, `semantic deny <id> --yes` are CLI-only. |
| Import or share | `loom import <wiki-directory>` previews, `--apply` writes. `loom share preview`, `share export`, and `share import` provide reviewed-content flows; inspect their `--help` before applying. |

`manage delete` **is** exposed over MCP, but is irreversible and requires `confirm=true` after explicit user approval. That boolean is a guard, not proof that the user approved. Archive when the goal is simply to retire an old lesson. Model testing/downloading and host registration are also outside the 17 tools; do not run them as an automatic connection check.

<a id="privacy"></a>
## 8. Storage, model consent, and practical privacy

| Location | Purpose |
|---|---|
| `<root>/.loom/project.id` | Project ID, checked against the database and canonical root before content is loaded. |
| `<root>/.loom/loom.db` | Default SQLite database: memories, intent, roles, tasks, evidence links, structure snapshots, and optional vectors. |
| `<root>/.loom/database.path` | Absolute database location when stored elsewhere. Keep project metadata with the project. |
| `<root>/.loom/ignore` | Optional extra source exclusions; can exclude, not re-include. |
| User `config.toml` / `credentials.toml` | Machine-local model/storage preferences and credentials; separate from project data. `loom config path` reports the configuration location. |

Keep `.loom/` out of Git and out of unreviewed uploads; use repository ignore rules or a local Git exclude. `loom stats` reports the selected database path and size. Do not move live SQLite/WAL files manually: use `loom db move` after approval. For a file-copy backup, stop sessions using the project first and preserve both its database and `.loom` metadata. Avoid network shares and cloud-synchronized database locations.

### Default: local and no model required

With optional model features disabled, Loom's local memory and structural operations need no network/model service. The index stores paths, symbol names, import/include spellings, and line numbers rather than a copy of source values. Exclude private source paths explicitly when necessary. Memory, role, and task text can still contain internal information. Data returned to an assistant is also subject to **that host/model's** handling; local Loom storage does not make a cloud assistant local.

### Optional project-level hybrid retrieval

After model configuration, `loom semantic enable` previews endpoints, send scope, and estimated vector storage without sending model requests. Only after approval should `loom semantic enable --yes` authorize the project and build vectors.

- `recall` may send a redacted query and active memories' redacted **titles and bodies** to the configured local or cloud endpoint: at most 2000 characters per memory document; optional reranking considers at most 24 candidate documents. It can incrementally write missing vectors to the local database. Empty-query recent listings do not call a model.
- IDs, anchors, paths, tags, project identifiers/roots, source, intent, roles, and task canvases are not added as separate payload fields. **They may still appear in user-authored text.** Redaction is not an internal-information detector: review text and the endpoint's retention policy before consenting.
- Consent is project-specific and bound to configured endpoint origins. Host/port changes require renewed approval; a model-name change requires rebuilding old vectors. A reranking-origin mismatch prevents that reranking call; it does not discard a successful vector retrieval.
- **Embedding failure:** keyword fallback with a reason. **Reranking failure after successful vector retrieval:** retain the keyword + vector fusion order and report the reranking failure, rather than fall all the way back to keywords.

### Separate, bounded local enhancements

The MCP tools `semantic` and `rerank` use the separately enabled loopback enhancement (`LOOM_EMBED_MODEL`); `extract` uses `LOOM_EXTRACT_MODEL`. Both are off by default and use `127.0.0.1` with `LOOM_MODEL_PORT` (default 11434). They do not inherit the project hybrid endpoint configuration. `semantic` requires per-memory CLI grants, at most 12 active titles; `rerank` sees at most 12 keyword-candidate titles; `extract` sees only the submitted redacted short text. Their fallbacks and scopes are described in the [catalogue](#tools).

Never put credentials in prompts, tool arguments, memories, shared exports, or command-line arguments. Have the user enter them locally using `loom config secret embedding` or `loom config secret rerank`, or supply only the name of a credential-bearing environment variable. Do not ask an assistant to inspect credential files to diagnose a connection.

<a id="troubleshooting"></a>
## 9. Errors and troubleshooting

CLI failures have a nonzero exit status and diagnostics on stderr. MCP tool failures are not successful empty results; inspect the error text and any omissions/fallback heading before proceeding.

| Symptom | Action |
|---|---|
| No Loom tools in the host | Check the host's server status/stderr, the absolute executable path, separate `serve` argument, and any pending trust approval. Reconnect after configuration changes. Use [Client setup](clients.md), not a web-server URL. |
| Only `resume`; project is unbound | Verify the actual project root, run explicit CLI `init` and `index` there, then call `resume`. Reconnect if the tool-list notification was not reflected. |
| Wrong root or another project's memories | Stop calls. Check pinned `--root` and the host's process launch cwd. Correct selection, reconnect, and verify `resume` before continuing. |
| Missing database, ID mismatch, or legacy binding error | Preserve `.loom/` and the database. Follow the exact repair/adoption diagnostic; do not delete markers or initialize a replacement. If a database was moved manually, verify it before the diagnostic's explicit `init --db <new-path>` reassociation. |
| Fewer than 17 tools; `context`/`guide` is unavailable | Check `--help` on the **same absolute executable** used by the host. An old build or another PATH entry may have the same version label; replace/select the intended build and restart its server processes. |
| Snapshot version mismatch, changed files, or request to index | Call MCP `index` (or CLI `loom index`), then repeat the query. `refresh` only refreshes the overview. |
| Target outside managed scope; no structural match | Check root-relative spelling and ignore rules. Index new files; preserve uncovered/unresolved warnings. Narrow the query or inspect CLI `--json` instead of claiming absence. |
| `context` budget too small; keyword question misses | Use the reported minimum budget, at most 6144. Use space-separated keywords; a whole unspaced sentence is one keyword. |
| Similar memory; memory rejected as code restatement | Read candidate IDs using `manage get`; reuse/supersede or deliberately force only after inspection. Save verified background, conclusion, and reason, not a paraphrase of source. |
| `note set` fingerprint is stale | Inspect the current file and `note get` fingerprint again; index if needed. Do not invent a SHA or silently turn a write into human review. |
| Model fallback or endpoint-consent warning | Read the reported reason and check `loom semantic status`. Review the new endpoint before renewed consent. Run `loom model test` only when authorized—it sends requests. Reranking-only failure keeps successful hybrid fusion. |
| Storage move reports active use | Wait for the current call to finish or close older server processes. Current servers release their database connection between calls; do not manually remove lock/WAL files. |
| Canonical Windows root starts with `\\?\` | This is Windows' normalized path notation, not a different workspace. Compare the actual normalized location. |

[Back to tools](#tools) · [Assistant workflow](#workflow) · [Client setup](clients.md) · [Project README](../../README.md)
