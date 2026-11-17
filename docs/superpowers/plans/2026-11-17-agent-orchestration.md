# Agents That Start and Talk to Other Agents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every Mnemonic chat (Claude or Codex) MCP tools to start chats in new or existing workspaces, message them, wait for their replies, list, stop and archive, and remove the old coordinator.

**Architecture:** A local MCP-over-HTTP endpoint on 127.0.0.1 (tiny_http, as the team share server) is added to every chat's ACP session with a per-session key. The endpoint runs each tool call on a request thread that hands it to a foreground GPUI task in `mnemonic-panels`, which carries it out with the existing `WorkspaceHost`/`WorkspaceView` chat machinery and answers over an SSE stream with keep-alives. Which chat started which is kept in a new `agent_started_chats` table.

**Tech Stack:** Rust, GPUI, tiny_http, rusqlite, agent-client-protocol (`acp`), serde_json.

**Spec:** `docs/superpowers/specs/2026-11-17-agent-orchestration-design.md`

## Global Constraints

- No em dashes anywhere (code, comments, strings, commit messages).
- No `unwrap()`/`expect()` outside tests; no `let _ =` on fallible calls; use `?`, `.log_err()` or explicit handling.
- Lint with `./script/clippy`; format only touched files with `rustfmt --edition 2024 <files>`, never `cargo fmt` on a whole crate.
- No `mod.rs` files; new modules are `name.rs`.
- MCP server name: `mnemonic` (never `workspace`, which Claude reserves).
- Endpoint binds `127.0.0.1` only, port chosen by the OS at startup.
- Per-chat key header: `X-Mnemonic-Chat-Key`.
- Limits: 8 busy agent-started chats per starting chat; chats at depth 2 (started by an agent-started chat) get no `start_chat`; dedupe window 2 minutes; `wait` default 1200 s, max 3600 s; reply text cut at 6000 characters.
- Teammates' chats (cwd under `{data_dir}/team_live`, i.e. `requires_session_extras(cwd)` is true) never get the tools.
- Branch: `jay/agent-orchestration` from `origin/main`.

## Review Focus

1. An agent starts a chat in the workspace Jay is looking at, or creates a workspace while he types elsewhere: no tab, pane or workspace switch may take focus. Covered by the Task 9 manual check 3; Task 7 adds new tabs with `activate: false`.
2. Two workspaces or chats share a name: a name lookup must refuse with both ids listed, never pick one. Test in Task 5 (`resolve_refuses_ambiguous_name`).
3. A chat resumed after Mnemonic restarts must keep working tools (new key, bound to its known session id). Test in Task 6 (`resumed_session_gets_tools_bound_to_its_id`).
4. `wait` on a chat whose tab was closed or whose workspace was archived must answer "closed" instead of waiting forever. Test in Task 5 (`render_wait_reports_closed_chat`); Task 8 maps a missing chat to `ChatState::Closed`.
5. A calling chat stopped mid-`wait`: the request is cancelled and the wait loop ends without leaking a task. Test in Task 4 (`cancelled_call_sets_cancel_flag`); Task 8 loop checks the flag each poll.

---

### Task 1: Remove the coordinator

**Files:**
- Delete: `crates/mnemonic-core/src/services/coordinator_service.rs`, `crates/mnemonic-core/src/repositories/coordinator_repository.rs`, `crates/mnemonic-core/src/models/coordinator.rs`, `crates/mnemonic-panels/src/coordinator_panel.rs`
- Modify: `crates/mnemonic-core/src/services/mod.rs`, `repositories/mod.rs`, `models/mod.rs`, `crates/mnemonic-panels/src/lib.rs:2,17-18`, `crates/zed/src/zed.rs:754-782,1280-1283`, `crates/mnemonic-core/src/services/orchestrator_service.rs:61-63` (and `run_coordinator_autostep_watch_job`), `crates/mnemonic-core/src/services/app_runtime_service.rs:4,15,50,70-73,135,145`, `crates/mnemonic-core/src/events.rs:12,27`, `crates/mnemonic-core/src/state.rs:20,71-72,143`, `crates/mnemonic-core/src/services/agent_profile_service.rs` (coordinator brain/coder defaults and eligibility), `models/agent_profile.rs` (`coordinator_eligible`), and the matching struct literals in `local_llm_service.rs` and `terminal_service.rs`; any keymap entry for `ToggleCoordinatorFocus` under `assets/keymaps/`.

**Interfaces:**
- Produces: nothing new. `orchestrator_service` keeps `workspace_health_check` and `review_reminder`. `AppRuntimeOptions` loses `reconcile_coordinators`. Database tables stay; no migration drops them.

- [ ] **Step 1:** Delete the four files and every `mod`/`use`/registration that names them. Remove the `coordinator_autostep_watch` match arm and its function, the startup reconcile, `COORDINATOR_NOTIFY`, `CoordinatorStepRegistry`/`coordinator_step_inflight`, `ToggleCoordinatorFocus` and the panel load. Remove `coordinator_eligible` and the coordinator default-profile functions, plus whatever only they used.
- [ ] **Step 2:** `rg -i coordinator crates assets` shows only migrations (table creation) and nothing else.
- [ ] **Step 3:** Run `cargo check -p mnemonic-core -p mnemonic-panels -p zed` and `cargo test -p mnemonic-core`. Expected: builds, tests pass (tests that only covered the coordinator are deleted with it).
- [ ] **Step 4:** Commit: `git commit -m "Remove the coordinator service and panel"`.

---

### Task 2: Share the MCP-over-HTTP code

**Files:**
- Create: `crates/mnemonic-core/src/services/mcp_http.rs`
- Modify: `crates/mnemonic-core/src/services/team_agent_tools.rs:537-830`, `crates/mnemonic-core/src/services/team_share_server.rs:470-625`, `services/mod.rs`

**Interfaces:**
- Produces (in `mcp_http.rs`):
  ```rust
  pub enum McpReply { Json(String), Accepted }
  pub trait McpTools: Send + Sync {
      fn server_name(&self) -> &str;
      fn definitions(&self) -> serde_json::Value;
      fn call(&self, call_key: &str, name: &str, arguments: &serde_json::Value) -> Result<String, String>;
      fn cancel(&self, call_key: &str);
  }
  pub fn handle_mcp_message(tools: &dyn McpTools, caller_key: &str, body: &[u8]) -> McpReply;
  pub fn mcp_request_method(body: &[u8]) -> (Option<String>, bool /* has id */);
  pub fn accepts_event_stream(accept_header: Option<&str>, http_version: (u8, u8)) -> bool;
  pub fn write_event_stream(writer: &mut dyn std::io::Write, reply: &std::sync::mpsc::Receiver<String>, keep_alive: std::time::Duration) -> std::io::Result<()>;
  pub fn required_str<'a>(arguments: &'a serde_json::Value, key: &str) -> Result<&'a str, String>;
  pub fn optional_str<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str>;
  pub fn optional_u64(arguments: &serde_json::Value, key: &str) -> Option<u64>;
  pub fn flag(arguments: &serde_json::Value, key: &str) -> bool;
  ```
  `call_key` is `{caller_key}:{request id}`, as today. `team_agent_tools` implements `McpTools` for a struct holding `AgentToolContext` and `RunningCommands`, with `cancel` = `RunningCommands::kill`.

- [ ] **Step 1:** Move the JSON-RPC framing, the argument helpers and `write_event_stream`/`write_chunk` into `mcp_http.rs` unchanged in behaviour; make `team_agent_tools` and `team_share_server` use them.
- [ ] **Step 2:** Run `cargo test -p mnemonic-core team_` . Expected: every existing team agent and share-server test passes unchanged (they are the proof the move kept behaviour).
- [ ] **Step 3:** Commit: `git commit -m "Share MCP over HTTP handling between servers"`.

---

### Task 3: Record which chat started which

**Files:**
- Create: `crates/mnemonic-core/src/repositories/agent_started_chat_repository.rs`
- Modify: `crates/mnemonic-core/src/db/migrations.rs` (after `workspace_chat_sessions`, ~line 760), `crates/mnemonic-core/src/models/chat_session.rs`, `repositories/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  // models/chat_session.rs
  pub struct AgentStartedChat { pub chat_id: String, pub started_by_chat_id: String, pub workspace_id: String, pub made_workspace: bool }
  // agent_started_chat_repository.rs
  pub fn insert(db: &Database, record: &AgentStartedChat) -> Result<(), String>;
  pub fn get(db: &Database, chat_id: &str) -> Result<Option<AgentStartedChat>, String>;
  pub fn started_by(db: &Database, chat_id: &str) -> Result<Vec<AgentStartedChat>, String>;
  pub fn workspace_made_by_agent(db: &Database, workspace_id: &str) -> Result<bool, String>;
  pub fn depth(db: &Database, chat_id: &str) -> Result<u32, String>; // 0 for a chat nobody started
  ```
  Table: `agent_started_chats(chat_id TEXT PRIMARY KEY, started_by_chat_id TEXT NOT NULL, workspace_id TEXT NOT NULL, made_workspace INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)`, index on `started_by_chat_id`. Rows are not deleted when chats close (they are the record of who made what).

- [ ] **Step 1: Write the failing test** `depth_follows_the_started_by_chain`: insert A started by `user-chat`, B started by A; assert `depth("user-chat") == 0`, `depth(A) == 1`, `depth(B) == 2`, and `workspace_made_by_agent` true only for the workspace inserted with `made_workspace: true`. Use the in-memory test database helper the other repository tests use.
- [ ] **Step 2:** Run `cargo test -p mnemonic-core agent_started_chat`. Expected: FAIL (module missing).
- [ ] **Step 3:** Add the migration and repository. `depth` walks `started_by_chat_id` with a cap of 16 steps so a corrupt cycle cannot loop.
- [ ] **Step 4:** Run the test. Expected: PASS.
- [ ] **Step 5:** Commit: `git commit -m "Record chats started by agents"`.

---

### Task 4: Local agent hub endpoint

**Files:**
- Create: `crates/mnemonic-core/src/services/agent_hub_server.rs`
- Modify: `services/mod.rs`

**Interfaces:**
- Consumes: `mcp_http::{handle_mcp_message, McpTools, accepts_event_stream, write_event_stream, mcp_request_method}` (Task 2).
- Produces:
  ```rust
  pub const AGENT_HUB_SERVER_NAME: &str = "mnemonic";
  pub const AGENT_HUB_KEY_HEADER: &str = "X-Mnemonic-Chat-Key";
  #[derive(Clone, Default)] pub struct HubKeys { /* Arc<Mutex<HashMap<String, Option<String>>>> */ }
  impl HubKeys {
      pub fn mint(&self) -> String;                       // random uuid, unbound
      pub fn bind(&self, key: &str, session_id: &str);
      pub fn session_for(&self, key: &str) -> Option<Option<String>>; // None = unknown key
  }
  #[derive(Clone, Debug)] pub struct HubCaller { pub session_id: String }
  pub trait AgentHubHandler: Send + Sync + 'static {
      fn definitions(&self, caller: &HubCaller) -> serde_json::Value;
      /// Blocking; runs on a request thread. `cancelled` turns true when the client cancels the call.
      fn call(&self, caller: &HubCaller, tool: &str, arguments: serde_json::Value, cancelled: Arc<AtomicBool>) -> Result<String, String>;
  }
  pub struct AgentHubServer { /* server, accept thread */ }
  impl AgentHubServer {
      pub fn start(keys: HubKeys, handler: Arc<dyn AgentHubHandler>) -> Result<Self, String>;
      pub fn url(&self) -> String; // http://127.0.0.1:{port}/mcp
  }
  ```
  Requests: only `POST /mcp`; missing/unknown key → 401; key not yet bound to a session → 409 "This chat's session isn't ready yet"; any `Origin` header or a `Host` other than `127.0.0.1:{port}`/`localhost:{port}` → 403 (blocks browsers and DNS rebinding). `tools/call` with `Accept: text/event-stream` is answered as SSE with keep-alives every 15 s; `notifications/cancelled` sets that call's `cancelled` flag. Dropping the server stops the accept thread.

- [ ] **Step 1: Write the failing tests** (spin up with a fake handler that records calls):
  - `refuses_missing_or_unknown_key`: POST without header → 401; with an unminted key → 401.
  - `refuses_requests_from_a_browser`: valid key plus `Origin: http://evil` → 403.
  - `answers_tools_list_for_a_bound_key`: mint, bind to `"s1"`, `tools/list` → 200 whose body contains the fake handler's tool name, and the handler saw `caller.session_id == "s1"`.
  - `long_call_streams_keep_alives`: fake `call` sleeps past a 50 ms keep-alive (make the interval a constructor-private parameter for tests), response contains `: keep-alive` and then the result.
  - `cancelled_call_sets_cancel_flag`: start a call that loops until `cancelled`, send `notifications/cancelled` for its id, the call returns within 1 s.
- [ ] **Step 2:** Run `cargo test -p mnemonic-core agent_hub_server`. Expected: FAIL (module missing).
- [ ] **Step 3:** Implement, reusing Task 2's helpers and the request-thread pattern of `team_share_server::handle_agent`.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Commit: `git commit -m "Serve agent hub tools on a local MCP endpoint"`.

---

### Task 5: Agent hub rules and replies

**Files:**
- Create: `crates/mnemonic-core/src/services/agent_hub_service.rs`
- Modify: `services/mod.rs`, `crates/mnemonic-core/src/services/workspace_service.rs`, `crates/mnemonic-panels/src/workspace_panel.rs:1030-1076` (call the new input builder instead of its inline copy)

**Interfaces:**
- Consumes: `agent_started_chat_repository` (Task 3).
- Produces:
  ```rust
  pub const MAX_BUSY_CHILDREN: usize = 8;
  pub const MAX_START_DEPTH: u32 = 1; // a chat at depth <= 1 may start chats
  pub const REPLY_CHARS: usize = 6000;
  pub const WAIT_DEFAULT_SECONDS: u64 = 1200;
  pub const WAIT_MAX_SECONDS: u64 = 3600;
  pub const DEDUPE_WINDOW: Duration = Duration::from_secs(120);

  pub enum ChatAgentChoice { Claude, Codex }               // "claude-acp" / "codex-acp"
  pub enum ChatPlace { Existing { workspace: String }, New { repo: String, branch: Option<String>, base: Option<String> } }
  pub struct StartChatRequest { pub task: String, pub agent: ChatAgentChoice, pub place: ChatPlace, pub title: Option<String> }
  pub fn parse_start_chat(arguments: &serde_json::Value) -> Result<StartChatRequest, String>;
  pub struct WaitRequest { pub chats: Vec<String>, pub any: bool, pub timeout: Duration }
  pub fn parse_wait(arguments: &serde_json::Value) -> Result<WaitRequest, String>;

  pub struct Named<'a> { pub id: &'a str, pub name: &'a str }
  pub fn resolve<'a>(kind: &str, items: &[Named<'a>], text: &str) -> Result<&'a str, String>; // id or exact unique name

  pub enum ChatState { Starting, Replying, WaitingForApproval, Replied, Stopped, Closed }
  pub struct ChatReport { pub id: String, pub title: String, pub agent: String, pub state: ChatState, pub workspace_path: Option<String>, pub branch: Option<String>, pub reply: Option<String> }
  pub fn render_wait(reports: &[ChatReport], timed_out: bool, typed: &[String]) -> String;
  pub fn render_list(repos: &[(String, String)], workspaces: &[ListedWorkspace], caller_chat_id: &str, started: &[String]) -> String;
  pub struct ListedWorkspace { pub id: String, pub name: String, pub repo: String, pub branch: String, pub path: Option<String>, pub chats: Vec<(String, String, String, ChatState)> /* id, title, agent, state */ }

  pub fn tool_definitions(can_start: bool) -> serde_json::Value;

  #[derive(Default)] pub struct RecentStarts { /* (caller, normalized request) -> (Instant, chat id) */ }
  impl RecentStarts { pub fn earlier(&mut self, caller: &str, request: &StartChatRequest, now: Instant) -> Option<String>; pub fn remember(&mut self, caller: &str, request: &StartChatRequest, chat_id: &str, now: Instant); }

  pub fn check_can_start(db: &Database, caller_chat_id: &str, busy_children: usize) -> Result<(), String>;
  pub fn check_can_stop(db: &Database, chat_id: &str) -> Result<(), String>;
  pub fn check_can_archive(db: &Database, workspace_id: &str, worktree_path: &Path) -> Result<(), String>;
  ```
  In `workspace_service.rs`:
  ```rust
  pub fn new_workspace_input(state: &AppState, repo: &RepositoryRecord, name: String, branch: Option<String>, base: Option<String>) -> Result<CreateWorkspaceInput, String>;
  ```
  It holds the branch-prefix and base-branch rules now inline in `workspace_panel.rs`; an existing branch (one `git rev-parse --verify` finds) goes in `selected_branch` with `branch: None`, a new one in `branch`. Tool descriptions follow Javen's wording style: full brief, returns at once, collect with `wait`.

- [ ] **Step 1: Write the failing tests:**
  - `parse_start_chat_needs_a_place`: neither `workspace` nor `repo` → Err; both → Err; `agent: "gemini"` → Err naming claude and codex.
  - `parse_wait_caps_the_timeout`: `timeout_seconds: 99999` → 3600 s; missing → 1200 s; empty `chats` → Err.
  - `resolve_refuses_ambiguous_name`: two items named "api" → Err containing both ids; exact id wins over a name; unknown → Err listing nothing found.
  - `render_wait_reports_closed_chat`: a `Closed` report renders "closed" and no reply; a 7000-char reply is cut to 6000 with a note; `typed` non-empty puts "The user wrote to you while you were waiting" and their text first, with the line telling the agent to answer before waiting again; `timed_out` says which are still replying and to call `wait` again.
  - `start_limits`: with depth-2 caller → Err; with `busy_children == 8` → Err naming the limit; depth-1 caller with 7 busy → Ok. `tool_definitions(false)` has no `start_chat`.
  - `recent_starts_dedupes_within_two_minutes`: same caller and request at t and t+119 s → earlier id; at t+121 s → None; different task → None.
  - `archive_rules`: workspace not made by an agent → Err; made by agent with an untracked file in a temp git repo → Err mentioning uncommitted changes; clean → Ok. `check_can_stop` on a chat without an `agent_started_chats` row → Err.
- [ ] **Step 2:** Run `cargo test -p mnemonic-core agent_hub_service`. Expected: FAIL.
- [ ] **Step 3:** Implement. Uncommitted check: `git status --porcelain` in the worktree is empty.
- [ ] **Step 4:** Run the tests, plus `cargo check -p mnemonic-panels` for the `workspace_panel.rs` change. Expected: PASS.
- [ ] **Step 5:** Commit: `git commit -m "Add agent hub rules and replies"`.

---

### Task 6: Give every chat session the tools

**Files:**
- Modify: `crates/agent_servers/src/acp.rs` (near `SessionExtras`, ~1480; `new_session` ~1738; `load_session` ~1876; `resume_session` ~1921; `fork_session` ~1972)

**Interfaces:**
- Produces:
  ```rust
  pub trait SessionToolProvider: 'static {
      /// MCP servers to add to a session in `cwd` and the key they carry; None to add nothing.
      fn tools_for_session(&self, cwd: &Path) -> Option<(Vec<acp::McpServer>, String)>;
      fn bind(&self, key: &str, session_id: &acp::SessionId);
  }
  pub fn set_session_tool_provider(provider: Rc<dyn SessionToolProvider>, cx: &mut App);
  ```
  Applied to new, load, resume and fork requests, appended after project MCP servers and before `SessionExtras`, and skipped when `requires_session_extras(cwd, cx)` (teammates' chats). Bind happens with the known id before the load/resume/fork request is sent, and with the returned id right after `session/new` answers.

- [ ] **Step 1: Write the failing tests** in `acp.rs`'s test module using the existing fake-connection helpers that capture outgoing requests (the ones around line 2790 that serialize `into_new_session_request`):
  - `new_session_gets_tools_and_binds_the_new_id`: provider returns one `McpServer::Http` named `mnemonic`; the captured `session/new` request lists it, and after the response `bind` was called with that key and the returned id.
  - `resumed_session_gets_tools_bound_to_its_id`: `resume_session("s-7")` → request lists the server; `bind(key, "s-7")` happened.
  - `teammate_sessions_get_no_tools`: cwd under a prefix passed to `require_session_extras_under` (with extras set) → the provider's server is absent.
- [ ] **Step 2:** Run `cargo test -p agent_servers session_tool`. Expected: FAIL.
- [ ] **Step 3:** Implement the global and its use in the four request paths.
- [ ] **Step 4:** Run the tests and the existing `acp.rs` tests. Expected: PASS.
- [ ] **Step 5:** Commit: `git commit -m "Let Mnemonic add its own tools to every chat session"`.

---

### Task 7: Chat controls the hub needs in the workspace view

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_view.rs`

**Interfaces:**
- Consumes: `agent_hub_service::ChatState` (Task 5), `agent_started_chat_repository::get` (Task 3).
- Produces (all `pub(crate)`):
  ```rust
  impl WorkspaceHost {
      /// Creates the workspace's view if needed (via `ensure_view`, without activating it) and opens a new chat tab
      /// in it without activating the tab. Returns the new chat record id.
      pub(crate) fn open_chat_in_background(workspace: &mut Workspace, workspace_id: &str, agent: ChatAgent, title: String, prompt: String, window: &mut Window, cx: &mut Context<Workspace>) -> Result<String, String>;
  }
  pub(crate) fn chat_state(record_id: &str, cx: &App) -> ChatState;             // Closed when find_open_chat is None
  pub(crate) fn latest_reply(record_id: &str, cx: &App) -> Option<String>;      // assistant text after the last user message, via to_markdown
  pub(crate) fn take_typed_messages(record_id: &str, cx: &mut App) -> Vec<String>; // drains the root thread view's local_queued_messages, text blocks only
  pub(crate) fn send_to_chat(record_id: &str, text: String, cx: &mut App) -> Result<(), String>; // pending_messages.push + flush_pending
  pub(crate) fn chat_record_for_session(session_id: &str, cx: &App) -> Option<String>; // open chat whose root thread has this session id
  ```
  `ChatState` mapping: thread missing (agent connecting) → `Starting`; `Generating` with a tool call in `WaitingForConfirmation` → `WaitingForApproval`; `Generating` → `Replying`; idle with an assistant message after the last user message → `Replied`; idle otherwise → `Stopped`. A chat's tab tooltip reads "Started by {starter title}" when `agent_started_chat_repository::get` has a row and the starter's record is found.
  Add `ChatAgent::from_choice(ChatAgentChoice) -> ChatAgent`.

- [ ] **Step 1: Write the failing GPUI test** `background_chat_does_not_take_the_active_tab`, following the existing `#[gpui::test]` setup in this file: with workspace A active and its chat tab focused, `open_chat_in_background` for A → the session count grows by one, the active item and focused element are unchanged; for workspace B (no view yet) → B's view exists in `host.views`, A stays the host's active view.
- [ ] **Step 2:** Run `cargo test -p mnemonic-panels background_chat`. Expected: FAIL.
- [ ] **Step 3:** Implement. New tab: `pane.add_item(item, false, false, None, window, cx)`; record inserted with `is_primary: false` and the next position; the prompt goes in as `initial_prompt` so it is sent once the session is ready.
- [ ] **Step 4:** Run the test. Expected: PASS.
- [ ] **Step 5:** Commit: `git commit -m "Open and drive chats without switching the view"`.

---

### Task 8: Wire the tools to Mnemonic

**Files:**
- Create: `crates/mnemonic-panels/src/agent_hub.rs`
- Modify: `crates/mnemonic-panels/src/lib.rs` (`mod agent_hub;` and call `agent_hub::init(cx)` from `init`)

**Interfaces:**
- Consumes: Tasks 3 to 7.
- Produces: `pub fn init(cx: &mut App)`. It starts `AgentHubServer` with a `HubKeys`, installs a `SessionToolProvider` whose `tools_for_session` mints a key and returns `McpServer::Http(McpServerHttp::new("mnemonic", url).headers([X-Mnemonic-Chat-Key: key]))`, and runs a foreground task receiving calls from an `AgentHubHandler` over an unbounded channel (the `PendingAction` pattern in `team.rs:537-606`, with the reply on a `std::sync::mpsc` channel the request thread waits on). If the server fails to start, log it and install no provider (chats work without the tools).

Per tool, on the foreground (caller chat = `chat_record_for_session(caller.session_id)`; refuse "This chat isn't open in Mnemonic" if None):
- `start_chat`: dedupe via `RecentStarts`; `check_can_start(db, caller, busy children counted with chat_state)`; for `New`, build input with `new_workspace_input` and run `workspace_service::create_workspace` on a background thread, then (as `workspace_panel.rs` does) refresh the sidebar without selecting the workspace; open with `open_chat_in_background` in the caller's window; insert the `agent_started_chats` row (`made_workspace` true for `New`). Returns chat id, workspace id, name and path. A failed create returns its error as the tool error.
- `send`: resolve chat by id or name among open chats; `send_to_chat`.
- `wait`: loop on the foreground with GPUI timers (500 ms doubling to 5 s): each pass `take_typed_messages(caller)`, collect `chat_state` per chat; return via `render_wait` when typed is non-empty, all (or with `any` one) are `Replied`/`Stopped`/`Closed`, the timeout passes, or `cancelled` is set (then answer nothing).
- `list`: `render_list` from `repository_repository` list, `workspace_repository::list`, open chats' states, and `started_by(caller)`.
- `stop_chat`: `check_can_stop`, then `WorkspaceView::stop_chat`.
- `archive_workspace`: `check_can_archive`, close the workspace's view, then `workspace_service::archive_workspace` on a background thread.
- `definitions`: `tool_definitions(depth(caller) <= MAX_START_DEPTH)`.

- [ ] **Step 1: Write the failing GPUI test** `start_send_wait_round_trip` with a fake ACP agent (the stub connection other `mnemonic-panels`/`agent_ui` tests use) that answers every prompt with "echo: {text}": call the handler's `start_chat` for an existing workspace, then `wait` → report shows `Replied` and "echo: <task>"; `send` "again" then `wait` → "echo: again"; `stop_chat` on a chat the user opened themselves → Err.
- [ ] **Step 2:** Run `cargo test -p mnemonic-panels agent_hub`. Expected: FAIL.
- [ ] **Step 3:** Implement `agent_hub.rs`.
- [ ] **Step 4:** Run the test, then `./script/clippy` and `cargo test -p mnemonic-core -p mnemonic-panels -p agent_servers`. Expected: all pass, no warnings.
- [ ] **Step 5:** Commit: `git commit -m "Let chats start and talk to other chats"`.

---

### Task 9: Check it in the real app

**Files:** none (manual; results noted in the PR description).

- [ ] **Step 1:** `script/bundle-mac -i` (exits 1 after installing because of the DMG move; the install is fine). Open `/Applications/Mnemonic Dev.app`.
- [ ] **Step 2:** In a Claude chat: "Use the mnemonic tools: list repositories, then start a codex chat in a new workspace on <repo> asking it to report `git status`, wait for it, and tell me what it said." Expect: a new workspace in the sidebar, a Codex chat tab in it titled from the task, the parent reports the reply.
- [ ] **Step 3:** While Step 2's parent is waiting, keep typing in another workspace's editor. Expect: no tab, workspace or focus change. Then type "status?" into the waiting parent chat. Expect: its wait returns within ~5 s and it answers.
- [ ] **Step 4:** Ask the parent to `send` a follow-up, `stop_chat` it mid-reply, and `archive_workspace` the new workspace (once with an uncommitted file present, expect refusal; then after cleaning, expect it archived).
- [ ] **Step 5:** Quit and reopen Mnemonic, open the parent chat, ask it to `list`. Expect: tools still work after resume.
- [ ] **Step 6:** In a teammate's shared workspace chat (team phase 1), confirm `mnemonic` tools are absent.
