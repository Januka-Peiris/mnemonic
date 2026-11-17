# Phone Page v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the phone page a place to start, drive and ship work: a "Needs you" home grouped by repo, New task, a thumb-friendly chat, Create PR and Merge, and web push alerts with Allow/Deny, served over Tailscale HTTPS.

**Architecture:** Extends v1 (PR #33). `phone_server.rs` (mnemonic-core) gains read routes, action routes, TLS and push routes; `phone.rs` (mnemonic-panels) carries the new actions out on the main thread, opening workspaces in the front window when needed, and sends web pushes beside ntfy. A new `web_push_service.rs` does VAPID and RFC 8291 encryption. The page stays one embedded HTML file, plus an embedded service worker, manifest and icon.

**Tech Stack:** Rust, GPUI, tiny_http 0.12, reqwest (blocking), serde, SQLite via the existing `Database`, RustCrypto P-256/HKDF/AES-GCM, vanilla JS.

**Spec:** `docs/superpowers/specs/2026-11-17-phone-page-v2-design.md` (builds on `docs/superpowers/specs/2026-11-17-phone-page-design.md`).

## Global Constraints

- Never use em dashes anywhere (code, comments, strings, commits).
- Rust rules from `CLAUDE.md`: no `unwrap()`/panicking indexing, no `let _ =` on fallible calls (use `.log_err()` or handle), no `mod.rs`, full-word names, comments only for non-obvious "why".
- v1 caller rules hold for every new route: owner Tailscale login only, refuse the Mac's own addresses, POST needs same-origin `Origin`, JSON `Content-Type` and the `X-Mnemonic-Phone` header.
- The team server, its routes and what teammates receive on the wire do not change.
- Alert text stays v1's generic lines (e.g. "needs permission to run a command"); never a command line, message text or diff in an alert.
- The page and its assets are embedded with `include_str!`/`include_bytes!`; no CDN, no build step.
- No-window error text, verbatim: `Open Mnemonic on the Mac first` (HTTP 409).
- Quick replies, verbatim and in order: `Yes, go ahead`, `Continue`, `Open a PR`.
- Notification button labels, verbatim: `Allow once`, `Deny`. Replacement texts: `Allowed`, `Denied`, `Already answered`.
- Certificate renewal: re-run `tailscale cert` at start and once a day.
- Rows must not overflow at 360 px wide.
- Run clippy as `./script/clippy -p <crate>`; 54 `disallowed_methods` errors already exist in other files, so judge only the files you touched.
- Tests: `cargo test -p mnemonic-core -p mnemonic-panels`.

## Review Focus

1. A message, New chat or Create PR for a workspace not open on the Mac: the workspace opens in the front window and the action lands (not a 409, not a silent drop). Test in Task 2.
2. Starting a task while the agent is still connecting: the first message is queued and sent once connected, and the branch is renamed from the chat title as on the Mac. Test in Task 2.
3. A push subscription that has expired (404/410) is deleted, not retried forever. Test in Task 4.
4. Over the HTTP fallback, the page must not show Turn on alerts or the mic as if they work. Covered by feature checks in Task 6/7 (page JS, manual check).
5. Tapping Allow on a notification after the Mac already answered shows `Already answered`, not an error. Test in Task 2 (409 path) plus Task 7 manual check.

---

### Task 1: Read routes and chat turn info

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (`TeamChat`)
- Modify: `crates/mnemonic-core/src/services/phone_server.rs`
- Modify: `crates/mnemonic-core/src/models/pr_draft.rs` (`WorkspacePrStatus::can_merge`)
- Modify: `crates/mnemonic-panels/src/workspace_detail_panel.rs` (use `can_merge` from the model)
- Modify: `crates/mnemonic-panels/src/chat_feed.rs`
- Modify: `crates/mnemonic-panels/src/phone.rs` (wire the new sources)

**Interfaces:**
- Produces:
  - `TeamChat` gains, all `#[serde(default, skip_serializing_if = ...)]` so the team wire is byte-for-byte unchanged when unset: `last_turn_ended_at: Option<u64>` (unix ms), `last_turn_errored: bool`, `waiting: bool` (a permission prompt is pending).
  - `ChatFeed` fills those three only when its scope is `PermissionScope::All` (the phone feed). Turn-end info lives in a `HashMap<String, (u64, bool)>` on the feed, set on `TurnEnded`/`Errored`, in memory only.
  - `impl WorkspacePrStatus { pub fn can_merge(&self) -> bool }` with the exact rule now in `WorkspaceDetailPanel::can_merge` (which is replaced by calls to it).
  - In `phone_server.rs`: `pub struct PhoneRepository { pub id: String, pub name: String }`; `pub struct PhonePrStatus { found, number: Option<i64>, title: Option<String>, url: Option<String>, state: Option<String>, is_draft: bool, merge_state_status: Option<String>, head_ref_oid: Option<String>, checks_summary: String, can_merge: bool, merge_method: String }` (Serialize).
  - `pub struct PhoneSources { pub diff: DiffSource, pub agents: PhoneAgentSource, pub repositories: Arc<dyn Fn() -> Result<Vec<PhoneRepository>, String> + Send + Sync>, pub pr_status: Arc<dyn Fn(&str) -> Result<PhonePrStatus, String> + Send + Sync> }`. `PhoneServer::start(config, hub, identity, sources: PhoneSources, action_handler)` replaces the separate `diff_source`/`agent_source` parameters.
  - Routes: `GET /v1/repositories` → `{ "repositories": [PhoneRepository] }`, most recently used first (order by the latest `created_at` of a workspace in that repo, then name). `GET /v1/workspaces/<id>/pr` → `PhonePrStatus`, 404 when the hub has no such workspace. Runs `get_workspace_pr_status` on the request thread.

- [ ] **Step 1: Write the failing tests**
  - `phone_server.rs`: `lists_repositories` (GET `/v1/repositories` with a source returning two repos → 200 and both names in the body); `unknown_workspace_pr_is_404`.
  - `chat_feed.rs`: `team_feed_leaves_turn_info_unset`: a `StepInOnceOnly` feed publishing a chat whose turn ended serializes a `TeamChat` with no `last_turn_ended_at`, `last_turn_errored` or `waiting` keys; `phone_feed_records_a_turn_end`: the `All` feed sets `last_turn_ended_at` after `TurnEnded` and `last_turn_errored: true` after `Errored`.
- [ ] **Step 2: Run them and see them fail**: `cargo test -p mnemonic-core phone_server` and `cargo test -p mnemonic-panels chat_feed`.
- [ ] **Step 3: Implement** the fields, the feed bookkeeping, `can_merge`, `PhoneSources` and both routes; build the sources in `phone.rs::start_server` (repositories from `repository_repository::list` plus workspace dates; PR status from `pr_draft_service::get_workspace_pr_status` and `configured_merge_method`).
- [ ] **Step 4: Run the tests and see them pass**, plus `cargo test -p mnemonic-core -p mnemonic-panels`.
- [ ] **Step 5: Commit**: `git commit -m "Give the phone repositories, PR status and chat turn info"`.

### Task 2: Start, ship, and act on workspaces not open on the Mac

**Files:**
- Modify: `crates/mnemonic-core/src/services/phone_server.rs` (`PhoneAction`, `PhoneActionOutcome`, `parse_action`)
- Modify: `crates/mnemonic-panels/src/phone.rs` (`run_phone_action`, action loop)
- Modify: `crates/mnemonic-panels/src/workspace_panel.rs` (factor creation out of `add_worktree`)
- Modify: `crates/mnemonic-panels/src/workspace_detail_panel.rs` (factor the Create PR instruction)

**Interfaces:**
- Consumes: `WorkspacePrStatus::can_merge`, `PhonePrStatus` (Task 1).
- Produces:
  - `PhoneAction` gains `StartTask { repository_id: String, agent: String, text: String }`, `CreatePr { workspace_id: String }`, `Merge { workspace_id: String, head: String }`.
  - `PhoneActionOutcome` gains `workspace_id: Option<String>` and `message: Option<String>` (the merge result text).
  - Routes: `POST /v1/tasks` `{repository_id, agent, text}` (text non-empty, ≤ `MAX_MESSAGE_BYTES`); `POST /v1/workspaces/<id>/pr` `{}`; `POST /v1/workspaces/<id>/merge` `{head}` (non-empty).
  - `WorkspacePanel::create_project_workspace(&mut self, repo: DiscoveredRepository, agent: ChatAgent, window, cx) -> Task<Result<(WorkspaceDetail, String), String>>` returning the detail and the primary chat record id; `add_worktree` uses it with `ChatAgent::default_agent(cx)`.
  - `pub(crate) fn request_pr(workspace: &mut Workspace, workspace_id: &str, branch: &str, cx: &mut Context<Workspace>) -> bool` holding the Create PR prompt and the `keep_ci_green` auto-fix arming; `WorkspaceDetailPanel::create_pr_per_settings` calls it.
  - `run_phone_action` returns `Task<Result<PhoneActionOutcome, TeamActionError>>` (sync arms use `Task::ready`); the action loop awaits it. Request-side wait: `StartTask` and `Merge` 60 s, others keep `ACTION_TIMEOUT` (10 s).
  - `fn front_window(cx: &App) -> Option<Entity<Workspace>>`: the frontmost Mnemonic window (first in `cx.window_stack()` that is a `Workspace`, else the first open window). None → `TeamActionError::NothingToDo("Open Mnemonic on the Mac first")`.
  - `fn open_workspace_for_phone(workspace_id: &str, cx: &mut App) -> Result<(Entity<Workspace>, Entity<WorkspaceView>), TeamActionError>`: if `find_workspace_window` has it, use that; otherwise select it in the front window's `WorkspacePanel` via `select_workspace` and return the loaded view.

Behaviour:
- `Message`, `NewChat`, `CreatePr` call `open_workspace_for_phone` first. `Message` to a saved chat finds the chat by record id after opening, then `send_to_chat`.
- `Stop` on a chat not loaded → `NothingToDo("The agent isn't running")`. `Permission` on a chat not loaded → `NothingToDo("Already answered")`.
- `StartTask`: unknown repository id → `Invalid` before anything is created; agent must be enabled; `create_project_workspace` in the front window; then `select_workspace` and `send_to_chat(primary_chat_id, text)` (an ordinary user message, so the title-based rename runs); outcome carries both ids.
- `Merge`: never opens a workspace. Fetch status on a background thread; `!can_merge()` → `NothingToDo("This PR can't be merged right now")`; `head_ref_oid != head` → `NothingToDo("The PR changed since you looked. Refresh and try again.")`; else `merge_workspace_pr(state, id, number, url, head, configured_merge_method)`; `gh` errors return as `Failed` verbatim.
- Mac notices: `started a task in <repo>`, `asked for a PR in <workspace>`, `merged PR #<n>`.

- [ ] **Step 1: Write the failing tests**
  - `phone_server.rs`: `refuses_a_task_from_another_origin` (POST `/v1/tasks` with a foreign `Origin` → 403); `parses_new_actions` (the three routes produce the three `PhoneAction`s; empty `text` and empty `head` → 400).
  - `phone.rs` (gpui): `message_opens_a_workspace_not_on_screen`: a saved chat in a workspace not loaded in the window receives the message after `run_phone_action` and the workspace is now loaded; `no_window_says_open_mnemonic`; `start_task_refuses_an_unknown_repository` (no workspace row created); `start_task_queues_the_first_message` (the message sits in the primary chat's `pending_messages` while the agent connects).
  - `phone.rs`: `merge_refuses_a_moved_head` using a pure `fn check_merge(status: &WorkspacePrStatus, head: &str) -> Result<i64, TeamActionError>`.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement** the factoring, the async action loop, the new actions and routes.
- [ ] **Step 4: Run the tests and see them pass**, plus the two crates' full suites.
- [ ] **Step 5: Commit**: `git commit -m "Start tasks, open PRs and merge from the phone"`.

### Task 3: Serve the phone page over Tailscale HTTPS

**Files:**
- Modify: `crates/mnemonic-core/Cargo.toml` (enable tiny_http's `ssl-rustls` feature, or plug TLS another way)
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (`tailscale_cert`)
- Modify: `crates/mnemonic-core/src/services/phone_server.rs`
- Modify: `crates/mnemonic-panels/src/phone.rs`, `crates/mnemonic-panels/src/team_settings.rs`

**Interfaces:**
- Produces:
  - `pub struct TlsFiles { pub certificate: Vec<u8>, pub private_key: Vec<u8> }`.
  - `pub fn tailscale_cert(dns_name: &str, directory: &Path) -> Result<TlsFiles, String>`: runs `tailscale cert --cert-file <dir>/phone.crt --key-file <dir>/phone.key <dns_name>`, sets the key file mode to 0600, reads both. The error string carries `tailscale`'s own stderr.
  - `PhoneServerConfig` gains `tls: Option<TlsFiles>`. With TLS the server listens with HTTPS only and the same-origin check expects `https://{host}`; without, `http://{host}` as now.
  - `PhoneShare` gains `https_error: Option<String>`; `pub(crate) fn phone_https_error(cx: &App) -> Option<String>`. `page_url` uses `https://` when TLS is on.
  - Directory: `<AppState.data_dir>/phone`.
- Behaviour: `start_server` tries `tailscale_cert` (skipped when `dns_name` is empty); on error it starts over HTTP and stores the reason. Once a day `PhoneShare` re-runs `tailscale cert` (it renews by itself near expiry) and restarts the server when the certificate bytes changed. The Team window's "This computer" card shows `Phone alerts are off: <reason>` when `https_error` is set.

- [ ] **Step 1: Write the failing test**: `origin_must_match_the_scheme`: a pure `fn is_same_origin(origin: &str, host: &str, https: bool) -> bool`; `https://h` passes only when `https`, `http://h` only when not.
- [ ] **Step 2: Run it and see it fail.**
- [ ] **Step 3: Implement** TLS start, cert fetch with fallback, daily renewal, the Team card line.
- [ ] **Step 4: Run tests**; then by hand: with HTTPS enabled in the tailnet, `curl -sI https://<dns_name>:47232/` from another tailnet device answers; with it disabled, the Team card shows the reason and HTTP still works.
- [ ] **Step 5: Commit**: `git commit -m "Serve the phone page over Tailscale HTTPS"`.

### Task 4: Web push sending

**Files:**
- Create: `crates/mnemonic-core/src/services/web_push_service.rs` (register in `services.rs`)
- Modify: `crates/mnemonic-core/src/db/migrations.rs`, create `crates/mnemonic-core/src/repositories/push_subscription_repository.rs` (register like its siblings)
- Modify: `crates/mnemonic-core/Cargo.toml`, root `Cargo.toml` if a new workspace dependency is needed

**Interfaces:**
- Produces:
  - `pub struct PushSubscription { pub endpoint: String, pub p256dh: String, pub auth: String }` (Serialize/Deserialize; keys base64url as browsers send them).
  - `pub struct VapidKeys`; `pub fn load_or_create_vapid_keys(directory: &Path) -> Result<VapidKeys, String>` storing the private key at `<dir>/vapid.key` mode 0600; `impl VapidKeys { pub fn public_key_base64url(&self) -> String }`.
  - `pub fn encrypt_with(plaintext: &[u8], subscription: &PushSubscription, server_private: &[u8; 32], salt: &[u8; 16]) -> Result<Vec<u8>, String>` (RFC 8291 `aes128gcm`, record size 4096) and `pub fn encrypt(plaintext, subscription) -> Result<Vec<u8>, String>` with a random key and salt.
  - `pub fn vapid_authorization(endpoint: &str, keys: &VapidKeys, now_unix: u64) -> Result<String, String>` returning `vapid t=<jwt>, k=<public key>`; JWT ES256 with `aud` = the endpoint's scheme+host, `exp` = now + 12 h, `sub` = `mailto:mnemonic@localhost`.
  - `pub enum PushOutcome { Delivered, Gone }`; `pub fn send(subscription: &PushSubscription, payload: &[u8], keys: &VapidKeys) -> Result<PushOutcome, String>` posting with headers `Content-Encoding: aes128gcm`, `TTL: 3600`, `Urgency: high`, `Authorization`. 404/410 → `Gone`; other non-2xx → `Err` with status and body.
  - Table `phone_push_subscriptions (endpoint TEXT PRIMARY KEY, p256dh TEXT NOT NULL, auth TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)`; repository `upsert(db, &PushSubscription)`, `list(db)`, `delete(db, endpoint)`.

- [ ] **Step 1: Write the failing tests**
  - `encrypts_the_rfc_8291_example`: copy the keys, salt, auth secret, plaintext and expected output verbatim from RFC 8291 Section 5 (https://www.rfc-editor.org/rfc/rfc8291#section-5); `encrypt_with` must reproduce the expected bytes exactly.
  - `vapid_token_names_the_push_service`: decode the JWT from `vapid_authorization("https://fcm.googleapis.com/fcm/send/abc", ..., 1_000)`; `aud == "https://fcm.googleapis.com"`, `exp == 1_000 + 43_200`, and it verifies against the public key.
  - `gone_on_404_and_410`: a pure `fn outcome_for_status(status: u16) -> Option<PushOutcome>` gives `Gone` for 404 and 410, `Delivered` for 201.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.** Prefer RustCrypto crates already in `Cargo.lock` (`p256`, `hkdf`, `sha2`) plus `aes-gcm`; `jsonwebtoken` may sign ES256 from the key's PKCS#8 form.
- [ ] **Step 4: Run the tests and see them pass.**
- [ ] **Step 5: Commit**: `git commit -m "Add web push sending"`.

### Task 5: Send alerts by web push

**Files:**
- Modify: `crates/mnemonic-core/src/services/phone_server.rs` (push routes)
- Modify: `crates/mnemonic-panels/src/chat_feed.rs` (`PermissionRequested` payload)
- Modify: `crates/mnemonic-panels/src/phone.rs` (`notify_idle_phone`)

**Interfaces:**
- Consumes: Task 4's service and repository; `PhoneSources` (Task 1).
- Produces:
  - `PhoneSources` gains `push_key: Arc<dyn Fn() -> Result<String, String> + Send + Sync>` and `save_push_subscription: Arc<dyn Fn(PushSubscription) -> Result<(), String> + Send + Sync>`.
  - Routes: `GET /v1/push/key` → `{ "key": "<base64url>" }`; `POST /v1/push/subscriptions` with a `PushSubscription` body (endpoint must start with `https://`) → 200.
  - `ChatFeedEvent::PermissionRequested` gains `tool_call_id: String` and `quick_answers: Option<QuickAnswers>`; `pub(crate) struct QuickAnswers { pub allow_option_id: String, pub deny_option_id: String }`; `pub(crate) fn quick_answers(options: &[acp::PermissionOption]) -> Option<QuickAnswers>`: the first `AllowOnce` and the first `RejectOnce` among the options the phone wire publishes for that request; None unless both exist.
  - Push payload JSON: `{ "title": "<repo> / <workspace>", "body": "<v1 generic line>", "chat_id", "url": "<page url>#chat=<chat id>", "permission": { "tool_call_id", "allow_option_id", "deny_option_id" } | null }`.
- Behaviour: `notify_idle_phone` keeps one gate (`should_notify` + the 30 s collapse) and then sends to ntfy when `ntfy_url` is set and to every stored subscription; a `Gone` deletes that subscription; other errors are logged with `log::warn!`. No subscriptions and no ntfy → nothing sent.

- [ ] **Step 1: Write the failing tests**: `quick_answers_need_both_once_options` (AllowOnce+RejectOnce → Some with their ids; AllowAlways+RejectOnce only → None); `refuses_a_non_https_push_endpoint` (POST with `http://` endpoint → 400).
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests and see them pass.**
- [ ] **Step 5: Commit**: `git commit -m "Send phone alerts by web push"`.

### Task 6: The page: home, new task, chat, ship

**Files:**
- Modify: `crates/mnemonic-core/src/services/phone_page.html`

**Interfaces:**
- Consumes: every route above; `TeamChat.waiting/last_turn_ended_at/last_turn_errored`; `PhonePrStatus`.
- Produces (behaviour, per the spec's sketches):
  - **Home:** "Needs you" first: chats with `waiting` (fetch `/v1/chats/<id>` without `wait` for the prompt; inline `Allow once`/`Deny` when the request has both an `allow_once` and a `reject_once` option, else just Open), then chats with `last_turn_ended_at` later than the chat's local-storage mark (`phone.opened.<chat id>`) and later than `phone.installedAt` (set on first run). Opening a chat writes its mark. Then workspaces grouped under repo headings (alphabetical), attention first, then most recent activity. Rows fit 360 px: name truncates with ellipsis, status and PR never clip.
  - **New task:** repo picker from `/v1/repositories`, textarea, mic, agent picker from `/v1/agents` (first selected), Start → `POST /v1/tasks` → open the returned chat.
  - **Chat:** header `<repo> / <workspace>` with Stop; consecutive tool calls folded into one tappable line (`▸ N tool calls`, or the single tool's title), never folding one waiting on permission; opens at the newest entry; pinned bottom with quick replies (`Yes, go ahead`, `Continue`, `Open a PR`), message box, mic, Send; an action row with New chat, Diff, and the PR button: `Create PR` when there's no open PR, `PR #n: Merge` when `can_merge`, else `PR #n` with its state. Merge asks `confirm()` showing title, merge method and the first 7 characters of `head_ref_oid`, then posts the head.
  - **Mic:** shown only when `window.SpeechRecognition || window.webkitSpeechRecognition` exists; fills the box, never sends by itself.
  - Errors from any POST show the server's text in the existing error banner.
- Keep v1's DOM rule: build with `textContent`, never `innerHTML` with data.

- [ ] **Step 1:** Implement the page changes.
- [ ] **Step 2:** Check by hand with the mock server (`/tmp/phone-mock/mock.py`, extended with the new routes) in Chrome device mode at 360 px: Needs you shows a waiting chat with buttons; New task opens the new chat; folding, quick replies and the Merge confirm work; nothing clips.
- [ ] **Step 3:** `cargo test -p mnemonic-core phone_server` still passes (`serves_the_page`).
- [ ] **Step 4: Commit**: `git commit -m "Rework the phone page for starting and shipping work"`.

### Task 7: Installable page and alert buttons

**Files:**
- Create: `crates/mnemonic-core/src/services/phone_sw.js`, `crates/mnemonic-core/src/services/phone_manifest.json`
- Modify: `crates/mnemonic-core/src/services/phone_server.rs`, `phone_page.html`

**Interfaces:**
- Produces:
  - Routes: `GET /manifest.webmanifest` (`application/manifest+json`), `GET /sw.js` (`text/javascript`, `Cache-Control: no-store`), `GET /icon.png` (`image/png`, `include_bytes!` of `crates/zed/resources/app-icon@2x.png`).
  - Manifest: `name` and `short_name` `Mnemonic`, `display: standalone`, `start_url: /`, theme and background colours matching the page's dark theme, one icon entry (`/icon.png`, its real size, `purpose: any`).
  - CSP on the page adds `img-src 'self'; manifest-src 'self'; worker-src 'self'`.
  - Page: registers `/sw.js` when `navigator.serviceWorker` exists; a `Turn on alerts` button (hidden when push is unsupported or the page isn't on `https:`, hidden once subscribed) that requests permission, subscribes with the key from `/v1/push/key`, and posts the subscription.
  - Service worker: `push` → `showNotification(title, { body, tag: chat_id, data, actions })` with actions `allow` (`Allow once`) and `deny` (`Deny`) only when `permission` is present. `notificationclick`: action `allow`/`deny` → `fetch('/v1/chats/<id>/permission', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Mnemonic-Phone': '1' }, body })`, then replace the notification (same tag, no actions) with `Allowed`/`Denied`, or `Already answered` on 409; a plain tap focuses an open page client or opens `url`. No caching or fetch handler.

- [ ] **Step 1: Write the failing test**: `serves_the_service_worker_as_javascript` (GET `/sw.js` → 200 with `Content-Type: text/javascript`; a wrong type makes Chrome refuse to register it).
- [ ] **Step 2: Run it and see it fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** Tests pass; by hand on the phone over HTTPS: install from Chrome, Turn on alerts, leave the Mac idle past the threshold, trigger a permission prompt, tap `Allow once` on the notification and see the agent continue; tap it again on a second alert after answering on the Mac and see `Already answered`.
- [ ] **Step 5: Commit**: `git commit -m "Make the phone page installable with actionable alerts"`.
