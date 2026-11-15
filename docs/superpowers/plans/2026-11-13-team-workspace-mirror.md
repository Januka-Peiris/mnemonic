# Team Workspace Mirror Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A teammate's shared workspace opens in the main window and looks like your own: their chats as real chat tabs updating live, their changed files in the right panel, all read-only.

**Architecture:** The sharer sends each chat entry as the ACP session updates that rebuild it (protocol v2, same indexed long-poll hub). The viewer runs a `TeamConnection` (`AgentConnection`) whose `load_session` builds a real `AcpThread` and applies entry changes to it, so a normal `ConversationView` draws it. A `TeammateWorkspaceView` hosted by `WorkspaceHost` shows those chats as tabs; the right panel lists files split from the teammate's diff.

**Tech Stack:** Rust, GPUI, `agent-client-protocol` 0.14 (`acp`), reqwest blocking client (existing).

**Spec:** `docs/superpowers/specs/2026-11-13-team-workspace-mirror-design.md`

## Global Constraints

- No `unwrap()`, no `let _ =` on fallible operations; log or propagate.
- No em dashes anywhere.
- Every share server route stays a GET; nothing in the mirror writes to the sharer's machine.
- No login, SSH or remote-shell access to a teammate's Mac.
- `TEAM_PROTOCOL_VERSION` becomes `2`.
- Per-crate checks: `cargo clippy -p <crate> --no-deps`; do not run `cargo fmt` on whole files.
- Tests only for concrete behaviour (Jay's rule): each test below names the broken behaviour it catches.

## Review Focus

- A reply streaming in the teammate's chat: text grows smoothly in place, not flicker or duplicated paragraphs (Task 2 `streaming_reply_appends_only_the_new_text`).
- A tool call finishing while later entries already exist (parallel tool calls): it updates in place without dropping the entries after it (Task 2 `tool_call_update_in_the_middle_keeps_later_entries`).
- The sharer rewinds or the hub resets: the mirror rebuilds from scratch instead of keeping stale entries (Task 2 `reset_rebuilds_the_thread`).
- A chat that was already open before sharing started: the first fetch rebuilds it completely (Task 1 round-trip test covers the snapshot path).
- Two Mnemonics on different ACP crate versions: shown as "On a different Mnemonic version", never a silently broken chat (Task 1 `hello_rejects_a_different_acp_version`).

---

### Task 1: Protocol v2, chat entries as ACP updates

**Files:**
- Modify: `crates/mnemonic-core/Cargo.toml` (add `agent-client-protocol.workspace = true`)
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (`TEAM_PROTOCOL_VERSION`, `TeamHello`, `TeamEntry`, `TeamChatUpdate`, hub plan storage, `TeamClient::hello`)
- Modify: `crates/mnemonic-core/src/services/team_share_server.rs` (hello payload, chat payload carries plan)
- Modify: `crates/mnemonic-panels/src/team.rs` (`team_entry`, `thread_entries`, publish plan on `AcpThreadEvent::*` that change `thread.plan()`)

**Interfaces:**
- Produces (in `team_share_service`):
  - `pub const TEAM_PROTOCOL_VERSION: u32 = 2;`
  - `pub const TEAM_ACP_CRATE_VERSION: &str = "0.14.0";` (must equal the `agent-client-protocol` pin in the root `Cargo.toml`)
  - `TeamHello { name: String, protocol: u32, acp_crate: String }`
  - ```rust
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum TeamEntry {
        /// Applied in order to the end of a thread, these rebuild one entry.
        Updates { updates: Vec<acp::SessionUpdate> },
        CompletedPlan { entries: Vec<acp::PlanEntry> },
        Compaction { #[serde(default)] summary: Option<String> },
        #[serde(other)] Unknown,
    }
    ```
  - `TeamChatUpdate` gains `#[serde(default)] pub plan: Vec<acp::PlanEntry>` (the chat's current plan, sent with every update).
  - `TeamShareHub::set_chat_plan(&self, chat_id: &str, entries: Vec<acp::PlanEntry>) -> bool` (bumps the chat version only when the plan changed).
  - `TeamClient::hello` returns `Err(TeamClientError::Incompatible { protocol })` when `protocol != TEAM_PROTOCOL_VERSION` or `acp_crate != TEAM_ACP_CRATE_VERSION`.
- Produces (in `team.rs`): `pub(crate) fn team_entry(entry: &AgentThreadEntry, cx: &App) -> TeamEntry`:
  - `UserMessage` -> `Updates` of one `UserMessageChunk` per `message.chunks` item.
  - `AssistantMessage` -> `Updates` of `AgentMessageChunk`/`AgentThoughtChunk` per chunk, text from `block.to_markdown(cx)`.
  - `ToolCall` -> `Updates` of one full `acp::SessionUpdate::ToolCall` carrying id, title (label source), kind, status (`WaitingForConfirmation` maps to `Pending`), locations, raw input/output, the tool call's `meta` as received, and content: text blocks as text, diffs as `acp::Diff { path, old_text, new_text }`, terminals as a text block of the terminal's output so far.
  - `CompletedPlan` -> `CompletedPlan`; `ContextCompaction` -> `Compaction`.

- [ ] **Step 1: Write the failing tests**
  - `team_share_service.rs`: `acp_crate_version_matches_the_dependency_pin` asserts `include_str!("../../../../Cargo.toml")` contains `agent-client-protocol = { version = "={TEAM_ACP_CRATE_VERSION}"` (catches bumping ACP without bumping the wire check).
  - `team_share_service.rs`: `hello_rejects_a_different_acp_version` runs a `TeamShareServer` (or the hello-checking function the client uses) answering `acp_crate: "0.13.0"` and asserts `Incompatible`.
  - `team.rs` (`#[gpui::test]`): `team_entries_rebuild_the_same_thread` builds an `AcpThread` on a stub connection, feeds it a user message, a streamed reply with thinking, and a completed tool call with a diff and text output via `handle_session_update`, converts every entry with `team_entry`, applies the `Updates` to a fresh thread in order, and asserts both threads' `to_markdown(cx)` are equal (catches lost content in the conversion).
- [ ] **Step 2: Run** `cargo test -p mnemonic-core team_share` and `cargo test -p mnemonic-panels --lib team_entries_rebuild`; expect compile failures.
- [ ] **Step 3: Implement** the interfaces above; update the hub's chat storage, the server's hello and chat responses, and `team.rs` publishing (also call `set_chat_plan` from the thread subscription when the plan changes). Delete `TeamToolStatus` and the old `TeamEntry` variants. Until Task 4 deletes `team_view.rs`, keep it compiling with a minimal shim: it shows an `Updates` entry as the concatenated text of its chunks and a tool call by its title.
- [ ] **Step 4: Run** the tests from Step 2 and `cargo clippy -p mnemonic-core -p mnemonic-panels --no-deps`; expect PASS and no new warnings.
- [ ] **Step 5: Commit** `Send shared chats as agent updates`.

### Task 2: Teammate connection that mirrors a chat into a real thread

**Files:**
- Modify: `crates/acp_thread/src/acp_thread.rs` (new `truncate_entries`)
- Create: `crates/mnemonic-panels/src/team_connection.rs`
- Modify: `crates/mnemonic-panels/src/lib.rs` (module)

**Interfaces:**
- Consumes: `TeamEntry`, `TeamChatUpdate`, `TeamClient::chat` (Task 1).
- Produces:
  - `AcpThread::truncate_entries(&mut self, len: usize, cx: &mut Context<Self>)`: drops entries from `len`, emits `AcpThreadEvent::EntriesRemoved(len..old_len)`; no-op when `len >= entries.len()`.
  - `pub(crate) struct TeamAgentServer { host: String, client: TeamClient }` implementing `AgentServer` (`agent_id` = `AgentId(format!("team:{host}").into())`, `connect` returns an `Rc<TeamConnection>`).
  - `pub(crate) struct TeamConnection` implementing `AgentConnection`: `supports_load_session() == true`; `load_session(session_id = chat id, project, work_dirs, title, cx)` creates the `AcpThread` and starts a long-poll task (kept in the connection keyed by session id, dropped by `close_session`); `new_session`, `prompt`, `authenticate` return an error "This is a teammate's chat; it is read-only"; `cancel` does nothing; `auth_methods` is empty.
  - `pub(crate) struct ChatMirror { applied: Vec<TeamEntry> }` with `fn apply(&mut self, thread: &mut AcpThread, update: TeamChatUpdate, cx: &mut Context<AcpThread>)`:
    - `reset`: `truncate_entries(0)`, clear `applied`, then apply all changes.
    - change at `index == applied.len()`: apply its updates with `handle_session_update`.
    - `Updates` holding a `ToolCall` at an existing index: apply the `ToolCall` (upsert by id, in place).
    - user or assistant entry at the last index whose new chunks extend the old ones: apply only the new suffix (new text of the last chunk, then any new chunks).
    - anything else: `truncate_entries(index)` and re-apply `applied[index..]` with the change in place.
    - `CompletedPlan`: `update_plan` with its entries then `snapshot_completed_plan`. `Compaction`: `push_context_compaction`.
    - Always `update_plan(acp::Plan::new(update.plan))` last.
  - Poll loop mirrors today's `team_view.rs` `ChatPoll`: `client.chat(id, cursor, MAX_CHAT_WAIT)` on the background executor, `RETRY_CHAT_AFTER` (5 s) on error or when the chat is not open; it never ends the thread on error.

- [ ] **Step 1: Write the failing tests** (`#[gpui::test]` in `team_connection.rs`, fake `TeamChatUpdate`s, no network):
  - `streaming_reply_appends_only_the_new_text`: apply entry 1 assistant "Hel", then "Hello"; assert one assistant entry whose markdown is "Hello" (catches duplicated text).
  - `tool_call_update_in_the_middle_keeps_later_entries`: entries [user, tool call pending, assistant]; then change index 1 to completed; assert 3 entries, tool call status completed, assistant text intact.
  - `reset_rebuilds_the_thread`: apply 3 entries, then a reset with 1 entry; assert 1 entry.
  - `prompting_a_teammate_chat_is_refused`: `connection.prompt(..)` resolves to an error.
- [ ] **Step 2: Run** `cargo test -p mnemonic-panels --lib team_connection`; expect compile failures.
- [ ] **Step 3: Implement** `truncate_entries`, `ChatMirror`, `TeamConnection`, `TeamAgentServer`.
- [ ] **Step 4: Run** the tests, `cargo test -p acp_thread`, `cargo clippy -p acp_thread -p mnemonic-panels --no-deps`; expect PASS.
- [ ] **Step 5: Commit** `Mirror a teammate's chat into a real agent thread`.

### Task 3: Read-only chat view

**Files:**
- Modify: `crates/agent_ui/src/conversation_view.rs`, `crates/agent_ui/src/conversation_view/thread_view.rs`

**Interfaces:**
- Produces: `ConversationView::set_read_only(&mut self, notice: SharedString)`. While set, `ThreadView` renders the notice (muted label, same padding as the message editor area) instead of the message editor and its toolbar, and the fork button (`fork_enabled`) stays off.

- [ ] **Step 1: Implement** as above.
- [ ] **Step 2: Run** `cargo check -p agent_ui` and `cargo clippy -p agent_ui --no-deps`.
- [ ] **Step 3: Commit** `Add a read-only mode to the chat view`.

No unit test: render-only, exercised in Task 6.

### Task 4: Teammate workspace in the main window

**Files:**
- Create: `crates/mnemonic-panels/src/team_workspace_view.rs` (`TeammateWorkspaceView`)
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`WorkspaceHost`)
- Modify: `crates/mnemonic-panels/src/team.rs` (sidebar teammate row click, selected highlight)
- Modify: `crates/mnemonic-panels/src/team_settings.rs` (`TeamWindow` keeps only settings; drop `TeamWindowTab::Teammate`)
- Delete: `crates/mnemonic-panels/src/team_view.rs`

**Interfaces:**
- Consumes: `TeamAgentServer` (Task 2), `ConversationView::set_read_only` (Task 3).
- Produces:
  - `pub(crate) fn teammate_workspace_key(host: &str, workspace_id: &str) -> String` = `format!("team:{host}:{workspace_id}")`; this key is what `ActiveWorkspace` holds while a teammate workspace is selected, so local lookups by id simply miss.
  - `WorkspaceHost::activate_teammate(workspace: &mut Workspace, host: String, workspace_id: String, window: &mut Window, cx: &mut Context<Workspace>)`, same host-finding as `activate_workspace`. `WorkspaceHost.active` becomes an enum `ActiveView { Local(String), Teammate(String) }`; `active_view()` (local) returns `None` for a teammate, so window items are not adopted into it.
  - `TeammateWorkspaceView::new(host: String, workspace_id: String, workspace: WeakEntity<Workspace>, project: Entity<Project>, window, cx)`: a header like today's `team_view.rs` (name, branch, PR state, agent state, change counts), then a tab row with one tab per `TeamChat`, each a `ConversationView` built with `TeamAgentServer`, `resume_session_id = Some(chat.id)`, `set_read_only("{peer name}'s workspace. Read-only.")`, `suppress_notifications()`. Chats with `open == false` show the existing "isn't open on {peer}'s machine" placeholder until they open, then get their view. New chats appear as tabs; removed chats keep their tab marked "closed on {peer}'s machine".
  - A banner under the header when the peer is not `PeerStatus::Online` ("{peer} is offline") or the workspace is no longer in their list ("{peer} stopped sharing this workspace"); content stays visible.
  - The sidebar teammate row calls `activate_teammate` and shows the selected highlight when `ActiveWorkspace` equals its key.

- [ ] **Step 1: Implement** the above; remove `team_view.rs` and the Team window's teammate tabs.
- [ ] **Step 2: Run** `cargo test -p mnemonic-panels --lib` and `cargo clippy -p mnemonic-panels --no-deps`; expect PASS (no new test: UI wiring, exercised in Task 6).
- [ ] **Step 3: Commit** `Open a teammate's workspace in the main window`.

### Task 5: Teammate changes in the right panel

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (diff splitting)
- Modify: `crates/mnemonic-panels/src/workspace_detail_panel.rs` (teammate mode)
- Modify: `crates/mnemonic-panels/src/team_workspace_view.rs` (open a file's diff as a tab)

**Interfaces:**
- Produces:
  - `pub struct TeamFileDiff { pub path: String, pub additions: u32, pub deletions: u32, pub diff: String }`
  - `pub fn split_unified_diff(diff: &str) -> Vec<TeamFileDiff>`: one item per `diff --git a/X b/Y` section, `path` from `b/` (or `a/` for deletions, `+++ /dev/null`), counts from `+`/`-` lines excluding `+++`/`---`.
  - `TeammateWorkspaceView::files(&self) -> &[TeamFileDiff]` and `open_file_diff(&mut self, path: &str, window, cx)`: a read-only `Editor` tab on the file's diff text with the "Diff" language, title = file name; re-opening focuses the existing tab.
  - `WorkspaceDetailPanel`: when the selected key is a teammate key, render the file list with the same row look as the local Changes list (`DiffStat` counts), clicking calls `open_file_diff`; refresh button reloads the teammate diff. The local-only sections (PR actions, checkpoints, ports) are hidden.
  - The teammate diff loads when the view opens and on refresh, using `TeamClient::diff`.

- [ ] **Step 1: Write the failing tests** in `team_share_service.rs`:
  - `split_unified_diff_lists_each_file_with_counts`: a two-file diff (one modified, one new) gives two items with the right paths and counts.
  - `split_unified_diff_names_deleted_files`: a deletion (`+++ /dev/null`) uses the `a/` path.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core split_unified_diff`; expect FAIL.
- [ ] **Step 3: Implement** the above.
- [ ] **Step 4: Run** the tests and `cargo clippy -p mnemonic-core -p mnemonic-panels --no-deps`; expect PASS.
- [ ] **Step 5: Commit** `Show a teammate's changed files in the right panel`.

### Task 6: Verify in the app

- [ ] **Step 1:** Build and install the Dev app (`script/bundle-mac -i`, then the manual signing steps from memory), restore `crates/zed/Cargo.toml`.
- [ ] **Step 2:** Share a workspace, add this Mac's own Tailscale name to `team.peers` (own login is always allowed), click it in the sidebar's Team section. Expect it in the main window, chats as tabs drawn like local chats, read-only notice in place of the message box, files listed in the right panel, clicking a file opens its diff.
- [ ] **Step 3:** Send a message in the local chat; watch the mirror stream it, tool calls update in place.
- [ ] **Step 4:** Unshare the workspace; the mirror shows "stopped sharing" and keeps its content.
- [ ] **Step 5:** Check the log for panics.
