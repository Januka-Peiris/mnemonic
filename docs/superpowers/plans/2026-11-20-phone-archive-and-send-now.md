# Phone Archive and Send-Now Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Phone messages sent while the agent works stop the turn and go straight through, and the phone can archive a workspace exactly like the Mac.

**Architecture:** Two Mac-side changes in `crates/mnemonic-panels` (an interrupting send for phone messages; a shared archive function behind a new phone route) and two Android changes (a pending bubble while a send is in flight; archive from Home and the chat menu).

**Tech Stack:** Rust (GPUI, tiny_http phone server), Kotlin with Jetpack Compose.

**Spec:** `docs/superpowers/specs/2026-11-20-phone-archive-and-send-now-design.md`

## Global Constraints

- Never use em dashes anywhere: code, comments, strings, docs, commit messages.
- Commit messages end with a blank line then `Written with Claude Code`.
- Rust: the repo `CLAUDE.md` rules (no `unwrap()`, no `let _ =` on fallible calls, `.log_err()`, full-word names). Format only touched Rust files with `rustfmt --edition 2024 <files>`; never `cargo fmt` the workspace.
- The phone server's caller rules don't change (owner login only, Mac's own addresses refused, JSON body, `X-Mnemonic-Phone`, same-origin `Origin`).
- Mac errors are shown on the phone verbatim.
- Android: keep AGP 8.10.1, Kotlin 2.4.21, Compose BOM and SDK versions; use the redesign's `MnColors` and `ui/Components.kt`; dense sizes as the rest of the app. Tests: `script/android test`.
- Emulator checks: AVD `pixel35` headless (`$HOME/Library/Android/sdk/emulator/emulator -avd pixel35 -no-snapshot-save -no-audio -no-window &`), always `ANDROID_SERIAL=emulator-5554` so nothing installs on Jay's phone, mock `python3 /tmp/phone-mock/mock.py crates/mnemonic-core/src/services/phone_page.html` on port 47299 (scratch; extend freely; stop it after), app connected to `http://10.0.2.2:47299`. Screenshots to `/tmp/android-archive-shots/`. Stop the emulator after (`adb -s emulator-5554 emu kill`).

## Review Focus

1. A phone message to a chat whose agent is mid-tool-call: the turn stops and the message is sent once, not twice, and not lost if the cancel takes a moment. Pinned in Task 1 (`phone_message_interrupts_busy_chat`).
2. A phone message to an idle chat: behaviour unchanged (sent at once, no cancel). Pinned in Task 1 (`phone_message_to_idle_chat_sends_without_cancel`).
3. Archiving a workspace whose `check_can_archive` refuses (for example unpushed work): the phone shows the Mac's reason and the row stays. Pinned in Task 2 (route test for the error mapping) and Task 4 (emulator check with the mock returning 400).
4. Two quick sends from the phone while busy: both bubbles show, in order, and both reach the agent. Pinned in Task 3 (`PendingMessagesTest.twoSendsKeepOrder`).
5. A swipe on a row that is mid-refresh or disappears during the confirm dialog: no crash; the dialog closes. Pinned in Task 4's emulator check.

---

### Task 1: Phone messages interrupt a busy chat

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`send_to_chat` / `queue_agent_message` area around lines 3591-3690, and its tests)
- Modify: `crates/mnemonic-panels/src/phone.rs` (`send_message` around line 1337 and the `PhoneAction::Message` arm around line 1273)

**Interfaces:**
- Produces: `pub(crate) fn send_now_to_chat(&mut self, record_id: &str, text: String, cx: &mut Context<Self>) -> Task<Result<(), String>>` on `WorkspaceView`: when the chat's root thread exists and its status isn't `ThreadStatus::Idle`, await `AcpThread::cancel`, then queue and flush the text through the existing pending path (so it is sent exactly once, when the thread is idle); otherwise behave as `send_to_chat` does today. Error `"That chat isn't open."` when the chat isn't found.
- Changes: `send_message` in `phone.rs` becomes async (returns `Task<Result<PhoneActionOutcome, TeamActionError>>`, using the 10 s short-action timeout already used for other actions) and calls `send_now_to_chat`. The Mac notice text and the open-workspace-first behaviour stay.

- [ ] **Step 1: Write failing tests** in `workspace_view.rs` tests using the existing harness (`StubAgentConnection`, `queue_typed_message`, `queued_message_count`):
  - `phone_message_interrupts_busy_chat`: with the stub thread generating (a pending turn), `send_now_to_chat("…", "carry on")` cancels the turn (the stub sees the cancel) and the next prompt the stub receives contains `"carry on"`; the message is sent exactly once.
  - `phone_message_to_idle_chat_sends_without_cancel`: idle thread; the stub receives the prompt and no cancel.
  If the harness can't hold a thread in a generating state, extract `fn phone_send_plan(status: Option<ThreadStatus>) -> PhoneSendPlan { Queue, SendNow, CancelThenSend }` and test that instead (None gives Queue, Idle gives SendNow, Generating gives CancelThenSend), and say so in the report.
- [ ] **Step 2: Run** `cargo test -p mnemonic-panels phone_message` and see them fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mnemonic-panels phone_message`, `cargo test -p mnemonic-panels phone`, `cargo check -p mnemonic-panels`. All pass.
- [ ] **Step 5: Commit** `Let phone messages stop a busy chat and go straight through`.

### Task 2: Archive route on the Mac

**Files:**
- Modify: `crates/mnemonic-core/src/services/phone_server.rs` (`PhoneAction::Archive`, route, tests)
- Modify: `crates/mnemonic-panels/src/agent_hub.rs` (factor the archive body)
- Modify: `crates/mnemonic-panels/src/phone.rs` (handle the action)

**Interfaces:**
- Produces: `PhoneAction::Archive { workspace_id: String }` from `POST /v1/workspaces/<id>/archive` (body `{}`; `is_plain_id` check like the other workspace routes).
- Produces: `pub(crate) async fn archive_workspace_by_id(workspace_id: String, fallback_window: Option<Entity<Workspace>>, cx: &mut AsyncApp) -> Result<(), String>` in `agent_hub.rs`, holding today's body of `agent_hub::archive_workspace` from `check_can_archive` through `forget_workspace`. Window choice: the workspace's own window, else `fallback_window`. `agent_hub::archive_workspace` resolves its text argument and calls it with the caller chat's window as fallback; its returned message is unchanged.
- In `phone.rs`: refuse an unknown workspace with 400 `"No such workspace"`, a teammate's copy (`workspace_source == TEAM_COPY_SOURCE`) with 400 `"Archive a teammate's copy on the Mac"`, no Mnemonic window with 409 `"Open Mnemonic on the Mac first"`; otherwise call `archive_workspace_by_id` with the front window as fallback, under the 60 s long-action timeout (on timeout return the existing "still running on the Mac" style message). Errors from `check_can_archive` come back as 400 with the text verbatim. Success returns the default outcome (`{}`), and a Mac notice "archived <name>".

- [ ] **Step 1: Write failing tests** in `phone_server.rs` (raw HTTP style used by existing tests): `archive_route_parses` (POST `/v1/workspaces/ws-1/archive` reaches the handler as `PhoneAction::Archive { workspace_id: "ws-1" }`), `archive_route_refuses_bad_id` (`/v1/workspaces/a%20b/archive` gives 404). In `phone.rs` tests, if a pure helper decides the refusal (for example `fn archive_refusal(source: Option<&str>) -> Option<TeamActionError>`), test unknown and team-copy cases.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core archive_route` and see it fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p mnemonic-core phone_server`, `cargo test -p mnemonic-panels phone`, `cargo test -p mnemonic-panels agent_hub`, `cargo check -p mnemonic-panels`. All pass.
- [ ] **Step 5: Commit** `Let the phone archive a workspace`.

### Task 3: Pending message bubbles in the app

**Files:**
- Create: `android/app/src/main/java/dev/mnemonic/phone/chat/PendingMessages.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatViewModel.kt`, `chat/ChatScreen.kt`
- Test: `android/app/src/test/java/dev/mnemonic/phone/chat/PendingMessagesTest.kt`

**Interfaces:**
- Produces: `data class PendingMessage(val id: Long, val text: String, val error: String? = null)` and a pure `class PendingMessages` (or functions) with `add(text): PendingMessage`, `fail(id, error)`, `retry(id)`, and `reconcile(userMessagesAfterSend: List<String>): List<PendingMessage>` that drops each pending message once a user message with the same text appears in the transcript after it was added (matching in order, each transcript message clearing at most one pending message).
- Changes: the composer and reply chips stay enabled while the agent is generating (only disabled while a send is in flight for the same text if needed). Sending adds a pending bubble immediately; pending bubbles render after the transcript as user bubbles at 60% opacity; a failed one shows the error text verbatim in `MnColors.error` under the bubble and a small "Retry" text button.

- [ ] **Step 1: Write failing tests** `PendingMessagesTest`: `addShowsImmediately`; `reconcileDropsWhenTranscriptHasIt`; `twoSendsKeepOrder` (add "a", add "b", transcript gains "a" then drops only "a", then "b"); `sameTextTwiceNeedsTwoTranscriptMessages`; `failKeepsWithError`; `retryClearsError`.
- [ ] **Step 2: Run** `script/android test`; they fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `script/android test`, then the emulator check: make the mock hold a chat as generating and delay echoing a posted message by 3 s; send while working, screenshot the dimmed bubble (`pending.png`) and after the echo (`pending-cleared.png`); make the mock answer 500 once and screenshot the error with Retry (`pending-failed.png`).
- [ ] **Step 5: Commit** `Show phone messages at once while they are being sent`.

### Task 4: Archive from the app

**Files:**
- Modify: `android/app/src/main/java/dev/mnemonic/phone/api/MacClient.kt` (`archiveWorkspace`)
- Modify: `android/app/src/main/java/dev/mnemonic/phone/home/HomeScreen.kt`, `home/HomeViewModel.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatScreen.kt`, `chat/ChatViewModel.kt`
- Test: `android/app/src/test/java/dev/mnemonic/phone/api/MacClientTest.kt` (extend)

**Interfaces:**
- Consumes: Task 2's route.
- Produces: `suspend fun archiveWorkspace(workspaceId: String): MacResult<Unit>` posting `{}` to `/v1/workspaces/<id>/archive` with the usual headers and the 75 s POST timeout.

Behaviour (spec, Android app):
- Home workspace rows swipe left to reveal a muted grey "Archive" action (`SwipeToDismissBox`, end-to-start only). Releasing on it opens the confirm dialog and resets the swipe.
- The chat `⋯` menu gains "Archive workspace" (Archive icon) at the bottom.
- The confirm dialog: title "Archive <workspace name>?", text "You can restore it on the Mac.", buttons Archive (accent) and Cancel, styled like the existing merge confirm.
- On success: Home removes the row at once and refreshes; from the chat, the app goes back to Home. On failure: the Mac's text verbatim in the existing error bar.

- [ ] **Step 1: Write failing test** in `MacClientTest`: `archivePostsToArchiveRoute` (MockWebServer: path `/v1/workspaces/ws-1/archive`, method POST, body `{}`, `X-Mnemonic-Phone: app`, and a 400 body `"The workspace has unpushed commits"` becomes `Refused(400, "The workspace has unpushed commits")`).
- [ ] **Step 2: Run** `script/android test`; it fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `script/android test`, then the emulator check (add the archive route to the mock: 200 for one workspace, 400 with a reason for another): swipe and confirm (`archive-swipe.png`, `archive-confirm.png`), the row gone (`archive-done.png`), the refused case with the reason (`archive-refused.png`), and archive from the chat menu returning Home.
- [ ] **Step 5: Commit** `Archive workspaces from the Android app`.
