# Phone Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A page served by the Mac over Tailscale that lets Jay read, unblock, stop, message and start agent chats from his Android phone, with ntfy alerts when he is away from the Mac.

**Architecture:** A second HTTP server (`PhoneServer`, tiny_http, bound to the Mac's Tailscale IPv4) with its own `TeamShareHub` holding every active workspace, allowing only the Mac's own Tailscale login from a device other than the Mac. The publish-and-watch logic in `team.rs` moves into a reusable `ChatFeed` entity that both the team side and a new `PhoneShare` entity own. `PhoneShare` turns `ChatFeed` events into ntfy posts when the Mac is idle.

**Tech Stack:** Rust, GPUI, tiny_http, reqwest (blocking), serde, plain HTML/CSS/JS embedded with `include_str!`.

**Spec:** `docs/superpowers/specs/2026-11-17-phone-page-design.md`

## Global Constraints

- Bind only to the Mac's Tailscale IPv4 from `tailscale_self()`, never `0.0.0.0`.
- Allowed caller: login equal (case-insensitive, trimmed) to `tailscale_self().login`; source address must not be the Mac's own Tailscale IP or loopback.
- POSTs need `Origin` equal to `http://<Host header>`, `Content-Type: application/json`, and header `X-Mnemonic-Phone` non-empty. A GET with an `Origin` for another host is refused. `Host` must pass the team server's `is_allowed_host`.
- Default port `DEFAULT_TEAM_PORT + 1` (47232). Default `notify_after_idle_minutes` 5.
- ntfy text never contains message text, tool titles, tool input or diff content. Collapse repeats per (chat, kind) within 30 seconds.
- Page: one HTML file, inline CSS and JS, no CDN, no build step.
- Repo rules (`CLAUDE.md`): no `unwrap`/`expect` outside tests, no `let _ =` on fallible calls, no new `mod.rs`, full-word names, comments only for non-obvious why. No em dashes anywhere.
- The team server's routes, allow list, workspace set and browser refusal do not change.

## Review Focus

1. Phone server running while Tailscale reconnects or switches account: expect it to restart on the new address like the team server does (`watch_tailscale`), not keep listening on a dead IP. Test in Task 4.
2. A message sent while the chat's agent is mid-turn: expect it queued and delivered after the turn, not dropped or sent concurrently. Covered by reusing the existing `pending_messages` + `flush_pending` path unchanged; no new test.
3. Phone answers a permission that was already answered on the Mac: expect 409 "no longer waiting", and the page refreshes rather than erroring. Test in Task 4.
4. Workspace archived while the phone has it open: expect 404 on its routes, and the list drops it on the next poll. Test in Task 2 (hub without the workspace returns 404).
5. ntfy unreachable or slow: expect a logged warning and no stall of the main thread or the thread-event handler. Test in Task 5 (send runs off the main thread; failure returns `Err` that is logged).

---

### Task 1: Settings and the Mac's MagicDNS name

**Files:**
- Modify: `crates/settings_content/src/settings_content.rs` (add `phone: Option<PhoneSettingsContent>` beside `team` near line 200; struct beside `TeamSettingsContent` near line 1017)
- Modify: `assets/settings/default.json` (add a `"phone"` block after `"team"` at line ~1800)
- Modify: `crates/settings_ui/src/page_data.rs` (add phone items after `team_section()` near line 8500)
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (`TailscaleSelf` gains `dns_name`; `parse_tailscale_status` fills it)
- Create: `crates/mnemonic-panels/src/phone.rs` (only `PhoneSettings` in this task); register `mod phone;` in `crates/mnemonic-panels/src/lib.rs`

**Interfaces:**
- Produces: `PhoneSettingsContent { enabled: Option<bool>, port: Option<u16>, ntfy_url: Option<String>, ntfy_token: Option<String>, notify_after_idle_minutes: Option<u32> }`.
- Produces: `pub(crate) struct PhoneSettings { enabled: bool, port: u16, ntfy_url: String, ntfy_token: Option<String>, notify_after_idle_minutes: u32 }` implementing `Settings` (trims strings; empty token is `None`).
- Produces: `TailscaleSelf.dns_name: String`, the `Self.DNSName` from `tailscale status --json` with any trailing `.` removed.

- [ ] **Step 1: Write the failing test** in `team_share_service.rs` tests: `status_reports_the_magic_dns_name` feeds a status JSON whose `Self.DNSName` is `"jay-mbp.tail1234.ts.net."` and asserts `dns_name == "jay-mbp.tail1234.ts.net"`.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core status_reports_the_magic_dns_name`. Expected: FAIL (no field).
- [ ] **Step 3: Implement** the field, the settings content struct (doc comments in the `TeamSettingsContent` style with `Default:` lines), default.json block with one comment line per key, `PhoneSettings::from_settings`, and Settings UI items for `phone.enabled`, `phone.ntfy_url` and `phone.notify_after_idle_minutes` under a "Phone" section header.
- [ ] **Step 4: Run** `cargo test -p mnemonic-core status_reports_the_magic_dns_name` (PASS) and `cargo check -p mnemonic-panels -p settings_ui` (clean).
- [ ] **Step 5: Commit** `Add phone settings and the Mac's MagicDNS name`.

### Task 2: The phone HTTP server

**Files:**
- Create: `crates/mnemonic-core/src/services/phone_server.rs`; register in `crates/mnemonic-core/src/services/mod.rs`
- Modify: `crates/mnemonic-core/src/services/team_share_server.rs`: make `is_allowed_host`, `respond`, `text_response`, `json_response`, `query_value`, `truncate_diff` `pub(crate)`; no behaviour change.

**Interfaces:**
- Consumes: `TeamShareHub`, `CallerIdentity`, `DiffSource`, `TeamActionError`, `MAX_CHAT_WAIT`, `is_plain_id` from team modules.
- Produces:
  ```rust
  pub const PHONE_REQUEST_HEADER: &str = "X-Mnemonic-Phone";
  pub struct PhoneServerConfig { pub bind: IpAddr, pub port: u16, pub owner_login: String, pub own_addresses: Vec<IpAddr> }
  pub enum PhoneAction {
      Stop { chat_id: String },
      Permission { chat_id: String, tool_call_id: String, option_id: String },
      Message { chat_id: String, text: String },
      NewChat { workspace_id: String, agent: String },
  }
  pub struct PhoneActionOutcome { pub chat_id: Option<String> } // Serialize
  pub type PhoneActionHandler = Arc<dyn Fn(PhoneAction) -> Result<PhoneActionOutcome, TeamActionError> + Send + Sync>;
  pub struct PhoneServer; // start(config, hub, identity, diff_source, handler) -> Result<Self, String>; address() -> SocketAddr; Drop stops it
  ```
- Routes: `GET /v1/workspaces`, `GET /v1/chats/<id>?cursor=&wait=`, `GET /v1/workspaces/<id>/diff`, `POST /v1/chats/<id>/{stop,permission,message}`, `POST /v1/workspaces/<id>/chats`. Bodies: permission `{tool_call_id, option_id}`, message `{text}` (non-empty after trim, max 16 KiB), new chat `{agent}`. Error mapping as the team server (`Invalid` 400, `NotAllowed` 403, `NotLoaded` 409 with body "Open this workspace on the Mac first", `NothingToDo` 409, `Failed` 500).

- [ ] **Step 1: Write the failing tests** in `phone_server.rs`, using a `FixedLogin`-style identity and `raw_status`-style raw requests (copy the helpers' approach from `team_share_server.rs` tests; bind `127.0.0.1`, port 0, `owner_login: "Jay@Example.com"`, `own_addresses: vec![]` unless stated):
  - `refuses_another_login`: identity returns `bob@example.com`; `GET /v1/workspaces` is 403.
  - `refuses_the_macs_own_address`: `own_addresses: vec![127.0.0.1]`, owner login; `GET /v1/workspaces` is 403.
  - `refuses_a_post_from_another_origin`: POST message with `Origin: http://evil.example`, JSON, header set: 403.
  - `refuses_a_post_without_the_phone_header`: same-origin `Origin`, JSON, no header: 400.
  - `accepts_a_same_origin_message`: `Host: 127.0.0.1:<port>`, `Origin: http://127.0.0.1:<port>`, JSON, header set, body `{"text":"hi"}`: 200, and the recorded action equals `PhoneAction::Message { chat_id: "chat-1", text: "hi" }`.
  - `unknown_workspace_diff_is_404`: hub without the workspace, `GET /v1/workspaces/ws-x/diff`: 404.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core phone_server`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement** `phone_server.rs`. Same accept-loop and `MAX_ACTIVE_REQUESTS` cap as the team server; checks in this order: Host, Origin rules, own-address refusal, whois login match, then routing. Thread names `mnemonic-phone` / `mnemonic-phone-request`.
- [ ] **Step 4: Run** `cargo test -p mnemonic-core phone_server` (PASS) and `cargo test -p mnemonic-core team_share_server` (still PASS).
- [ ] **Step 5: Commit** `Add the phone server`.

### Task 3: Extract the chat feed from the team side

No behaviour change for teammates. This lets the phone side publish a different workspace set with all permissions.

**Files:**
- Create: `crates/mnemonic-panels/src/chat_feed.rs`; register in `lib.rs`
- Modify: `crates/mnemonic-panels/src/team.rs`: `TeamShare` holds `feed: Entity<ChatFeed>` instead of `hub`, `chat_watches`, `saved_chats`; `publish`, `watch_chats`, `publish_thread_event`, `share_saved_chats`, `replace_chat`, `publish_activity` move into `ChatFeed`. `load_shared_records` becomes `load_records(state, filter: Option<&HashSet<String>>)` (None = every active workspace) and moves too.

**Interfaces:**
- Produces:
  ```rust
  pub(crate) enum PermissionScope { StepInOnceOnly, All }
  pub(crate) struct WorkspaceFlags { step_in: bool, share_files: bool, edit_files: bool, agents: bool }
  pub(crate) struct ChatFeed; // Entity
  impl ChatFeed {
      pub fn new(hub: TeamShareHub, scope: PermissionScope) -> Self;
      pub fn hub(&self) -> &TeamShareHub;
      /// Publishes records into the hub and returns closed chats' transcripts to read.
      pub fn publish(&mut self, records: SharedRecords, flags: &dyn Fn(&str) -> WorkspaceFlags, cx: &mut Context<Self>) -> Vec<TranscriptToRead>;
      pub fn share_saved_chats(&mut self, saved: Vec<(TranscriptToRead, Vec<TeamEntry>)>);
  }
  pub(crate) enum ChatFeedEvent {
      PermissionRequested { workspace_id: String, chat_id: String, tool_kind: acp::ToolKind },
      TurnEnded { workspace_id: String, chat_id: String, chat_title: String, stop_reason: acp::StopReason },
      Errored { workspace_id: String, chat_id: String, chat_title: String },
  }
  impl EventEmitter<ChatFeedEvent> for ChatFeed {}
  ```
  With `PermissionScope::All`, `publish_activity` publishes `thread_permissions` without the step-in check and without `once_only_options` (all flat options, and each dropdown choice's allow and deny); action-choice prompts stay excluded. `publish` still sets `file_roots`/`agent_roots` only through the flags; the team side keeps owning those maps (pass them back via the return value or keep that part in `TeamShare::publish` wrapping `ChatFeed::publish`; either is fine as long as `team.rs` tests pass unchanged).

- [ ] **Step 1: Write the failing test** in `chat_feed.rs`: `all_scope_publishes_always_options` builds a thread waiting on `allow_always_and_once_options()` (move that helper and `thread_waiting_on` / `new_thread` from `team.rs` tests into a shared `#[cfg(test)]` module in `chat_feed.rs`), publishes it through a `ChatFeed` with `PermissionScope::All`, and asserts the hub's `chat_update(...).permissions[0].options` contains an `AllowAlways` option; the same with `StepInOnceOnly` and step-in on contains none.
- [ ] **Step 2: Run** `cargo test -p mnemonic-panels all_scope_publishes_always_options`. Expected: FAIL.
- [ ] **Step 3: Implement** the move and the scope switch. Emit `ChatFeedEvent`s from the thread subscription on `ToolAuthorizationRequested` (tool kind from the tool call), `Stopped`, and `Error`.
- [ ] **Step 4: Run** `cargo test -p mnemonic-panels team` and `cargo test -p mnemonic-panels chat_feed`. Expected: all PASS, team tests unchanged.
- [ ] **Step 5: Commit** `Move team chat publishing into a reusable chat feed`.

### Task 4: PhoneShare: run the server and carry out phone actions

**Files:**
- Modify: `crates/mnemonic-panels/src/phone.rs` (add `PhoneShare`, `init`)
- Modify: `crates/mnemonic-panels/src/lib.rs` (call `phone::init(cx)` after `team::init(cx)`)
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (two small methods below)
- Modify: `crates/mnemonic-panels/src/team.rs`: `answer_permission` takes a `scope: PermissionScope` so the phone can pick any offered option; make `find_open_chat`, `show_step_in_notice`, `wait_for_reply` reachable from `phone.rs`.

**Interfaces:**
- Consumes: `PhoneServer`, `PhoneAction`, `ChatFeed`, `PhoneSettings`, `load_records(state, None)`, `tailscale_self()`.
- Produces in `workspace_view.rs`:
  - `pub(crate) fn send_to_chat(&mut self, record_id: &str, text: String, cx: &mut Context<Self>) -> bool`: pushes onto that chat's `pending_messages` (not `app_messages`, as it is Jay's own message) and calls `flush_pending`; false if no such chat.
  - `pub(crate) fn start_chat(&mut self, agent: ChatAgent, window: &mut Window, cx: &mut Context<Self>) -> String`: `add_chat` returning the new record id.
  - `ChatAgent::from_agent_id` becomes `pub(crate)`.
- Produces in `phone.rs`: `pub(crate) struct PhoneShare` holding `server: Option<PhoneServer>`, `feed: Entity<ChatFeed>`, publish/action/tailscale-watch tasks; `pub(crate) fn phone_page_url(cx: &App) -> Option<String>` (`http://<dns_name>:<port>/`) for the Settings UI.

Behaviour: start when `enabled`, stop when disabled or on quit, restart when port changes or Tailscale's IP or login changes (same 15 s check as `watch_tailscale`). Publish every 2 s with `load_records(state, None)` and flags all false (phone never shares files or agents). Actions run on the main thread via the `PendingAction` channel pattern with a 10 s timeout. Mac toast text: `"From your phone: stopped the agent in <chat>"`, `"From your phone: allowed <tool> in <chat>"`, etc.

- [ ] **Step 1: Write the failing tests** in `phone.rs` (GPUI `TestAppContext`):
  - `phone_can_answer_with_an_always_option`: thread waiting on `allow_always_and_once_options()`; `answer_permission(.., PermissionScope::All)` with the AllowAlways option id returns `Ok` with kind `AllowAlways`.
  - `answer_after_the_mac_answered_is_nothing_to_do`: answer once, answer again: `Err(TeamActionError::NothingToDo(_))`.
  - `restarts_on_a_new_tailscale_address`: `PhoneShare::needs_restart(&running: TailscaleSelf, &now: TailscaleSelf) -> bool` returns true when `ip` or `login` differs, false when equal (extract this pure function for the test).
- [ ] **Step 2: Run** `cargo test -p mnemonic-panels phone`. Expected: FAIL.
- [ ] **Step 3: Implement** `PhoneShare`, the two `workspace_view.rs` methods, the `answer_permission` scope parameter, and the action runner (`Message` → `send_to_chat` on the view found by `find_open_chat`; `NewChat` → `find_workspace_window`, the `WorkspaceHost` view for the id, `start_chat` via `window.update`; not found → `NotLoaded`). Show the page URL under the Phone settings section when running.
- [ ] **Step 4: Run** `cargo test -p mnemonic-panels phone` and `cargo test -p mnemonic-panels team`. Expected: PASS.
- [ ] **Step 5: Commit** `Run the phone server and carry out phone actions`.

### Task 5: ntfy alerts while the Mac is idle

**Files:**
- Create: `crates/mnemonic-core/src/services/ntfy_service.rs`; register in `services/mod.rs`
- Modify: `crates/mnemonic-panels/src/phone.rs` (subscribe to `ChatFeedEvent`)

**Interfaces:**
- Produces:
  ```rust
  pub enum NoticeKind { Permission, Finished, Error }
  pub struct Notice { pub kind: NoticeKind, pub workspace_name: String, pub chat_id: String, pub chat_title: String, pub tool_kind: Option<acp::ToolKind> }
  pub struct NtfyRequest { pub url: String, pub title: String, pub body: String, pub click: String, pub token: Option<String> }
  pub fn ntfy_request(ntfy_url: &str, token: Option<&str>, page_url: &str, notice: &Notice) -> NtfyRequest;
  pub fn send(request: &NtfyRequest) -> Result<(), String>; // reqwest blocking POST, headers Title, Click, optional Authorization: Bearer; 10 s timeout
  pub fn parse_hid_idle_seconds(ioreg_output: &str) -> Option<u64>; // "HIDIdleTime" = <nanoseconds>
  pub fn mac_idle_seconds() -> Option<u64>; // runs `ioreg -c IOHIDSystem`
  pub fn should_notify(kind: &NoticeKind, idle_seconds: Option<u64>, idle_threshold_minutes: u32, stop_reason: Option<acp::StopReason>) -> bool;
  ```
  Body copy: Permission → `needs permission to ` + (`Execute` "run a command", `Edit`/`Delete`/`Move` "change files", `Read`/`Search` "read files", `Fetch` "fetch from the web", otherwise "use a tool"); Finished → `finished: <chat title>`; Error → `hit an error in <chat title>`. Title is the workspace name. Click is `<page_url>#chat=<chat_id>`. `should_notify` is false when idle is unknown or below the threshold, and false for `StopReason::Cancelled`.

- [ ] **Step 1: Write the failing tests** in `ntfy_service.rs`:
  - `permission_notice_has_no_tool_text`: notice with `tool_kind: Some(Execute)`, chat title `"Deploy"`; request body is exactly `"needs permission to run a command"`, click is `"http://jay-mbp.ts.net:47232/#chat=chat-1"`.
  - `quiet_while_the_mac_is_in_use`: `should_notify(&Finished, Some(60), 5, Some(EndTurn))` is false; `Some(301)` is true; `None` is false.
  - `quiet_for_a_cancelled_turn`: `should_notify(&Finished, Some(9999), 5, Some(Cancelled))` is false.
  - `reads_hid_idle_time`: `parse_hid_idle_seconds("    |   \"HIDIdleTime\" = 312000000000\n")` is `Some(312)`.
  - `send_reports_an_unreachable_server`: `send` to `http://127.0.0.1:1/topic` returns `Err`.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core ntfy_service`. Expected: FAIL.
- [ ] **Step 3: Implement** the service, then in `PhoneShare` subscribe to the feed's events: skip if `ntfy_url` empty; collapse repeats per (chat_id, kind) within 30 s; look up the workspace name from the hub; run `mac_idle_seconds`, `should_notify` and `send` on `cx.background_spawn`, logging a failed send with `log::warn!`.
- [ ] **Step 4: Run** `cargo test -p mnemonic-core ntfy_service`. Expected: PASS.
- [ ] **Step 5: Commit** `Send ntfy alerts from the phone server while the Mac is idle`.

### Task 6: The page

**Files:**
- Create: `crates/mnemonic-core/src/services/phone_page.html`
- Modify: `crates/mnemonic-core/src/services/phone_server.rs` (route `GET /` serving the file with `Content-Type: text/html; charset=utf-8` and `Cache-Control: no-store`)

**Interfaces:**
- Consumes: the Task 2 routes and JSON shapes (`TeamWorkspaceList`, `TeamChatUpdate`, `TeamEntry::Updates` carrying ACP `SessionUpdate`s, `PhoneActionOutcome`).

Page behaviour (follow the spec's two sketches): workspace list polling `/v1/workspaces` every 5 s, attention first (`waiting_for_approval`, then `error`, then `working`, then idle); chat view long-polling `/v1/chats/<id>?cursor=<c>&wait=25`, applying `reset`/`changes` by index; render user and agent message chunks, tool calls as one line with status, the plan, permission prompts as buttons; composer, Stop, Diff (plain `<pre>`), New chat (agent picker: Claude, Codex, Gemini, OpenCode, Cursor). Light markdown in its own JS: paragraphs, fenced code, inline code, bold, links. `#chat=<id>` opens that chat. All POSTs send `Content-Type: application/json` and `X-Mnemonic-Phone: 1`. Errors show the server's text in a dismissible banner. `prefers-color-scheme` for dark mode. Add `<meta name="viewport" content="width=device-width, initial-scale=1">` and `<title>Mnemonic</title>` for the home-screen shortcut. A tool call waiting for confirmation with no matching entry in `permissions` (an action choice) shows "Answer this on the Mac".

- [ ] **Step 1: Write the failing test** `serves_the_page`: `GET /` with the owner login returns 200, `text/html`, and a body containing `X-Mnemonic-Phone` (proves the served page is the one that sends the header the server requires).
- [ ] **Step 2: Run** `cargo test -p mnemonic-core serves_the_page`. Expected: FAIL.
- [ ] **Step 3: Implement** the page and the route.
- [ ] **Step 4: Run** `cargo test -p mnemonic-core phone_server` (PASS), then `./script/clippy -p mnemonic-core -p mnemonic-panels -p settings_content -p settings_ui` (clean).
- [ ] **Step 5: Manual check on the phone:** enable `phone.enabled`, open `http://<Mac MagicDNS>:47232/` in Android Chrome over Tailscale; read a live chat, send a message, approve a prompt, stop a turn, start a chat; leave the Mac untouched 5 minutes and confirm a permission prompt produces an ntfy alert whose tap opens that chat. Report anything not verified.
- [ ] **Step 6: Commit** `Add the phone page`.
