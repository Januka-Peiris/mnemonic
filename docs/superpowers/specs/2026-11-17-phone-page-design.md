# Phone page design

## Intent

Jay wants to keep Mnemonic's agents moving while away from the Mac: see what each workspace's agent is doing, unblock it when it asks for a permission, stop it, reply to it, and start a fresh chat, all from an Android phone. The Mac stays the only place the agents run. The phone reaches it over Tailscale, which is already installed on both, and ntfy (already in use) tells the phone when an agent needs attention.

Prior art: Delta (delta.dev) has web and mobile clients onto its hosted threads. This is the self-hosted, single-user version of that idea for Mnemonic.

## Goals

- A web page served by the Mac, opened in Chrome on the phone and saved to the home screen.
- From the page: list every active (non-archived) workspace with its agent state, PR state and diff counts; read any chat live; answer a permission request with any of the options the agent offers; stop a running agent; send a message to a chat; start a new chat in a workspace.
- ntfy notifications when an agent asks for a permission, finishes a turn, or errors. Tapping one opens that chat on the page.

## Non-goals

- A native Android app.
- Creating new workspaces from the phone (needs picking a project and branch; stays on the Mac).
- Running agents with the Mac off or asleep. If the Mac sleeps the page is unreachable, and that is acceptable.
- Editing files or browsing the file tree from the phone. Viewing the diff is in.
- Access for anyone other than Jay. Teammates keep using the team server.

## Anti-goals

- Any change to what teammates can see or do. The team server (`crates/mnemonic-core/src/services/team_share_server.rs`) keeps its current routes, allow list, workspace set and its refusal of browser requests.
- A page any other web page (or another site open in the phone's browser) can drive.
- Leaking workspace or chat content into ntfy beyond a short line.

## Constraints

- Mac only, Tailscale only. The phone server binds to the Mac's Tailscale address, as the team server does (`tailscale_self()`), never to `0.0.0.0`.
- No third-party CDN or build step for the page: one HTML file with inline CSS and JS, embedded in the binary (`include_str!`), so it works with no internet beyond the tailnet.
- Plain HTTP is fine: Tailscale (WireGuard) already encrypts the traffic. Chrome's home-screen shortcut works over HTTP; web push is not used.
- Follow the repo's Rust rules (`CLAUDE.md`): no `unwrap`, no silently dropped errors, no `mod.rs`.

## Approaches

1. **Extend the team server.** One server, one port. The server would have to answer differently depending on the caller (all workspaces for Jay's own devices, shared ones for teammates) and accept browser requests for Jay but not teammates. Rejected: it puts a per-caller rule into the part teammates reach, where a mistake leaks private workspaces.
2. **A separate phone server on its own port (chosen).** Reuses the team server's building blocks (Tailscale whois identity, Host checks, the `TeamShareHub` long-poll model, the request-thread to main-thread action plumbing) but with its own hub holding every workspace, its own allow rule (Jay's own login only) and its own routes, including serving the page. The team server is untouched. Costs some extra code; the security rule stays simple.
3. **ntfy-only.** Notifications with Approve and Stop buttons that POST to the Mac, no page. Rejected: no way to read or reply to a chat.

Jay picked approach 2.

## Design

### Settings

A new `phone` section in the settings file (`crates/settings_content/src/settings_content.rs`, beside `team`):

- `enabled` (bool, default false): run the phone server.
- `port` (u16, default `DEFAULT_TEAM_PORT + 1`).
- `ntfy_url` (string, default empty): the full ntfy topic URL, for example `https://ntfy.sh/<topic>` or a self-hosted one. Empty means no notifications.
- `ntfy_token` (string, optional): sent as `Authorization: Bearer <token>` when set.
- `notify_after_idle_minutes` (u32, default 5): how long the Mac must be untouched before ntfy alerts are sent.

Shown in Settings next to the Team section, together with the page's address (`http://<Mac's MagicDNS name>:<port>/`) once the server is running, so Jay can type it into the phone.

### Who may connect

The phone server allows only the Mac's own Tailscale login (`tailscale_self().login`), checked per request through `TailscaleWhois` exactly as the team server does. The team `allow` list is not consulted.

It also refuses any request whose source address is the Mac itself (its own Tailscale address, or loopback). Agents run shell commands on the Mac, including teammates' agents through the team server's `bash` tool, and a `curl` from there would otherwise pass as Jay and could approve its own permission prompts. (Builder-check call.)

### Browser requests

The page is served by the phone server itself, so its requests are same-origin. The server:

- Requires the `Host` header to be the Mac's Tailscale IP or MagicDNS name (reuse `is_allowed_host`).
- For every POST, requires an `Origin` header equal to `http://<Host>` (same origin), a JSON `Content-Type`, and a custom header (for example `X-Mnemonic-Phone: 1`). A cross-site page can't send the custom header without a CORS preflight, and the server never answers preflights.
- GETs that carry an `Origin` from another host are refused.

### Routes

- `GET /`: the page.
- `GET /v1/workspaces`: every non-archived workspace, the same shape as `TeamWorkspaceList`.
- `GET /v1/chats/<id>?cursor=&wait=`: the long-poll chat update, the same `TeamChatUpdate` shape, with all permission options (not filtered to once-only as for teammates).
- `GET /v1/workspaces/<id>/diff`: the workspace diff, as the team server does.
- `POST /v1/chats/<id>/stop`, `POST /v1/chats/<id>/permission`: as on the team server, with any offered option allowed.
- `POST /v1/chats/<id>/message` `{ "text": "..." }`: queue a message on that chat.
- `POST /v1/workspaces/<id>/chats` `{ "agent": "claude" | ... }`: start a new chat in the workspace and return its id.

### Publishing every workspace

The team side publishes only shared workspaces into its hub every 2 seconds (`TeamShare::start_publishing` / `publish` in `crates/mnemonic-panels/src/team.rs`) and follows open threads' events (`watch_chats`, `publish_thread_event`). The phone side needs the same into its own `TeamShareHub`, for every non-archived workspace, with permissions always published. The builder should factor that publish-and-watch logic so both a team hub and a phone hub can be fed from it, rather than copying it. The phone side runs whenever `phone.enabled` is on, whether or not anything is shared with teammates.

### Acting on chats

Actions run on the main thread through the same channel-and-wait pattern as team actions (`PendingAction`, `wait_for_reply`):

- **Stop and permission:** as `run_action` in `team.rs`, without the step-in check and without the once-only filter. Prompts that need more than picking an option (action choices such as Save or Discard, and "always" rules that need a pattern chosen) show their simple options where they have them, and otherwise "Answer this on the Mac"; the `TeamChatUpdate` wire shape is kept. (Builder-check call.) Mac notices name the phone ("You, from your phone, allowed ...") so it is clear on the Mac what happened.
- **Message:** pushes the text onto that chat's `pending_messages` and calls `flush_pending` (`crates/mnemonic-panels/src/workspace_view.rs`), so a message sent mid-turn is queued and sent when the agent is idle, exactly like typing it on the Mac. It is addressed to the chat by its record id, not routed through `pick_dispatch_target`.
- **New chat:** calls the workspace view's `add_chat` with the chosen agent in the window showing that workspace.

A chat or workspace not loaded in any Mac window returns 409 "Open this workspace on the Mac first", and the page shows that message. Saved transcripts of closed chats are readable, as on the team side.

### Notifications

From the thread events the phone side already follows, a POST to `ntfy_url` when:

- `ToolAuthorizationRequested`: title `<workspace name>`, body a generic line from the tool's kind, such as `needs permission to run a command` or `needs permission to edit a file`. Never the tool title, which for shell tools is the command line and can hold secrets. (Builder-check call.)
- `Stopped` at the end of a turn: `<workspace name>`, `finished: <chat title>`.
- `Error`: `<workspace name>`, `hit an error in <chat title>`.

All three are sent only while the Mac has had no keyboard or mouse input for `phone.notify_after_idle_minutes` (default 5; read from `HIDIdleTime` in `ioreg -c IOHIDSystem`), so the phone stays quiet while Jay is at the desk and starts buzzing once he walks away, even with the window still open. Turns Jay cancelled never notify. (Jay chose the idle rule over always-on or page-open-recently.)

Each carries a `Click` header pointing at `http://<MagicDNS name>:<port>/#chat=<chat id>`. No message text, tool input or diff content goes to ntfy. Repeats for the same chat and kind are collapsed within a short window (for example 30 seconds). Sending runs on the background executor; a failure is logged, never shown as an error dialog.

### The page

One file, mobile first, light and dark following the phone's setting.

```
Workspaces                      [refresh]
-----------------------------------------
* fix-billing-retry   working   PR #212 ok
  +120 -31  2 chats
! onboarding-copy     needs you
  idle 4m   1 chat
```

```
< fix-billing-retry / Retry backoff
-----------------------------------------
you: make the retry exponential
agent: I'll update RetryPolicy ...
  [tool] Edit retry.rs        done
  [tool] Run cargo test       waiting
  +-------------------------------+
  | Run cargo test?               |
  | [Allow once] [Always] [Deny]  |
  +-------------------------------+
-----------------------------------------
[ message...                 ] [Send]
[Stop]   [Diff]   [New chat]
```

- The workspace list polls `/v1/workspaces` every few seconds; workspaces needing attention sort first.
- The chat view long-polls `/v1/chats/<id>` with the cursor, renders user and agent messages, tool calls as one line with status, the plan, and permission prompts as buttons.
- Agent text gets light markdown (paragraphs, code blocks, inline code, bold, links), done in the page's own JS.
- The `#chat=<id>` fragment opens that chat directly, which is what ntfy links use.

### Tests

Only for behaviour that could break silently:

- The phone server refuses a caller whose login isn't the Mac's own, and refuses a POST with a foreign or missing `Origin` or without the custom header.
- A message POST to a busy chat is queued and sent once the agent is idle.
- The ntfy request for a permission prompt carries no message or tool-input content and the right `Click` URL.
- No ntfy request is made while the Mac is not idle, or for a cancelled turn.
- A request from the Mac's own address is refused even with the right login and headers.

## Left to the builder

- How exactly to factor the publishing logic out of `TeamShare` for reuse.
- Whether the phone server shares code with `team_share_server.rs` via extracted helpers or a parameterised server.
- File placement: suggested `crates/mnemonic-core/src/services/phone_server.rs` for the HTTP side and `crates/mnemonic-panels/src/phone.rs` for the app side, with the page at `crates/mnemonic-core/src/services/phone_page.html`.
- The page's visual details within the sketch above.
