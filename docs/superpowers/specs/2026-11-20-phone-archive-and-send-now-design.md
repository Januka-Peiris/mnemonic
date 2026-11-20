# Phone: archive workspaces, and messages that go straight through

## Intent

Jay asked for two things from the phone, after using the redesigned Android app:

- **Archive from the phone.** On the Mac you archive a whole workspace. That runs teardown, stops its agent, moves the worktree to the archive folder and keeps the branch, and it can be restored. Jay wants the same from the phone.
- **Messages that don't disappear.** A message sent from the phone while the agent is working is held on the Mac until the turn ends (`WorkspaceView::queue_agent_message` puts it in `pending_messages`, and `flush_pending` waits for `ThreadStatus::Idle`), so nothing shows. Jay wants it to "push straight through, like Conductor and the other apps", and to keep it simple: no queue, no extra button.

## Goals

- Archive a workspace from the phone, with the same result as the Mac's own Archive.
- A phone message sent while the agent is working stops the current turn and is sent at once. It shows in the chat immediately.

## Non-goals

- Restoring archived workspaces from the phone (that stays on the Mac).
- Archiving a single chat inside a workspace (the Mac has no such thing).
- Changing how messages typed on the Mac behave.

## Approaches

**Sending while the agent works.**
1. Keep the queue and show a "Queued" bubble with a "Send now" button. Rejected: Jay asked for simpler.
2. **Interrupt and send (chosen).** Stop the current turn the way the Stop button does, then send. The agent keeps everything it did so far and reads the message next. The agent connection (ACP) has no way to hand a message to an agent mid-turn without stopping it, so this is the closest to "push it through".

**Archive.** One sensible way: reuse the path the agent tools already use (`agent_hub::archive_workspace`), which:
- checks `check_can_archive`;
- runs teardown in the workspace's view, opening it in the background if needed;
- calls `archive_own_workspace`;
- makes every sidebar forget the workspace.

## Design

### Mac

- **Messages:** `PhoneAction::Message` on a chat whose root thread isn't idle cancels the turn (`AcpThread::cancel`, awaiting its task), then sends the text as an ordinary user message. On an idle chat nothing changes. A chat still connecting (no thread) keeps today's queue.
  - The send goes through the existing path, so the Mac's Team notice ("You, from your phone, ...") and the transcript entry are as today.
  - The web phone page gets the same behaviour, since it uses the same route.
- **Archive:** new `PhoneAction::Archive { workspace_id }` on `POST /v1/workspaces/<id>/archive`, under the existing caller rules.
  - It factors the body of `agent_hub::archive_workspace` into a shared function both callers use, taking a workspace id instead of resolving text. Window choice is the workspace's own window, else the front Mnemonic window; 409 "Open Mnemonic on the Mac first" when there is none.
  - A teammate's copy (`workspace_source == TEAM_COPY_SOURCE`) is refused with 400 "Archive a teammate's copy on the Mac".
  - `check_can_archive` errors come back verbatim (400).
  - It uses the 60 s long-action timeout. Success returns `{}`.

### Android app

- **Archive** is offered from:
  - a workspace row on Home: swipe left reveals a muted grey "Archive" action (not red; archiving is undoable);
  - the chat's `⋯` menu: "Archive workspace".

  Both ask "Archive <name>? You can restore it on the Mac." with Archive and Cancel. On success the row disappears; from the chat, the app returns to Home. Errors show verbatim.
- **Sending while working:** the composer stays enabled while the agent works. On Send, the message appears at once as a user bubble, slightly dimmed, until the Mac's transcript contains it; then it becomes a normal bubble. If the send fails, the dimmed bubble shows the error verbatim and a Retry action.

## Tests

- **Rust:**
  - Archive route: an unknown or team-copy workspace is refused, and a valid one reaches the shared archive function.
  - Message on a busy chat: the turn is cancelled before the message is sent. Use the existing workspace_view test harness (`StubAgentConnection`) if it can drive a generating thread; otherwise test the decision as a pure function.
- **Kotlin:**
  - The pending-bubble logic: shown immediately, cleared when the transcript contains the same text after the send, kept with an error on failure.

## Left to the builder

- Exact swipe gesture implementation (`SwipeToDismissBox` or similar).
- How the pending bubble is matched to the transcript entry (text equality on the newest user message after the send is enough).
