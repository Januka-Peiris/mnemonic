# Chat-first Workspaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** New Mnemonic workspaces open on a Claude chat (Zed's ACP `ConversationView`) instead of a CLI terminal, with chats as a session kind next to terminals, PR/auto-fix/review routed to chats, and chats resumed after restart.

**Architecture:** `WorkspaceView` replaces its parallel `terminals`/`terminal_titles` vectors with one `sessions: Vec<WorkspaceSession>` whose kind is either a terminal or a chat. A chat session wraps an `agent_ui::ConversationView` built the same way `AgentPanel::create_agent_thread_inner` builds one. Chat sessions are persisted in a new `workspace_chat_sessions` table in mnemonic-core (with the ACP session id), which is also the marker that a workspace is chat-first.

**Tech Stack:** Rust, GPUI, rusqlite (mnemonic-core), `agent_ui` / `acp_thread` / `agent_servers` crates.

**Spec:** `docs/superpowers/specs/2026-11-10-chat-first-workspaces-design.md`

## Global Constraints

- Repo `CLAUDE.md` Rust rules: no `unwrap()`/panicking indexing, no `let _ =` on fallible calls (use `?`, `.log_err()`, or match), no `mod.rs`, full-word variable names, comments only for non-obvious "why".
- Lint with `./script/clippy`, not `cargo clippy`.
- No em dashes anywhere (code, comments, strings, commits).
- zed.dev cloud provider stays unregistered; `disable_ai` stays `true` by default; sign-in and edit predictions stay off; the Zed agent side panel is not shown.
- Agents: `claude-acp` and `codex-acp`, both `"type": "registry"`.
- Commit messages end with the line `Written with Claude Code`.

## Review Focus

1. **Dispatch before the chat has connected** (Create PR clicked seconds after opening a workspace): the instruction must be queued and sent once the thread exists, not dropped. Test in Task 3.
2. **Dispatch while the chat is mid-turn** (auto-fix fires while the agent is still working): the instruction must wait until the turn ends, not cancel or interleave. Test in Task 3.
3. **ACP session id changes or arrives late** (first prompt creates the session): the stored id must be updated, and a stale id on resume must fall back to a fresh chat with an error shown, not a blank tab. Task 2 persists, Task 4 manual check.
4. **Workspace with no chat rows** (every pre-existing workspace): must behave exactly as today (main claude CLI, CLI dispatch). Test in Task 3 (dispatch target), Task 2 (no rows -> legacy path).
5. **Archive of a chat-first workspace**: chat rows deleted and connections closed, so a recreated workspace with the same id does not resume a dead session. Task 5.

---

### Task 1: Persist chat sessions in mnemonic-core

**Files:**
- Modify: `crates/mnemonic-core/src/db/migrations.rs` (new table before the deprecated-table drop at ~line 735; test alongside `creates_coordination_artifacts_table`)
- Create: `crates/mnemonic-core/src/repositories/chat_session_repository.rs`
- Modify: `crates/mnemonic-core/src/repositories/mod.rs` (add `pub mod chat_session_repository;`)
- Modify: `crates/mnemonic-core/src/models.rs` (add `ChatSessionRecord`)

**Interfaces:**
- Produces:
  ```rust
  // models.rs
  #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
  pub struct ChatSessionRecord {
      pub id: String,               // uuid
      pub workspace_id: String,
      pub agent: String,            // "claude-acp" | "codex-acp"
      pub title: String,            // tab label, e.g. "Claude", "Codex 2", "Review"
      pub acp_session_id: Option<String>,
      pub is_review: bool,
      pub position: i64,            // tab order among chats
  }
  // chat_session_repository.rs
  pub fn list_for_workspace(db: &Database, workspace_id: &str) -> Result<Vec<ChatSessionRecord>, String>; // ORDER BY position, rowid
  pub fn insert(db: &Database, record: &ChatSessionRecord) -> Result<(), String>;
  pub fn set_acp_session_id(db: &Database, id: &str, acp_session_id: &str) -> Result<(), String>;
  pub fn delete(db: &Database, id: &str) -> Result<(), String>;
  pub fn delete_for_workspace(db: &Database, workspace_id: &str) -> Result<(), String>;
  ```
- Table: `workspace_chat_sessions(id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, agent TEXT NOT NULL, title TEXT NOT NULL, acp_session_id TEXT, is_review INTEGER NOT NULL DEFAULT 0, position INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)` plus index on `workspace_id`.

- [ ] **Step 1: Write the failing test** in `chat_session_repository.rs` `#[cfg(test)] mod tests`, using an in-memory `Database` the same way other repository tests in this crate do:

```rust
#[test]
fn chat_sessions_round_trip_per_workspace() -> Result<(), String> {
    // insert two records for "ws-a" (positions 1, 0) and one for "ws-b"
    // list_for_workspace("ws-a") returns 2, ordered by position (the position-0 one first)
    // set_acp_session_id(first.id, "sess-1") -> listing shows Some("sess-1")
    // delete(first.id) -> "ws-a" has 1
    // delete_for_workspace("ws-a") -> "ws-a" has 0, "ws-b" still has 1
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p mnemonic-core chat_sessions_round_trip_per_workspace`
Expected: FAIL to compile (`chat_session_repository` not found).

- [ ] **Step 3: Implement** the table (`CREATE TABLE IF NOT EXISTS` in `migrations::run`, mapping errors like the neighbouring blocks), the model, and the five repository functions with `db.with_connection` / `with_connection_mut` as in `workspace_repository.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mnemonic-core chat_session`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mnemonic-core
git commit -m "Persist workspace chat sessions

Written with Claude Code"
```

---

### Task 2: Chat session kind in WorkspaceView

**Files:**
- Modify: `crates/mnemonic-panels/Cargo.toml` (add `agent_ui`, `agent_servers`, `acp_thread`, `agent`, `agent-client-protocol` workspace deps as needed, plus `fs`)
- Modify: `crates/agent_ui/src/conversation_view.rs:616` (make `root_thread` `pub`)
- Modify: `crates/mnemonic-panels/src/workspace_view.rs`
- Modify: `assets/settings/default.json:2682` (default `agent_servers`)

**Interfaces:**
- Consumes: `chat_session_repository::*`, `ChatSessionRecord` (Task 1); `mnemonic_bridge::mnemonic_state(cx)` for the db.
- Produces (in `workspace_view.rs`):
  ```rust
  pub(crate) enum SessionKind {
      Terminal(Entity<TerminalView>),
      Chat(ChatSession),
  }
  pub(crate) struct WorkspaceSession {
      pub title: SharedString,
      pub kind: SessionKind,
  }
  pub(crate) struct ChatSession {
      pub record_id: String,                 // ChatSessionRecord.id
      pub agent: ChatAgent,
      pub is_review: bool,
      pub view: Entity<agent_ui::ConversationView>,
      pub pending_messages: Vec<String>,     // dispatched text waiting for connect / idle
      _subscriptions: Vec<Subscription>,
  }
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(crate) enum ChatAgent { Claude, Codex }
  impl ChatAgent {
      fn agent_id(self) -> &'static str;     // "claude-acp" | "codex-acp"
      fn label(self) -> &'static str;        // "Claude" | "Codex"
  }
  // WorkspaceView fields: `sessions: Vec<WorkspaceSession>`, `active_session: usize`
  // replace `terminals`, `terminal_titles`, `active_terminal`.
  fn open_chat(&mut self, record: ChatSessionRecord, initial_prompt: Option<String>, window: &mut Window, cx: &mut Context<Self>);
  fn add_chat(&mut self, agent: ChatAgent, window: &mut Window, cx: &mut Context<Self>); // inserts a record, then open_chat
  fn active_is_chat(&self) -> bool;
  ```

- [ ] **Step 1: Ship the agents by default.** In `assets/settings/default.json` set:

```jsonc
"agent_servers": {
  "claude-acp": { "type": "registry", "default_config_options": { "mode": "bypassPermissions" } },
  "codex-acp": { "type": "registry", "default_config_options": { "mode": "<codex-acp's no-approval mode id>" } }
},
```

Look up codex-acp's mode ids from its session modes (start a Codex chat once and log `ConversationView` mode options, or read the registry entry) and use the full-access / never-ask one. If codex-acp exposes no such mode, omit `default_config_options` for it and note it in the PR description.

- [ ] **Step 2: Refactor terminals into `sessions`.** Mechanical: every use of `terminals`, `terminal_titles`, `active_terminal` (`push_terminal`, `start_review`, `add_*_terminal`, `dispatch_target`, `render_terminal_tabs`, `Focusable`, `Render`, `set_active`) goes through `sessions`/`active_session`, matching on `SessionKind`. The activity subscription in `push_terminal` stays attached to the first *terminal* pushed. Run `cargo check -p mnemonic-panels`; expected: compiles, behaviour unchanged.

- [ ] **Step 3: Implement `open_chat`.** Build the view like `AgentPanel::create_agent_thread_inner` (`crates/agent_ui/src/agent_panel.rs:4505`):
  - `agent = agent_ui::Agent::Custom { id: record.agent.into() }`; `server = agent.server(project.read(cx).fs().clone(), agent::ThreadStore::global(cx))` (use the fs the project exposes; any `Arc<dyn Fs>` from the app works).
  - One `AgentConnectionStore` per `WorkspaceView` (field `connection_store: Entity<agent_ui::AgentConnectionStore>`, created with `AgentConnectionStore::new(project, cx)` when the view is created), shared by all its chats.
  - `resume_session_id = record.acp_session_id.map(acp::SessionId::new)`, `thread_id = None`, `work_dirs = worktree_path` as a `PathList`, `title = Some(record.title)`, `initial_content = None`, `thread_store = None`, `source` = the `AgentThreadSource` variant used for panel-created threads.
  - Subscribe to `RootThreadUpdated`: read `view.root_thread(cx)`; if its `session_id()` differs from the stored one, call `chat_session_repository::set_acp_session_id` (on `cx.background_spawn`, log errors); observe that `AcpThread` so that while `status() == ThreadStatus::Generating` it calls `mnemonic_bridge::record_agent_activity` + `schedule_idle_check`, and when it becomes `Idle` it flushes one `pending_messages` entry (Task 3 defines the flush).
  - If `initial_prompt` is `Some`, push it onto `pending_messages`.
  - Push a `WorkspaceSession { title, kind: Chat(..) }` and make it active; focus the view.
  - If the view ends in an error state (`!view.read(cx).is_ok()` after load, e.g. stale session id, registry unavailable), the tab still shows Zed's error UI; add a Button "Open Claude CLI instead" above it that calls `add_claude_terminal`.

- [ ] **Step 4: Chat-first startup.** In `WorkspaceHost::set_active`, replace the unconditional `spawn_agent_terminal` with: load `chat_session_repository::list_for_workspace`; if non-empty, `open_chat` each in order (no CLI started); if empty, `spawn_agent_terminal` as today. In `WorkspacePanel::add_worktree` (`crates/mnemonic-panels/src/workspace_panel.rs:~343`, on `Ok(Ok(detail))`, before `select_workspace`), insert `ChatSessionRecord { id: uuid, workspace_id, agent: "claude-acp", title: "Claude", acp_session_id: None, is_review: false, position: 0 }`.

- [ ] **Step 5: Tabs, "+" menu, composer.**
  - Replace the "+ Claude", "+ GPT" and "+" buttons with one `PopoverMenu` + `ContextMenu` on an `IconName::Plus` button with entries, in order: "Claude chat", "Codex chat", "Claude CLI", "Codex CLI", "Shell" -> `add_chat(Claude)`, `add_chat(Codex)`, `add_claude_terminal`, `add_gpt_terminal`, `add_shell_terminal`.
  - `add_chat` titles: first Claude chat "Claude", then "Claude 2", ...; same for "Codex". Position = max existing chat position + 1.
  - Each chat tab gets a close (x) `IconButton`; closing removes the session, calls `chat_session_repository::delete`, and `view.close_all_sessions(cx)` (detach the task). No replacement chat is created.
  - `Render`: when `active_is_chat()`, do not render `render_resize_handle` / `render_composer`.
  - Status chip: "running" if any chat's root thread is `Generating` or `agent_is_running()`.

- [ ] **Step 6: Build and lint**

Run: `cargo check -p mnemonic-panels -p zed && ./script/clippy`
Expected: no errors, no new warnings.

- [ ] **Step 7: Commit**

```bash
git add assets/settings/default.json crates/agent_ui/src/conversation_view.rs crates/mnemonic-panels crates/mnemonic-panels/Cargo.toml Cargo.lock
git commit -m "Add chat sessions to workspaces and start new workspaces on a Claude chat

Written with Claude Code"
```

---

### Task 3: Route PR, Fix errors, auto-fix and Review to chats

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_view.rs`

**Interfaces:**
- Consumes: `WorkspaceSession`, `SessionKind`, `ChatSession`, `ChatAgent`, `open_chat` (Task 2).
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(crate) enum DispatchTarget { Chat(usize), Terminal(usize) }
  /// Pure: picks where a dispatched instruction goes.
  pub(crate) fn pick_dispatch_target(sessions: &[SessionSummary], active: usize) -> Option<DispatchTarget>;
  #[derive(Clone, Debug)]
  pub(crate) struct SessionSummary { pub is_chat: bool, pub is_review: bool, pub agent_label: SharedString /* "claude", "codex", "shell", ... */ }
  fn flush_pending(&mut self, session_index: usize, cx: &mut Context<Self>);
  #[derive(Default)]
  pub(crate) struct PendingMessages(std::collections::VecDeque<String>);
  impl PendingMessages { pub fn push(&mut self, text: String); pub fn take_next(&mut self) -> Option<String>; }
  ```
  `ChatSession.pending_messages` (Task 2) becomes `PendingMessages`.
  Rule order: active is a non-review chat -> it; else first non-review Claude chat; else first non-review chat; else active if it is a claude/codex terminal; else first claude/codex terminal; else `None`.

- [ ] **Step 1: Write the failing tests** in the existing `mod tests`:

```rust
#[test]
fn dispatch_prefers_active_chat_then_first_claude_chat() {
    // sessions: [Codex chat, Claude chat, Review chat(claude), claude terminal]
    // active=0 -> Chat(0); active=2 (review) -> Chat(1); active=3 (terminal) -> Chat(1)
}
#[test]
fn dispatch_falls_back_to_cli_without_chats() {
    // sessions: [claude terminal "claude", shell] ; active=1 -> Terminal(0); active=0 -> Terminal(0)
    // sessions: [shell] -> None
}
#[test]
fn dispatch_to_unconnected_or_busy_chat_is_queued_in_order() {
    // Pure helper test: given pending ["a"], a dispatch of "b" while not idle leaves ["a","b"];
    // flushing on idle sends "a" first and leaves ["b"].
    // Implement the queue as a small pure struct (e.g. `PendingMessages { fn push, fn take_next }`) so this needs no GPUI.
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mnemonic-panels dispatch_`
Expected: FAIL to compile (`pick_dispatch_target` not found).

- [ ] **Step 3: Implement.**
  - `pick_dispatch_target` per the rule above; `dispatch_target_is_claude` uses it.
  - `dispatch_to_agent(text)`: `Chat(i)` -> push to that chat's pending queue, then `flush_pending(i)`; `Terminal(i)` -> existing text-then-Enter path; `None` -> log.
  - `flush_pending(i)`: if the chat has a root thread with `status() == Idle`, take the next message and call `thread.update(cx, |thread, cx| thread.send_raw(&text, cx))`, detaching the returned task with `.detach_and_log_err(cx)` (or the equivalent for its return type); otherwise leave it queued (the `Idle` observer from Task 2 calls this again).
  - `start_review`: if a chat session with `is_review` exists, focus it; else if the workspace is chat-first (has any chat session), insert a `ChatSessionRecord { agent: "claude-acp", title: "Review", is_review: true, .. }` and `open_chat(record, Some(review_prompt))`; else keep today's CLI review. After the review chat connects, if `connection.model_selector(session_id)` exists and lists a model whose id contains `"sonnet"`, select it; otherwise leave the agent default.
  - Composer `/model` and `/effort` live-switching only applies when the target is a claude terminal (unchanged behaviour).

- [ ] **Step 4: Run tests**

Run: `cargo test -p mnemonic-panels`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/mnemonic-panels/src/workspace_view.rs
git commit -m "Route PR, auto-fix and review instructions to workspace chats

Written with Claude Code"
```

---

### Task 4: Manual verification in the running app

**Files:** none (fix-ups go in the task that owns the code).

- [ ] **Step 1:** Remove `"disable_ai": false` and the `agent_servers` block added to `~/.config/mnemonic/settings.json` earlier in this session, so the defaults are what is tested.
- [ ] **Step 2:** Build and launch Mnemonic (`./run-mnemonic.command` or `cargo run -p zed`).
- [ ] **Step 3:** Check, and note each result for the PR description:
  - New workspace opens on a "Claude" chat tab; no CLI started; composer bar hidden on it.
  - Sending a message gets a reply; paste an image and drag-drop an image file into the chat box; both attach and are sent.
  - "+" menu: each of the five entries opens the right session; composer shows on CLI tabs.
  - Create PR right after opening a new workspace: instruction appears in the chat once connected.
  - Turn on Auto-fix while the chat is mid-turn: instruction is sent after the turn ends.
  - Review opens a separate "Review" chat; clicking Review again refocuses it.
  - Quit and relaunch: chat tabs come back with history and continue the same conversation.
  - An existing (pre-change) workspace still opens on its claude CLI and Create PR still types into it.
  - Zed's agent side panel and the zed.dev sign-in are not visible.
- [ ] **Step 4:** If chats do not work with `disable_ai: true` (agent store or ACP connection gated by it), STOP and report to Jay with the gating code location rather than flipping the default.

---

### Task 5: Archive cleans up chats

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_panel.rs` (`confirm_archive`, `Ok(())` branch ~line 176)
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`WorkspaceHost::remove_workspace_view`)

- [ ] **Step 1:** In `remove_workspace_view`, before dropping the view, call `close_all_sessions(cx)` on each chat's `ConversationView` (detach the task).
- [ ] **Step 2:** In `confirm_archive`'s success branch, call `chat_session_repository::delete_for_workspace(&state.db, &ws_id)` (in the existing background block after `archive_workspace` succeeds), logging errors.
- [ ] **Step 3:** Run `cargo check -p mnemonic-panels && ./script/clippy`; manually archive a chat-first workspace and confirm its rows are gone (`sqlite3` on the Mnemonic db: `SELECT count(*) FROM workspace_chat_sessions WHERE workspace_id = '<id>'` returns 0).
- [ ] **Step 4: Commit**

```bash
git add crates/mnemonic-panels
git commit -m "Close and forget chat sessions when a workspace is archived

Written with Claude Code"
```
