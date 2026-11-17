# Phone page v2 design

## Intent

The first phone page (PR #33, spec `2026-11-17-phone-page-design.md`) lets Jay follow and unblock agents from his Android phone. Using it, he found it too thin: he wants to actually work from the phone, not just keep agents moving. That means starting new work, driving it, and shipping it, the way he works on the Mac today. Reading diffs on the phone is not what he wants it for.

Prior art looked at: Cursor's iOS app (agent list, Changes card, PR review sheet, lock-screen activities), Conductor's iPhone app (workspaces per branch, PR status and merge from the phone), Paseo and Happy (QR pairing, voice input, push alerts with actions), Claude Code Remote Control. The common thread is a phone app that is a control surface, not an editor, with alerts you can act on.

A native Android app may come later. This round stays a web page served by the Mac, made installable.

## Goals

- **Home:** a "Needs you" section first (permission prompts waiting, with their buttons inline; chats that finished or errored since Jay last opened them), then every active workspace, each showing its repo name as well as its workspace name. Fix the current overflow where the right side of a row (status, PR number, refresh button) is cut off at phone width.
- **Start:** a New task screen: pick a repo, type or dictate the task, pick the agent. The Mac creates the workspace exactly as its own New workspace does and sends the task as the chat's first message. The phone then opens that chat.
- **Drive:** a message box pinned to the bottom of the chat with a voice button and one-tap replies; Stop and New chat always in reach; tool calls folded so the agent's words stand out.
- **Ship:** Create PR and Merge from the phone, with the PR's state shown on the workspace.
- **Alerts without ntfy:** web push to the installed page, with Allow and Deny buttons on permission alerts.

## Non-goals

- Reading or commenting on diffs beyond the existing diff view, which stays as it is.
- Choosing the base branch, branch name or workspace name when starting a task. Jay confirmed the Mac's defaults are what he wants.
- A native Android app, iOS support, or reaching the page from outside the tailnet.
- Archiving or deleting workspaces from the phone.

## Anti-goals

- Anything that widens who can drive the Mac. The caller rules from v1 stay: Jay's own Tailscale login only, never the Mac's own addresses, same-origin POSTs with the `X-Mnemonic-Phone` header.
- Putting the page on the public internet (see Approaches).
- Secrets in alerts. Alert text stays the generic lines v1 uses (for example "needs permission to run a command"), never a command line, message text or diff.

## Constraints

- Everything from v1's Constraints still holds: Mac only, bound to the Mac's Tailscale address, one page embedded in the binary with no CDN or build step, the repo's Rust rules.
- Chrome on Android only allows a service worker, web push and speech recognition on a secure origin, so this round needs HTTPS.
- Android shows at most two buttons on a notification.

## Approaches

**How the phone reaches the Mac.**
1. Tailscale, as in v1 (chosen). HTTPS comes from Tailscale's own certificate for the Mac's MagicDNS name.
2. A Cloudflare Tunnel with Cloudflare Access. Gives a public HTTPS address without Tailscale on the phone, but puts a page that can run commands on the Mac on the open internet, guarded only by a login layer that Mnemonic would have to verify on every request. Rejected; Jay has no problem keeping Tailscale on the phone.

**How alerts reach the phone without ntfy.**
1. Web push (chosen). The installed page subscribes through Chrome; the Mac sends encrypted pushes through Google's push service, which can't read them. Allow and Deny live on the notification. The Mac-side sending carries over to a native app later.
2. ntfy with action buttons. Less work, but the secret that authorises a button would pass through the ntfy server, and Jay keeps a second app.
3. Leave alerts on ntfy until a native app. Rejected; alerts you can act on are the biggest gain.

Jay chose Tailscale plus web push. ntfy support from v1 stays and works alongside.

## Design

### HTTPS

- Jay turns on HTTPS certificates once in the Tailscale admin console. Accepted cost: the Mac's MagicDNS name then appears in public certificate transparency logs.
- When the phone server starts it runs `tailscale cert` for `TailscaleSelf.dns_name`, writing the certificate and key into Mnemonic's data directory (key file readable by Jay only). It renews the certificate when fewer than 30 days remain, checked at start and once a day, and reloads the server without dropping the port.
- With a certificate, the phone server serves HTTPS only, on the same port. The page address shown in the Team window becomes `https://<MagicDNS name>:<port>/`.
- If the certificate can't be obtained (HTTPS not enabled in the tailnet, `tailscale` missing or too old), the server falls back to plain HTTP as in v1 and the Team window's "This computer" card says alerts are off and why, with the exact reason from `tailscale cert`. Everything except web push and voice input keeps working over HTTP.
- `is_allowed_host` and the same-origin check accept the `https://` origin.

### Installable page

- Add a web app manifest (name "Mnemonic", standalone display, theme colours matching the page, one icon) and a service worker, both served by the phone server from embedded files.
- The service worker exists for push and notification clicks only. It does not cache the page; the page needs the Mac anyway.

### Home

```
Needs you
-----------------------------------------
! mnemonic / fix-billing-retry
  needs permission to run a command
  [Allow once]  [Deny]  [Open]
✓ sidekick-api / onboarding-copy
  finished: Copy tweaks              [Open]

Workspaces                    [+ New task]
-----------------------------------------
mnemonic
  fix-billing-retry   working   PR #212
  phone-page-v2       idle 4m
sidekick-api
  onboarding-copy     idle      PR #88 ready
```

- "Needs you" lists, across all workspaces: every chat with a pending permission prompt, and every chat whose last turn ended (finished or errored) after Jay last opened that chat on the phone. Opening the chat clears it. The "last opened" marks live in the page's local storage; no server state.
- On a device that has never run the page, the baseline is the moment it first runs: only turns that end after that show. (Builder-check call.)
- To support this, each chat on the wire gains when its last turn ended and whether it errored. The Mac keeps these in memory only, so turns that ended before the Mac app restarted don't show. (Builder-check call.)
- Inline buttons on a permission card are the same once-only Allow and Deny the notification uses (below). Prompts without those options show only Open.
- Workspaces are grouped by repo (`TeamWorkspace.project`, already on the wire), repos in alphabetical order, workspaces within a repo with the ones needing attention first, then by most recent activity.
- Rows wrap or truncate the workspace name so status and PR never fall off the right edge at 360 px wide.

### Start a task

- `+ New task` opens a screen with: a repo picker (every repository added in Mnemonic, most recently used first), a large text box with a voice button, an agent picker (agents enabled on the Mac, default selected), and Start.
- Start POSTs `/v1/tasks` `{ "repository_id", "agent", "text" }`. On the Mac this does what `WorkspacePanel::add_worktree` does: generated workspace name, `<branch_prefix>/<name>` branch, the repo's base branch or the default, a primary chat with the chosen agent. It then sends the text as an ordinary user message to that chat, so the workspace and branch get renamed from the chat title exactly as when Jay types the first message on the Mac. The builder should factor the creation part of `add_worktree` so both callers use it rather than copying it.
- The response carries the new workspace id and chat id; the page opens that chat.
- The new workspace goes into the frontmost Mnemonic window (see "Workspaces not open on the Mac"). If no Mnemonic window is open, the POST returns 409 with "Open Mnemonic on the Mac first".

### Workspaces not open on the Mac

v1 refused to act on a workspace not loaded in a Mac window ("Open this workspace on the Mac first"). v2 drops that rule, because after a Mac restart most workspaces aren't loaded and the phone would be mostly unusable. (Jay's decision.)

- Any phone action that needs a loaded workspace (message, Stop, New chat, Create PR, answering a permission on a chat that is only saved) first opens that workspace in the frontmost Mnemonic window, the same way selecting it in the Mac's workspace list does, then carries out the action. The Mac window switching to that workspace is accepted; nobody is at the Mac.
- Only when no Mnemonic window is open at all does the phone get 409 "Open Mnemonic on the Mac first".
- Merge doesn't need a loaded workspace and never opens one.

### Driving a chat

```
< mnemonic / fix-billing-retry       [Stop]
-----------------------------------------
you: make the retry exponential
agent: I'll update RetryPolicy ...
  ▸ 6 tool calls
agent: Done. Backoff doubles up to 30s,
  tests pass.
-----------------------------------------
[Yes, go ahead] [Continue] [Open a PR]
[ message...              ] [mic] [Send]
[New chat]  [Diff]  [PR #212: Merge]
```

- Consecutive tool calls collapse into one line ("▸ 6 tool calls", or the single tool's title when there is one); tapping expands them as v1 shows them. A tool call waiting on permission is never folded.
- The view opens scrolled to the newest message.
- One-tap replies send their text as an ordinary message: "Yes, go ahead", "Continue", "Open a PR". Fixed list.
- The mic button uses the browser's speech recognition and fills the message box; Jay reviews and sends. It is hidden when the browser doesn't offer speech recognition (for example over the HTTP fallback).
- The composer and the action bar stay pinned to the bottom, above the keyboard.

### Shipping

- Each workspace carries its PR status as the Mac's detail panel already fetches it (state, draft, merge state, number, URL, head commit). Add what `can_merge` needs to the phone's workspace data.
- **Create PR** (shown when the workspace has no open PR): does what the Mac's Create PR does, `create_pr_per_settings`, which sends the PR instruction to the workspace's agent and arms the CI auto-fix when `keep_ci_green` is on. It's a visible message in the chat, so Jay watches it happen.
- **Merge** (shown when `can_merge` holds): the page asks for confirmation showing the title, merge method and short head commit, as the Mac's prompt does. The POST carries the head commit the page saw; the Mac calls `merge_workspace_pr` with it and the configured merge method, so GitHub refuses if something was pushed since. Errors from `gh` come back to the page verbatim.

### Web push

- On first run the Mac generates a VAPID key pair (P-256) and stores it in Mnemonic's data directory, private key readable by Jay only. `GET /v1/push/key` returns the public key.
- The installed page asks for notification permission from an explicit "Turn on alerts" button (Chrome requires a user gesture), subscribes, and POSTs the subscription to `/v1/push/subscriptions`. The Mac keeps every subscription it is given (Jay may install on more than one device), stored in its database, and deletes one when the push service answers 404 or 410.
- Pushes go out for the same events and under the same idle rule as v1's ntfy alerts (`notify_after_idle_minutes`, cancelled turns skipped, repeats collapsed within 30 s). ntfy and web push are independent; either, both or neither may be configured.
- Messages are encrypted per RFC 8291 (`aes128gcm`) and signed per RFC 8292 (VAPID), sent with `reqwest`. The payload holds the workspace and repo names, the generic line, the chat id, and for a permission prompt the request id and the ids of its once-only Allow and Deny options when it has both.
- Notification taps: the body opens the page at `#chat=<chat id>`. On a permission alert the two buttons are "Allow once" and "Deny"; the service worker POSTs the choice to `/v1/chats/<id>/permission` (same origin, with the phone header) and replaces the notification with "Allowed" or "Denied", or "Already answered" if the Mac answers 409 because the prompt has gone. Prompts without both simple options get no buttons.

### New routes

All under the v1 caller rules.

- `GET /v1/repositories`: id and name of every repository in Mnemonic, most recently used first.
- `POST /v1/tasks`: start a task (above).
- `POST /v1/workspaces/<id>/pr`: Create PR.
- `POST /v1/workspaces/<id>/merge` `{ "head": "<commit>" }`: merge.
- `GET /v1/push/key`, `POST /v1/push/subscriptions`.
- `GET /manifest.webmanifest`, `GET /sw.js`, `GET /icon.png`.

Mac notices name the phone for the new actions as v1 does ("You, from your phone, merged PR #212").

### Tests

Only for behaviour that could break silently:

- Web push encryption reproduces the RFC 8291 worked example, and the VAPID token carries the push endpoint's origin as `aud` and expires within 24 hours.
- A 410 from the push service removes that subscription.
- A push is not sent while the Mac is not idle (extend v1's idle test to cover the push path).
- `POST /v1/tasks` with an unknown repository id is refused without creating anything; with a known one it creates the workspace and the first message reaches its chat.
- The merge route passes the page's head commit through to `merge_workspace_pr` and refuses when `can_merge` doesn't hold.
- When `tailscale cert` fails the server still starts over HTTP and the failure reason reaches the Team window.
- The new POST routes refuse a foreign origin and a missing phone header (extend v1's tests to one of them).

## Left to the builder

- How to factor workspace creation out of `add_worktree` for the phone to share.
- Which crypto crates to use for VAPID and RFC 8291 (the workspace already has `jsonwebtoken`, `rustls`, `reqwest`), and how TLS is plugged into the tiny_http server.
- Where in the database push subscriptions live (a new table or the settings store).
- The icon, and the page's visual details within the sketches above.
