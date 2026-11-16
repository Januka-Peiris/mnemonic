# Team Agent In Place, Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A teammate (Dan) starts a chat in a sharer's (Jay's) shared workspace; Dan's Claude runs on Dan's Mac with Dan's login, and every file read, search, edit and shell command it makes happens in Jay's real workspace folder on Jay's Mac.

**Architecture:** Jay's share server gains an MCP-over-HTTP endpoint per shared workspace that implements Read, Write, Edit, Glob, Grep and Bash against the workspace root. On Dan's side, a chat started from Jay's mirrored workspace runs the normal `claude-acp` agent with its built-in file and shell tools disallowed and that endpoint passed as an `McpServer::Http`, through a per-session extras registry in `agent_servers`. Approvals stay on Dan's side in this phase (phase 2 relays them to Jay; phase 3 shows Dan's chat to Jay).

**Tech Stack:** Rust, GPUI, `tiny_http`, `reqwest` (blocking), `agent-client-protocol` schema 0.13, `ignore`/`globset`/`regex` crates, claude-acp adapter v0.107.2.

**Spec:** `docs/superpowers/specs/2026-11-16-team-agent-in-place-design.md`

## Global Constraints

- Protocol bumps to `TEAM_PROTOCOL_VERSION = 5`; no compatibility with v4.
- Every new route sits behind the existing `check_request` (no `Origin`, allowed `Host`, JSON body, `X-Mnemonic-Team` header on POST) and the `team.allow` whois check.
- MCP server name passed to Claude: `teammate_workspace` (the name `workspace` is reserved by Claude and silently dropped). Tool names Claude sees: `mcp__teammate_workspace__Read`, `__Write`, `__Edit`, `__Glob`, `__Grep`, `__Bash`.
- Built-ins to disallow, passed as `_meta.claudeCode.options.disallowedTools`: `Read, Write, Edit, MultiEdit, NotebookEdit, Bash, BashOutput, KillShell, Monitor, PowerShell, Grep, Glob, LS`.
- Read tools pre-allowed via `_meta.claudeCode.options.allowedTools`: `mcp__teammate_workspace__Read, mcp__teammate_workspace__Glob, mcp__teammate_workspace__Grep`.
- System prompt addition via `_meta.systemPrompt = {"append": "<text>"}` (the adapter maps an object to the `claude_code` preset plus `append`).
- Bash: default timeout 120000 ms, maximum 600000 ms, output truncated to 30000 characters, cwd = workspace root, runs as Jay's user through `$SHELL -l -c`, with `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` set from Dan's git identity.
- Read: default 2000 lines, lines longer than 2000 characters truncated, `cat -n` style numbering (`{line_number}\t{text}`), binary files refused.
- New switch label: "Teammates' agents can work here". On by default when a workspace is shared, and turned on for workspaces already shared when this version first runs.
- No em dashes anywhere (code, strings, commits). Use `./script/clippy`, not `cargo clippy`. No `unwrap()` outside tests; no `let _ =` on fallible calls.

## Review Focus

- A long command (`cargo test` for several minutes): the HTTP request must stay open until the command ends or hits its timeout, and the result must say it timed out, not vanish.
- Paths: the agent will pass absolute paths in Jay's folder, sometimes relative ones, sometimes paths outside it, and this repo's `CLAUDE.md` is a symlink to `.rules`. Inside-root symlinks read fine; anything resolving outside the root, or writes through a symlink or into `.git`, are refused with a reason.
- Jay has unsaved edits to a file in his editor: Write/Edit on it is refused with "has unsaved changes in <Name>'s editor", and the file on disk is untouched.
- Dan presses Stop while a command runs: Claude sends `notifications/cancelled`; the command's process group on Jay's Mac is killed.
- Jay's Mac is offline or the switch is off: tool calls return an MCP error result with a readable reason, and Dan's chat stays usable.

---

### Task 1: Agent tool implementations

**Files:**
- Create: `crates/mnemonic-core/src/services/team_agent_tools.rs`
- Modify: `crates/mnemonic-core/src/services/mod.rs` (add `pub mod team_agent_tools;`)
- Modify: `crates/mnemonic-core/Cargo.toml` (add `ignore.workspace = true`, `globset.workspace = true`, `regex.workspace = true`)

**Interfaces:**
- Produces:
  - `pub struct AgentToolContext { pub root: PathBuf, pub git_author: Option<GitAuthor>, pub can_write: Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync> }` where `can_write` is asked before every Write/Edit with the absolute path and returns `Err(reason)` to refuse.
  - `pub struct GitAuthor { pub name: String, pub email: String }`
  - `pub fn resolve_agent_path(root: &Path, path: &str, for_write: bool) -> Result<PathBuf, String>`
  - `pub fn read(context: &AgentToolContext, file_path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String, String>`
  - `pub fn write(context: &AgentToolContext, file_path: &str, content: &str) -> Result<String, String>`
  - `pub fn edit(context: &AgentToolContext, file_path: &str, old_string: &str, new_string: &str, replace_all: bool) -> Result<String, String>`
  - `pub fn glob(context: &AgentToolContext, pattern: &str, path: Option<&str>) -> Result<String, String>`
  - `pub fn grep(context: &AgentToolContext, request: &GrepRequest) -> Result<String, String>` with `pub struct GrepRequest { pub pattern: String, pub path: Option<String>, pub glob: Option<String>, pub output_mode: GrepOutputMode, pub case_insensitive: bool, pub line_numbers: bool, pub head_limit: Option<usize> }` and `pub enum GrepOutputMode { FilesWithMatches, Content, Count }` (default `FilesWithMatches`).
  - `pub fn bash(context: &AgentToolContext, command: &str, timeout: Option<Duration>, running: &RunningCommands, call_key: &str) -> Result<String, String>`
  - `pub struct RunningCommands` (`Clone`, `Default`): `pub fn kill(&self, call_key: &str) -> bool`. `bash` registers its child's process group under `call_key` while it runs.

- [ ] **Step 1: Write the failing tests** (in `#[cfg(test)] mod tests`, each on a `tempfile`-style temp dir the test creates under `std::env::temp_dir()` with a unique name)

```rust
#[test] fn paths_resolve_inside_the_root_only() {
    // root contains a.txt, sub/b.txt, link -> a.txt (inside), out -> /etc (outside)
    assert!(resolve_agent_path(&root, root.join("a.txt").to_str()?, false).is_ok());
    assert!(resolve_agent_path(&root, "sub/b.txt", false).is_ok());
    assert!(resolve_agent_path(&root, "link", false).is_ok());       // symlink inside root: readable
    assert!(resolve_agent_path(&root, "link", true).is_err());       // never written through
    assert!(resolve_agent_path(&root, "out/passwd", false).is_err());
    assert!(resolve_agent_path(&root, "../x", false).is_err());
    assert!(resolve_agent_path(&root, "/etc/passwd", false).is_err());
    assert!(resolve_agent_path(&root, ".git/config", true).is_err());
}
#[test] fn read_numbers_lines_and_honours_offset_and_limit() {
    // file "one\ntwo\nthree\n"
    assert_eq!(read(&context, "f.txt", Some(2), Some(1))?, "2\ttwo");
}
#[test] fn read_refuses_binary_files() { /* file with a NUL byte */ assert!(read(&context, "bin", None, None)?.is_err()...) }
#[test] fn edit_requires_a_unique_match_unless_replace_all() {
    // "a a" : edit(old "a", new "b", false) is Err containing "2 matches"; with replace_all -> "b b"
}
#[test] fn write_and_edit_ask_can_write_first() {
    // can_write returns Err("has unsaved changes in Jay's editor") -> write() is Err with that text, file unchanged
}
#[test] fn grep_and_glob_skip_gitignored_files() {
    // .gitignore lists "target"; target/x.rs and src/y.rs both contain "needle"
    // grep files_with_matches -> lists src/y.rs only; glob("**/*.rs") -> src/y.rs only
}
#[test] fn bash_runs_in_the_root_with_the_git_author() {
    // command `pwd && echo $GIT_AUTHOR_NAME` -> output contains root path and "Dan"
}
#[test] fn bash_reports_a_timeout() {
    // `sleep 5` with timeout 200ms -> Err or Ok text containing "timed out after 0.2s"
}
#[test] fn killing_a_running_command_stops_it() {
    // spawn bash(`sleep 30`) on a thread with call_key "k"; after 200ms running.kill("k") == true; thread returns within 2s
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p mnemonic-core --lib team_agent_tools`
Expected: compile errors for the missing module and functions.

- [ ] **Step 3: Implement the module**

Absolute paths must start with the root; strip it, then resolve. Reads canonicalize and require the result under the canonical root; writes use `team_files_service::safe_relative_path` (no symlinks, no `.git`). Glob and Grep walk with `ignore::WalkBuilder` (respects `.gitignore`, skips `.git`), match with `globset`/`regex`, return paths relative to the root, newest first for Glob, at most 100 Glob results and `head_limit` lines for Grep. Bash spawns `$SHELL` (fall back to `/bin/zsh`) with `-l -c <command>` in its own process group (`std::os::unix::process::CommandExt::process_group(0)`), reads stdout and stderr on threads, waits with the timeout, kills the group on timeout or `kill`, and returns `"exit {code}\n{output}"` truncated to 30000 characters with a note when cut. Use `#[allow(clippy::disallowed_methods)]` on the spawn as `team_share_server::is_git_tracked` does.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mnemonic-core --lib team_agent_tools`
Expected: all pass.

- [ ] **Step 5: Commit** `Add the tools a teammate's agent uses in a shared workspace`

### Task 2: MCP endpoint logic

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_agent_tools.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces:
  - `pub const AGENT_MCP_SERVER_NAME: &str = "teammate_workspace";`
  - `pub enum McpReply { Json(String), Accepted }` (Accepted maps to HTTP 202 with no body).
  - `pub fn handle_mcp_message(context: &AgentToolContext, running: &RunningCommands, caller_login: &str, body: &[u8]) -> McpReply`
  - Call keys are `format!("{caller_login}:{request_id}")`, so `notifications/cancelled` with `params.requestId` kills that caller's command.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn mcp_initialize_echoes_the_protocol_version() {
    // body {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
    // -> Json with result.protocolVersion == "2025-06-18", result.capabilities.tools present, serverInfo.name == "teammate_workspace"
}
#[test] fn mcp_lists_the_six_tools_with_claude_shaped_inputs() {
    // names == ["Read","Write","Edit","Glob","Grep","Bash"]; Read schema requires ["file_path"]; Edit requires file_path, old_string, new_string
}
#[test] fn mcp_tool_call_returns_text_or_an_error_result() {
    // tools/call Read existing file -> result.content[0].text contains "1\t", isError false
    // tools/call Read "/etc/passwd" -> isError true, text explains it is outside the workspace
}
#[test] fn mcp_notifications_are_accepted_without_a_body() { /* notifications/initialized -> McpReply::Accepted */ }
#[test] fn mcp_cancel_kills_the_callers_command() { /* tools/call Bash sleep 30 id 7 on a thread; then notifications/cancelled requestId 7 from same login; thread returns quickly */ }
```

- [ ] **Step 2: Run to see them fail** - `cargo test -p mnemonic-core --lib team_agent_tools::tests::mcp` - compile errors.
- [ ] **Step 3: Implement `handle_mcp_message`** - JSON-RPC 2.0; methods `initialize`, `ping`, `tools/list`, `tools/call`; unknown method -> error `-32601`; malformed JSON -> error `-32700`. Tool input schemas mirror Claude's built-ins (Read: `file_path`, `offset`, `limit`; Write: `file_path`, `content`; Edit: `file_path`, `old_string`, `new_string`, `replace_all`; Glob: `pattern`, `path`; Grep: `pattern`, `path`, `glob`, `output_mode`, `-i`, `-n`, `head_limit`; Bash: `command`, `timeout`, `description`). Descriptions say paths are absolute paths in the shared workspace.
- [ ] **Step 4: Run the tests** - all pass.
- [ ] **Step 5: Commit** `Answer MCP requests for a teammate's agent`

### Task 3: Serve the endpoint from the share server

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_share_server.rs` (route, config, tests)
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (`TEAM_PROTOCOL_VERSION = 5`, `TeamWorkspace` fields, header const, client helper)

**Interfaces:**
- Consumes: Task 2.
- Produces:
  - `TeamWorkspace { .., #[serde(default)] pub agents: bool, #[serde(default)] pub root_path: Option<String> }` (`root_path` set only when `agents`).
  - `pub type AgentRootSource = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;` (root when the agents switch is on).
  - `pub type AgentWriteCheck = Arc<dyn Fn(&str, &Path) -> Result<(), String> + Send + Sync>;` (workspace id, absolute path).
  - `TeamServerConfig { .., pub agent_roots: AgentRootSource, pub agent_write_check: AgentWriteCheck }`
  - `pub const TEAM_GIT_AUTHOR_HEADER: &str = "X-Mnemonic-Git-Author";` value `Name <email>`.
  - Route: `POST /v1/workspaces/{id}/agent` -> `handle_mcp_message`; `GET` on the same path -> 405 (no SSE stream). 403 "Teammates' agents can't work in this workspace" when `agents` is off or no root.
  - `pub fn agent_mcp_url(base_url: &str, workspace_id: &str) -> String`
  - `TeamClient::agent_read(&self, workspace_id: &str, absolute_path: &str) -> Result<Option<String>, TeamClientError>` (a `tools/call Read` through the endpoint; `Ok(None)` when the file is missing).

- [ ] **Step 1: Write the failing tests** in the existing `tests` module (reuse `start_with_handler`, `workspace`, `raw_status`):

```rust
#[test] fn agent_endpoint_answers_mcp_when_agents_are_on() { /* POST initialize with JSON + X-Mnemonic-Team -> 200 and a JSON result */ }
#[test] fn agent_endpoint_refuses_when_agents_are_off() { /* workspace.agents = false -> 403 */ }
#[test] fn agent_endpoint_refuses_browser_requests() { /* Origin header -> 403; missing X-Mnemonic-Team -> 400 */ }
#[test] fn agent_endpoint_refuses_callers_off_the_allow_list() { /* FixedLogin(Some("stranger")) -> 403 */ }
#[test] fn agent_writes_ask_the_write_check() { /* write check returns Err("unsaved") -> tools/call Write isError true containing "unsaved", file absent */ }
```

- [ ] **Step 2: Run to see them fail** - `cargo test -p mnemonic-core --lib team_share_server` - compile errors.
- [ ] **Step 3: Implement** - read the body up to `MAX_ACTION_BODY_BYTES * 64` (1 MiB; Write carries whole files), build `AgentToolContext` per request (root from `agent_roots`, `can_write` wrapping `agent_write_check` with the workspace id, `git_author` parsed from the header), keep one `RunningCommands` in `Shared`. `McpReply::Accepted` -> 202 empty. Update `TeamServerConfig` construction sites and the docs comment at the top of the file (writes and commands now exist, behind the agents switch).
- [ ] **Step 4: Run the tests** - `cargo test -p mnemonic-core --lib team_share_server` - all pass.
- [ ] **Step 5: Commit** `Serve a teammate's agent tools from the share server`

### Task 4: Jay's side: the switch, roots and the unsaved-edits check

**Files:**
- Modify: `crates/mnemonic-panels/src/team.rs`
- Modify: `crates/mnemonic-panels/src/team_settings.rs` (checkbox)
- Modify: `crates/mnemonic-panels/src/workspace_panel.rs` (sidebar menu entry next to the step-in and share-files entries, ~line 2060)

**Interfaces:**
- Consumes: Task 3 config fields.
- Produces: `pub(crate) fn workspace_allows_agents(workspace_id: &str, cx: &App) -> bool`, `pub(crate) fn set_workspace_agents(workspace_id: &str, allowed: bool, cx: &mut App)`; settings key `AGENT_WORKSPACES_KEY = "team_agent_workspaces"`.

- [ ] **Step 1: Write the failing tests** (next to `sharing_turns_on_step_in_files_and_edits`)

```rust
#[gpui::test] async fn sharing_turns_on_teammates_agents(cx) { /* set_workspace_shared(true) -> allows_agents("workspace") */ }
#[gpui::test] async fn unsharing_turns_off_teammates_agents(cx) { /* then set_workspace_shared(false) -> !allows_agents */ }
#[test] fn agents_start_on_for_workspaces_already_shared() {
    // initial_agent_workspaces(saved: None, shared: {"a","b"}) == {"a","b"}; with saved Some({"a"}) == {"a"}
}
```

- [ ] **Step 2: Run to see them fail** - `cargo test -p mnemonic-panels --lib team::tests` - compile errors.
- [ ] **Step 3: Implement** - `agent_workspaces: HashSet<String>` on `TeamShare`, loaded through `fn initial_agent_workspaces(saved: Option<HashSet<String>>, shared: &HashSet<String>) -> HashSet<String>` (a missing key means first run: copy the shared set and save it). Add it to the set loop in `set_workspace_shared` and remove it on unshare. In `publish`, set `agents` and `root_path` from `workspace_root(&summary)`, and fill an `agent_roots` map (same `Arc<Mutex<HashMap>>` pattern as `file_roots`). `agent_write_check` crosses to the foreground exactly as `action_handler` does (`PendingAction`-style channel, `wait_for_reply` with `ACTION_TIMEOUT`) and refuses when `unsaved_paths(root, cx)` contains the path, with `"{path} has unsaved changes in {display_name}'s editor"`.
- [ ] **Step 4: Run the tests** - all pass.
- [ ] **Step 5: Commit** `Let a sharer allow teammates' agents in a workspace`

### Task 5: Per-session extras in agent_servers

**Files:**
- Modify: `crates/agent_servers/src/acp.rs`

**Interfaces:**
- Produces:
  - `pub struct SessionExtras { pub mcp_servers: Vec<acp::McpServer>, pub meta: acp::Meta }`
  - `pub fn set_session_extras(cwd: PathBuf, extras: SessionExtras, cx: &mut App)` and `pub fn clear_session_extras(cwd: &Path, cx: &mut App)`, stored in a GPUI `Global` keyed by cwd.
  - Applied in `new_session`, `load_session`, `resume_session` and `fork_session`: the extras' MCP servers are appended to `mcp_servers_for_project(..)` and `meta` set on the request, when the session's cwd (first of `work_dirs`) has extras.

- [ ] **Step 1: Write the failing test** in `acp.rs` tests: `fn session_requests_carry_extras_for_their_cwd()` builds `SessionDirectories { cwd: "/tmp/x".into(), .. }` and asserts the new, load and resume requests built through the new helper `fn with_extras<R>(request: R, extras: Option<&SessionExtras>) -> R` (or per-type equivalents) contain the extra server named `teammate_workspace` and `meta["claudeCode"]["options"]["disallowedTools"]`; a different cwd gets neither.
- [ ] **Step 2: Run to see it fail** - `cargo test -p agent_servers --lib session_requests_carry_extras` - compile error.
- [ ] **Step 3: Implement** - look the extras up synchronously where `mcp_servers_for_project` is called (1646, 1783, 1825, 1872) and pass them into the `into_*_request` builders.
- [ ] **Step 4: Run** - passes; `cargo test -p agent_servers` still passes.
- [ ] **Step 5: Commit** `Let Mnemonic add MCP servers and options to one folder's agent sessions`

### Task 6: Dan's side: chats in a teammate's workspace

**Files:**
- Create: `crates/mnemonic-panels/src/team_live_chat.rs` (records, scratch folder, session extras, accept-edits answering)
- Modify: `crates/mnemonic-panels/src/lib.rs` (module)
- Modify: `crates/mnemonic-panels/src/team_workspace_view.rs` (action, chat items)

**Interfaces:**
- Consumes: Task 3 (`agent_mcp_url`, `TEAM_GIT_AUTHOR_HEADER`, `TeamClient::agent_read`, `TeamWorkspace.agents/root_path`), Task 5 (`set_session_extras`).
- Produces:
  - `pub(crate) struct LiveChatRecord { pub id: String, pub host: String, pub workspace_id: String, pub title: String, pub acp_session_id: Option<String> }`, persisted as JSON in `{data_dir}/team_live_chats.json` via `load_live_chats(data_dir: &Path) -> Vec<LiveChatRecord>` and `save_live_chats(data_dir: &Path, records: &[LiveChatRecord]) -> Result<(), String>`.
  - `pub(crate) fn live_chat_dir(data_dir: &Path, host: &str, workspace_id: &str) -> PathBuf` -> `{data_dir}/team_live/{host_slug}/{workspace_id}`, created empty.
  - `pub(crate) fn live_session_extras(shared: &TeamWorkspace, sharer_name: &str, base_url: &str, git_author: Option<&GitAuthor>, instructions: &str) -> Option<SessionExtras>` (None unless `agents` and `root_path`).

- [ ] **Step 1: Write the failing tests** in `team_live_chat.rs`:

```rust
#[test] fn live_session_extras_swap_claudes_file_and_shell_tools() {
    // extras.mcp_servers == [Http "teammate_workspace" at "{base}/v1/workspaces/w1/agent"] with headers X-Mnemonic-Team and X-Mnemonic-Git-Author "Dan <dan@x>"
    // meta.claudeCode.options.disallowedTools == the eleven built-ins; allowedTools == the three read tools
    // meta.systemPrompt.append contains "Jay's Mac", the root path, and the instructions text
}
#[test] fn no_extras_when_the_sharer_disallows_agents() { /* agents false -> None */ }
#[test] fn live_chat_records_round_trip() { /* save then load equals input */ }
```

- [ ] **Step 2: Run to see them fail** - `cargo test -p mnemonic-panels --lib team_live_chat` - compile errors.
- [ ] **Step 3: Implement the extras and records.** The system prompt addition says: this workspace lives on `{sharer_name}`'s Mac at `{root_path}`; use only the `mcp__teammate_workspace__*` tools for files and commands, with absolute paths under that root; commands run on `{sharer_name}`'s Mac; then the contents of `CLAUDE.md` and `AGENTS.md` from the root (fetched with `TeamClient::agent_read` on a background thread before the session starts, skipped when missing).
- [ ] **Step 4: Run the tests** - pass.
- [ ] **Step 5: Wire the view.** In `TeammateWorkspaceView::read_only_presentation`, when `live.agents`, the action is `"New chat in {peer}'s workspace"` (keep "Work on this in your copy" reachable when `share_files` is also on, as a second action or in the banner). Starting one: write a `LiveChatRecord`, create `live_chat_dir`, fetch instructions, `set_session_extras(dir, extras, cx)`, and add a chat item built like `WorkspaceView::build_chat` (`workspace_view.rs:2724-2772`): `Agent::Custom { id: "claude-acp" }`, the view's own `AgentConnectionStore`, `work_dirs: Some(PathList::new(&[dir]))`, `resume_session_id` from the record, not read-only. Save `acp_session_id` when the thread reports it, as `workspace_view.rs:2911` does. Load existing records for this (host, workspace) when the view opens, from `last_known` when Jay is offline. Subscribe to the thread: on `AcpThreadEvent::ToolAuthorizationRequested`, when the tool call's title is `mcp__teammate_workspace__Edit` or `__Write` and the session's current mode id is `acceptEdits`, answer with the allow-once option through `AcpThread::authorize_tool_call`.
- [ ] **Step 6: Build and clippy** - `cargo build -p mnemonic-panels` and `./script/clippy -p mnemonic-panels`; no new warnings in touched files.
- [ ] **Step 7: Commit** `Start a chat in a teammate's workspace with your own agent`

### Task 7: End-to-end check on one Mac

Your own Tailscale login is always allowed, so one Mac can follow itself.

- [ ] **Step 1:** Run the app (`cargo run`), share a workspace with Tailscale on, add this machine as a teammate in the Team window, open the mirrored workspace, start "New chat in <you>'s workspace".
- [ ] **Step 2:** Ask the agent to read the secret in a file, edit a line, and run `git diff --stat && git log -1 --format=%an`. Expected: the edit lands in the real workspace folder; the scratch folder under `team_live/` stays empty; tool calls in the chat are `mcp__teammate_workspace__*`.
- [ ] **Step 3:** With the file open and unsaved in the editor, ask for an edit to it. Expected: the tool result says it has unsaved changes; the disk file is unchanged.
- [ ] **Step 4:** Ask for `sleep 300`, press Stop. Expected: the `sleep` process is gone (`pgrep -f "sleep 300"` prints nothing).
- [ ] **Step 5:** Turn "Teammates' agents can work here" off and ask for a read. Expected: a readable refusal, the chat stays usable.
- [ ] **Step 6:** Record the results in the PR description.
