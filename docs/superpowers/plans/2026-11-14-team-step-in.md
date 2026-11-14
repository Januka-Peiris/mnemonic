# Team Step-In Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From a mirrored teammate chat, stop their agent, message it, and answer its permission requests, only when the sharer switched that on for the workspace.

**Architecture:** The share server gains three POST routes that hand an action to a handler; in the app the handler crosses to the GPUI foreground thread, where `TeamShare` runs the action on the live chat. The viewer keeps the real chat view from step 1: its Stop button, message editor and permission buttons drive those routes, and results come back through the existing mirror feed.

**Tech Stack:** Rust, GPUI, `tiny_http`, `agent-client-protocol` 0.14.0 types on the wire.

**Spec:** `docs/superpowers/specs/2026-11-14-team-step-in-design.md` (read it first; this plan argues from it).

## Global Constraints

- `TEAM_PROTOCOL_VERSION = 3`. No compatibility with v2.
- Every action needs: Tailscale identity on the allow list, AND the chat's workspace shared with step-in on. Otherwise 403.
- Only `AllowOnce` and `RejectOnce` permission options are sent or accepted.
- Message body limit 256 KB (413 above it). Text only.
- The action handler answers within 10 seconds or the request fails with 500.
- The viewer never adds entries to the mirrored thread itself.
- Teammate message text in the sharer's chat: `From {name} (teammate):\n\n{text}`.
- Never use em dashes anywhere (code, comments, strings, commit messages).
- No `unwrap()`, no `let _ =` on fallible operations, full words for names.
- `mnemonic-core` and `mnemonic-panels` are edition 2021: no let chains.
- Tests only for concrete behaviour named in this plan. No reassurance tests.
- Check with `cargo clippy -p <crate> --no-deps --tests` (the crates have old warnings; add none in files you touch). Do not run `cargo fmt` on whole files; format only your own lines.
- Each commit message ends with a blank line then `Written with Claude Code`.

## Review Focus

1. Step-in is switched off while a viewer still shows controls: the next action is refused with 403 and shows the reason; the viewer goes back to the read-only notice on its next workspace poll.
2. The chat closes on the sharer's Mac between the viewer's click and the action: 404 with a visible reason, no panic on either side.
3. Two teammates (or teammate and owner) answer the same permission request: the second gets 409 and its buttons clear.
4. An empty or whitespace-only message: not sent by the viewer, refused with 400 by the server.
5. The sharer's foreground thread is busy: the request thread gives up after 10 seconds with 500 instead of holding a server thread forever.

---

### Task 1: Protocol v3, hub state, action routes and client calls

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs`
- Modify: `crates/mnemonic-core/src/services/team_share_server.rs`
- Modify: `crates/mnemonic-panels/src/team.rs` (only the call sites this task breaks: `TeamShareServer::start`, `TeamWorkspace`/`TeamChat` literals, the identity trait)
- Test: the `tests` modules of the two core files

**Interfaces:**
- Produces (all `pub`, in `team_share_service.rs`):
  - `TEAM_PROTOCOL_VERSION: u32 = 3`
  - `TeamWorkspace.step_in: bool` and `TeamChat.live: bool`, both `#[serde(default)]`
  - `struct TeamPermissionRequest { tool_call_id: String, options: Vec<acp::PermissionOption> }`
  - `TeamChatUpdate.generating: bool` and `TeamChatUpdate.permissions: Vec<TeamPermissionRequest>`, both `#[serde(default)]`
  - `struct TeamCaller { login: String, name: String }`; `CallerIdentity::login` becomes `fn caller(&self, address: IpAddr) -> Result<Option<TeamCaller>, String>` (`name` is the Tailscale display name, falling back to the login)
  - `struct TeamAction { chat_id: String, from: String, kind: TeamActionKind }`
  - `enum TeamActionKind { Stop, Message { text: String }, Permission { tool_call_id: String, option_id: String } }`
  - `struct TeamActionOutcome { queued: bool }` (serializes as `{"queued": bool}`)
  - `enum TeamActionError { NotAllowed(String), NotLoaded, NothingToDo(String), Failed(String) }` mapping to 403, 404, 409, 500
  - `type ActionHandler = Arc<dyn Fn(TeamAction) -> Result<TeamActionOutcome, TeamActionError> + Send + Sync>`
  - Hub: `set_chat_activity(&self, chat_id: &str, generating: bool, permissions: Vec<TeamPermissionRequest>) -> bool` (false when the chat is unknown; bumps the version and wakes waiters only on change, like `set_chat_plan`), `set_step_in_chats(&self, chat_ids: HashSet<String>)`, `allows_step_in(&self, chat_id: &str) -> bool`
  - `TeamClientError::Refused { status: u16, reason: String }`
  - `TeamClient::stop(&self, chat_id: &str) -> Result<(), TeamClientError>`, `TeamClient::send_message(&self, chat_id: &str, text: &str) -> Result<bool, TeamClientError>` (the bool is `queued`), `TeamClient::answer_permission(&self, chat_id: &str, tool_call_id: &str, option_id: &str) -> Result<(), TeamClientError>`
- Produces (in `team_share_server.rs`): `TeamShareServer::start(config, hub, identity, diff_source, action_handler: ActionHandler)`

- [ ] **Step 1: Write the failing tests**

In `team_share_server.rs` tests (extend the existing `start` helper to take an `ActionHandler` that records every `TeamAction` it gets and returns `Ok(TeamActionOutcome { queued: true })`):

- `an_allowed_teammate_can_stop_message_and_answer_when_step_in_is_on`: hub has chat `chat-1` and `set_step_in_chats({"chat-1"})`. `client.stop("chat-1")` is `Ok(())`, `client.send_message("chat-1", "hello")` is `Ok(true)`, `client.answer_permission("chat-1", "call-1", "allow")` is `Ok(())`. The recorded actions are `Stop`, `Message { text: "hello" }`, `Permission { tool_call_id: "call-1", option_id: "allow" }`, each with `chat_id == "chat-1"` and `from` equal to the fake caller's `name`.
- `actions_are_refused_when_step_in_is_off`: same hub without `set_step_in_chats`. All three client calls return `Err(TeamClientError::Refused { status: 403, .. })` and the handler recorded nothing.
- `actions_are_refused_for_a_login_not_on_the_allow_list`: step-in on, caller not allowed: `stop` returns `Refused { status: 403, .. }`, handler recorded nothing.
- `an_empty_or_oversized_message_is_refused`: `send_message("chat-1", "  ")` is `Refused { status: 400, .. }`; a 300 KB text is `Refused { status: 413, .. }`; handler recorded nothing.
- `a_handler_error_reaches_the_client_with_its_reason`: handler returns `Err(TeamActionError::NothingToDo("The agent isn't running".into()))`; `client.stop` returns `Refused { status: 409, reason }` with that reason.

In `team_share_service.rs` tests:

- `chat_activity_bumps_only_on_change`: after `replace_chat`, `set_chat_activity("chat", true, vec![])` makes the next `chat_update` from the previous cursor carry `generating: true` with no entry changes; repeating the same call leaves a later cursor's `wait_chat_update(.., Duration::ZERO)` reporting no change (same cursor back); `set_chat_activity("missing", true, vec![])` is `false`.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p mnemonic-core --lib team_share`
Expected: compile errors for the missing items.

- [ ] **Step 3: Implement**

- Routes in `handle`: keep the identity and allow-list check first for every method. `GET` routes unchanged. `POST /v1/chats/{id}/stop|message|permission` with `is_plain_id(id)`: 403 `"Stepping in is off for this workspace"` unless `hub.allows_step_in(id)`; read at most 256 KB + 1 byte of body (413 past the limit); parse `{ "text" }` or `{ "tool_call_id", "option_id" }` (400 on bad JSON or blank text); call the handler; 200 with the outcome JSON, or the error's status with its reason as the text body. Any other method stays 405.
- Update the file's module comment: it is no longer GET-only.
- `TeamClient`: the three calls POST JSON; a non-200 answer becomes `Refused { status, reason: body }`.
- `TailscaleWhois` fills `TeamCaller.name` from the whois profile's display name when present.
- In `crates/mnemonic-panels/src/team.rs` make the crate compile again: pass a handler to `start` that returns `Err(TeamActionError::Failed("Not available yet".into()))` (Task 2 replaces it), set `step_in: false` and `live: thread.is_some()` in the published literals.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mnemonic-core --lib team_share && cargo check -p mnemonic-panels --tests`
Expected: all pass, clean check.

- [ ] **Step 5: Commit**

`git commit -m "Add step-in actions to the team share server"` (plus the trailer).

---

### Task 2: Sharer: the switch, publishing activity, and running actions

**Files:**
- Modify: `crates/mnemonic-panels/src/team.rs`
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (the type that owns `sessions` and `pending_messages`; see `dispatch_from` and `flush_pending`)
- Modify: `crates/mnemonic-panels/src/workspace_panel.rs` (workspace menu, next to the Share item near `set_workspace_shared`)
- Modify: `crates/mnemonic-panels/src/team_settings.rs` (the shared-workspace rows near `set_workspace_shared`)
- Test: `team.rs` tests module

**Interfaces:**
- Consumes: everything Task 1 produces.
- Produces (in `team.rs`):
  - `pub(crate) fn workspace_allows_step_in(workspace_id: &str, cx: &App) -> bool`
  - `pub(crate) fn set_workspace_step_in(workspace_id: &str, allowed: bool, cx: &mut App)`
  - `fn thread_permissions(thread: &AcpThread) -> Vec<TeamPermissionRequest>`
  - `fn answer_permission(thread: &Entity<AcpThread>, tool_call_id: &str, option_id: &str, cx: &mut App) -> Result<String, TeamActionError>` (returns the tool call's title for the notification)
  - `fn teammate_message_text(from: &str, text: &str) -> String`
- Wire behaviour later tasks rely on: `TeamWorkspace.step_in` true only when shared and switched on; `TeamChat.live` true only while the chat has a thread; `TeamChatUpdate.generating` follows the thread's status; `permissions` lists waiting requests only when step-in is on.

- [ ] **Step 1: Write the failing tests** (in `team.rs` tests, using the existing `new_thread` helper)

- `only_once_only_options_are_offered_to_teammates`: a thread with a tool call waiting on options `AllowOnce "allow"`, `AllowAlways "always"`, `RejectOnce "reject"` gives `thread_permissions` one request with option ids `["allow", "reject"]`. A waiting tool call offering only `AllowAlways` gives no request.
- `a_teammate_can_only_pick_an_offered_once_only_option`: on that first thread, `answer_permission(.., "always")` and `answer_permission(.., "never-offered")` are `Err(TeamActionError::NotAllowed(_))` and the tool call is still waiting; `answer_permission(.., "allow")` is `Ok(title)` and the tool call is no longer waiting; answering again is `Err(TeamActionError::NothingToDo(_))`.
- `a_teammates_message_carries_their_name`: `teammate_message_text("Dan", "try the other branch") == "From Dan (teammate):\n\ntry the other branch"`.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p mnemonic-panels --lib team::`
Expected: compile errors for the missing functions.

- [ ] **Step 3: Implement the switch**

- `TeamShare.step_in_workspaces: HashSet<String>`, loaded and saved like `shared_workspaces` under the core settings key `team_step_in_workspaces`. Unsharing a workspace removes it from the set.
- Sidebar workspace menu: a toggle entry "Let teammates step in" shown only for shared workspaces. Team window: the same toggle on each shared workspace row.

- [ ] **Step 4: Implement publishing**

- In `publish`: `step_in` per workspace; `hub.set_step_in_chats` with the live chats (those with a thread) of step-in workspaces.
- Wherever a chat's entries or plan are published (`replace_chat`, `publish_thread_event`), also call `hub.set_chat_activity(chat_id, generating, permissions)` with `generating = thread.status() == ThreadStatus::Generating` and `permissions = thread_permissions(thread)` when the chat's workspace has step-in on, else empty. A switch flipped while a chat is open must take effect by the next publish tick.

- [ ] **Step 5: Implement running actions**

- `start_server` passes a handler that sends `(TeamAction, reply sender)` to a foreground task held by `TeamShare` and waits up to 10 seconds for the reply (`TeamActionError::Failed("Timed out")` after that).
- The foreground side re-checks on the main thread that the chat's workspace is still shared with step-in on (`NotAllowed`), finds the live chat across `open_windows` (`NotLoaded` when it has no thread), then:
  - Stop: `NothingToDo("The agent isn't running")` when the thread is idle; otherwise the chat view's `cancel_generation` (the owner's Stop path, see spec).
  - Message: push `teammate_message_text(from, text)` onto the chat's `pending_messages`, flush, and report `queued` = the thread was not idle.
  - Permission: `answer_permission`.
- After a stop or a permission answer, show a notification in the sharer's window, using what `mnemonic-panels` already uses for notifications: `{from} stopped the agent in {chat title}` / `{from} allowed {tool title} in {chat title}` (or `rejected`).

- [ ] **Step 6: Run the tests and clippy**

Run: `cargo test -p mnemonic-panels --lib team:: && cargo clippy -p mnemonic-panels --no-deps --tests`
Expected: tests pass; no new warnings in touched files.

- [ ] **Step 7: Commit**

`git commit -m "Let a sharer allow teammates to step in on a workspace's agent"` (plus the trailer).

---

### Task 3: Viewer: working state, Stop, and sending messages

**Files:**
- Modify: `crates/acp_thread/src/acp_thread.rs`
- Modify: `crates/agent_ui/src/conversation_view.rs`, `crates/agent_ui/src/conversation_view/thread_view.rs`
- Modify: `crates/mnemonic-panels/src/team_connection.rs`, `crates/mnemonic-panels/src/team_workspace_view.rs`
- Test: `team_connection.rs` tests, `conversation_view.rs` tests

**Interfaces:**
- Consumes: `TeamClient::stop`, `TeamClient::send_message`, `TeamChatUpdate.generating`, `TeamWorkspace.step_in`, `TeamChat.live`.
- Produces:
  - `AcpThread::set_remote_generating(&mut self, generating: bool, cx: &mut Context<Self>)`. While set, `status()` is `ThreadStatus::Generating`; `cancel` with the flag set and no local turn calls `self.connection.cancel(&self.session_id, cx)`.
  - `ConversationView::set_send_handler(&mut self, handler: Option<Rc<dyn Fn(String, &mut Window, &mut App)>>, cx: &mut Context<Self>)`. With a handler set on a read-only view, the message editor shows instead of the notice, and sending passes the editor's text to the handler and clears the editor. It must run before `ThreadView::send`'s local "queue while generating" branch, and it never calls `thread.send`. Fork stays disabled and the view stays out of local thread history (both keyed on `read_only`, which stays set).
  - `TeamConnection::new(host: &str, client: TeamClient, report: Rc<dyn Fn(SharedString, &mut App)>)` and the same extra argument on `TeamAgentServer::new`. `report` shows a failure to the user. `TeamConnection::cancel` posts stop off the main thread and reports a failure's reason.

- [ ] **Step 1: Write the failing tests**

- `team_connection.rs`: `a_mirrored_chat_is_working_while_the_sharers_is`: applying an update with `generating: true` makes `thread.status() == ThreadStatus::Generating`; a following update with `generating: false` makes it `Idle`. Neither adds an entry.
- `conversation_view.rs` (follow the nearest existing test that builds a `ConversationView` with a stub connection): `a_send_handler_takes_the_message_and_leaves_the_thread_alone`: on a read-only view with a handler, typing `hello` and sending calls the handler once with `"hello"`, leaves the editor empty and the thread with zero entries; the same holds while the thread reports generating. Sending whitespace does not call the handler.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p mnemonic-panels --lib team_connection && cargo test -p agent_ui --lib send_handler`
Expected: compile errors for the missing items.

- [ ] **Step 3: Implement**

- `ChatMirror` turns `update.generating` into its own operation, applied in its own `thread.update` like the others.
- `team_workspace_view.rs`: in `sync_chats`, a chat can be stepped into when the workspace's `step_in` and the chat's `live` are both true. Set or clear the chat view's send handler to match on every sync, so switching step-in off returns the view to the read-only notice. The handler posts `send_message` off the main thread; on `Ok(true)` it shows `Queued until {peer_name}'s agent finishes`; on error it shows the reason. `report` and these notices use the notification mechanism the crate already uses.
- While a chat cannot be stepped into, keep it exactly as in step 1 (notice, no Stop button).

- [ ] **Step 4: Run the tests and clippy**

Run: `cargo test -p mnemonic-panels --lib team && cargo test -p agent_ui --lib send_handler && cargo test -p acp_thread --lib && cargo clippy -p acp_thread -p agent_ui -p mnemonic-panels --no-deps --tests`
Expected: pass; no new warnings in touched files.

- [ ] **Step 5: Commit**

`git commit -m "Stop and message a teammate's agent from its mirrored chat"` (plus the trailer).

---

### Task 4: Viewer: answering permission requests

**Files:**
- Modify: `crates/acp_thread/src/acp_thread.rs`
- Modify: `crates/mnemonic-panels/src/team_connection.rs`
- Test: `team_connection.rs` tests

**Interfaces:**
- Consumes: `TeamChatUpdate.permissions`, `TeamClient::answer_permission`, `AcpThread::request_tool_call_authorization`, the `report` callback from Task 3.
- Produces:
  - `AcpThread::clear_tool_call_authorization(&mut self, id: &acp::ToolCallId, cx: &mut Context<Self>)`: a tool call that is `WaitingForConfirmation` goes back to its `current_status` and emits `EntryUpdated`; its pending request resolves as cancelled. Anything else is left alone.
  - `apply_chat_update` gains an `answer: &Rc<dyn Fn(String, String, &mut App)>` argument, called with `(tool_call_id, option_id)` when the viewer picks an option. `poll_chat` passes one that posts `answer_permission` off the main thread and reports a failure's reason.

- [ ] **Step 1: Write the failing tests** (in `team_connection.rs`, with an `answer` that records its calls)

- `permission_buttons_appear_for_a_waiting_request`: after an update adding tool call `call-1` and listing a permission request for it with options `allow` (AllowOnce) and `reject` (RejectOnce), the thread's entry for `call-1` is `WaitingForConfirmation` with those two options.
- `picking_an_option_posts_the_answer_once`: then `thread.authorize_tool_call("call-1", allow outcome)`; after `run_until_parked` the recorded answers are `[("call-1", "allow")]`. Applying another update that still lists the request (the sharer has not processed the answer yet) does not put the tool call back to waiting and records nothing more.
- `a_request_answered_elsewhere_clears_without_posting`: from the waiting state, an update with empty `permissions` leaves `call-1` not waiting and the recorded answers empty.
- `a_reset_asks_again_for_requests_still_waiting`: from the waiting state, an update with `reset: true` carrying the same entry and the same request leaves `call-1` waiting, with no answers recorded.

- [ ] **Step 2: Run them and see them fail**

Run: `cargo test -p mnemonic-panels --lib team_connection`
Expected: compile errors, then failures.

- [ ] **Step 3: Implement**

Derive what to do from state after the update's entry changes are applied, in separate `thread.update` calls: request authorization for each listed request whose tool call exists, is not waiting, and was not answered here; clear each waiting tool call that is no longer listed. Remember the ids answered here until they leave `permissions`. A request is `PermissionOptions::Flat(options)` with `AuthorizationKind::PermissionGrant`. The spawned task that awaits the outcome calls `answer` only for `RequestPermissionOutcome::Selected`.

- [ ] **Step 4: Run the tests and clippy**

Run: `cargo test -p mnemonic-panels --lib team && cargo test -p acp_thread --lib && cargo clippy -p acp_thread -p mnemonic-panels --no-deps --tests`
Expected: pass; no new warnings in touched files.

- [ ] **Step 5: Commit**

`git commit -m "Answer a teammate agent's permission requests from the mirror"` (plus the trailer).
