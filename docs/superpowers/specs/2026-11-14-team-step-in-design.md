# Team sharing step 2: step in on a teammate's agent

## Intent

Jay wants a collaborative workspace with teammates over Tailscale, peer to
peer, with no login or SSH access to the other Mac. Step 1 (PR #27) made a
teammate's shared workspace a read-only mirror. Step 2 lets a teammate act on
the agent running there: stop it, message it, and answer its permission
requests. The point is that a teammate's agent does not sit blocked or run off
in the wrong direction while its owner is away from the desk.

## Goals

- From a mirrored chat, stop the teammate's agent.
- From a mirrored chat, send a message to the teammate's agent. It arrives in
  their chat labelled with who sent it and queues if their agent is busy.
- From a mirrored chat, answer a permission request their agent is waiting on.
- Nothing above works unless the sharer switched it on for that workspace.
- The sharer can see that a teammate acted.

## Non-goals

- Messaging a chat that is not loaded on the sharer's machine.
- Images, attachments or mentions in a teammate's message. Text only.
- Creating, closing, forking or renaming the teammate's chats.
- Changing their model, mode or other options.
- Their terminals, Setup and Run.
- Running your own agent on their work (step 3).
- Compatibility with protocol v2. Both Macs update.

## Anti-goals

- No action is accepted without a Tailscale identity on the allow list.
  That identity belongs to a machine, not a person at a keyboard: any
  process on an allowed person's machine acts as them, and the sharer's own
  login is always allowed. A web page open in a browser there is such a
  process, so the server also refuses what a browser can send (see Request
  checks).
- No action is accepted for a workspace whose step-in switch is off, even
  from an allowed teammate.
- The share server exposes no other way to change the sharer's machine: no
  file writes, no commands, no settings.
- A stepping-in teammate never slows or breaks the sharer's own chat.

## Constraints

- Peer to peer over the existing share server (Tailscale address, port
  47231). No central server.
- The server runs on plain threads (`tiny_http`); chats live on the GPUI
  foreground thread. Actions must cross that boundary and report back.
- The mirror's thread is rebuilt from the sharer's entries by index
  (`ChatMirror`). Anything the viewer adds locally to that thread breaks the
  index accounting, so the viewer never adds entries itself.

## Approaches

**A. Actions over the share server, shown through the real chat view
(picked).** The share server gains three POST routes. The viewer keeps the
real chat view from step 1 and its normal controls drive those routes: the
Stop button, the message editor, and the permission buttons. What happens
shows up through the mirror like everything else. This continues step 1's
approach, so the mirror keeps looking like a local workspace.

**B. A separate "remote control" strip.** Keep the chat view read-only and
add our own Stop / Message / Approve controls above it. Less work inside the
chat view, but it is a second UI to maintain and the mirror stops looking
like a local workspace, which is what Jay asked for.

**C. Speak ACP to the sharer's agent directly.** Proxy the agent connection
so the viewer becomes a second ACP client. Agents do not support two clients
on one session, and it would bypass the sharer's own chat, so they would not
see what happened.

A was picked: it is the only one that keeps "their workspace looks like
mine" and keeps the sharer's chat as the single place things happen.

## Design

### Opt-in

- A per-workspace switch "Teammates can step in", off by default, shown only
  for shared workspaces: in the sidebar workspace menu next to Share, and in
  the Team window's workspace list.
- Stored like the shared set, in the core settings table
  (`team_step_in_workspaces`, JSON list of workspace ids). Unsharing a
  workspace clears it.
- `TeamWorkspace` gains `step_in: bool`. The viewer enables its controls only
  when it is true.
- `TeamChat` gains `live: bool`: the chat is loaded in the sharer's app right
  now. `open` stays "readable", which since step 1 includes chats shown from a
  saved transcript. A chat that is readable but not live keeps the step 1
  read-only notice, so no control is offered that can only fail.

### Protocol v3

`TEAM_PROTOCOL_VERSION = 3`.

`TeamChatUpdate` gains two fields sent with every update, like `plan`:

- `generating: bool`: the sharer's thread is mid-turn.
- `permissions: Vec<TeamPermissionRequest>`: one per tool call waiting for an
  answer, `{ tool_call_id, options: Vec<acp::PermissionOption> }`. The
  sharer's dropdown-style options are flattened to their allow and deny
  options. Only `AllowOnce` and `RejectOnce` options are sent: an "always"
  answer would leave a standing rule in the owner's agent, which is a
  setting, and that is the owner's to make. A request with no once-only
  option is not sent. Only permission requests
  (`AuthorizationKind::PermissionGrant`) are sent: an action choice, such as "Save" or "Discard"
  before the agent edits a file with unsaved changes, is the owner's to
  make. Once-only answers flattened out of a dropdown are named "Allow once"
  and "Deny once", as the dropdown names only its choices. Empty unless the
  workspace allows stepping in.

New routes, all POST with a JSON body, all behind the same Tailscale identity
and allow-list check as the GET routes, plus: the chat must belong to a shared
workspace with step-in on, else 403.

| Route | Body | Effect |
|---|---|---|
| `/v1/chats/{id}/stop` | none | Cancels the running turn |
| `/v1/chats/{id}/message` | `{ "text": "..." }` | Queues the text for the chat |
| `/v1/chats/{id}/permission` | `{ "tool_call_id": "...", "option_id": "..." }` | Answers a waiting permission request |

Responses: 200 with `{ "queued": bool }` for message (true when their agent
was busy), 200 for the others, 403 when not allowed, 404 when the chat is not
loaded, 409 when there is nothing to stop, the permission request is no
longer waiting, or two teammate messages are already queued for the chat,
413 when the body is over 256 KB.

### Request checks

Checked on every request before the caller is identified, so a web page on
an allowed machine cannot act as its owner:

- A request with an `Origin` header is refused with 403. Browsers send it on
  cross-origin requests; the app's client never does.
- A request whose `Host` is missing, or whose host part (port stripped,
  bracketed IPv6 allowed) is not an IP address, a name ending in `.ts.net`,
  or a bare machine name with no dot, is refused with 403. This blocks DNS
  rebinding, which would otherwise let a page read `GET /v1/workspaces`.
- A POST must carry `Content-Type: application/json` (parameters allowed)
  and a non-empty `X-Mnemonic-Team` header (the client sends the protocol
  version), else 400. Either one makes a browser send a CORS preflight
  first, which the server never answers.

### Sharer

- The server hands each accepted action to an action handler and blocks the
  request thread on its answer, up to 10 seconds. In the app the handler
  sends the action to a foreground task owned by `TeamShare`. An action whose
  request gave up waiting is skipped, never carried out later.
- The action carries who asked: the caller's Tailscale display name,
  collapsed to one line without control characters, falling back to the
  login. The owner's notifications show the login next to the display name
  when they differ, as the display name is whatever the teammate set.
- Stop: the same path as the owner's own Stop button (the thread view's
  `cancel_generation`), not a bare `thread.cancel`, and always on the root
  thread, whose turn is the one checked as running, even when the owner is
  looking at a subagent. The owner's button also marks the turn as
  interrupted by the user, which stops
  the chat view from sending the owner's next queued message straight after.
  The workspace's pending-message queue flushes as it does after a local
  stop, so a teammate's queued message still goes out.
- Message: pushed onto the chat's existing pending-message queue in
  `WorkspaceHost`, as `From {name} (teammate):\n\n{text}`, then flushed. The
  queue already sends when the thread goes idle. At most two teammate
  messages wait per chat (counting only those not yet sent); a third is
  refused with 409. A teammate's message is recorded like an app-sent one,
  so it never names the workspace.
- Permission: the request must be a permission request
  (`AuthorizationKind::PermissionGrant`), and the option is looked up by id
  in the tool call's own options (so a teammate can only pick an option the
  agent offered) and must be `AllowOnce` or `RejectOnce`, else 403. Then
  `thread.authorize_tool_call`. The notification says allowed or rejected
  from the chosen option's kind.
- Whether a chat is loaded (the 404) is decided on the foreground thread from
  the live chats, not from the hub, which also keeps feeds for closed chats.
  A closed chat's kept feed is shown as not generating with no permission
  requests.
- Each stop and each permission answer shows a notification on the sharer's
  Mac: "{name} stopped the agent in {chat}", "{name} allowed {tool} in
  {chat}". A message needs none; it is in the chat with its label.

### Viewer

- `AcpThread` gets a remote-generating flag the mirror sets from
  `generating`. `status()` reports generating while it is set, so the chat
  view shows its usual working state and Stop button. `cancel` with the flag
  set and no local turn calls the connection's `cancel`.
- `TeamConnection::cancel` posts stop.
- The chat view's read-only mode gains a variant where the editor stays and
  sending calls a handler instead of `thread.send`. The handler runs before
  the chat view's own "queue locally while generating" branch, or the
  remote-generating flag would trap the message in the viewer's queue. The teammate view uses it
  when `step_in` is true and posts the message. The message appears through
  the mirror once the sharer's chat has it. If the answer says queued, the
  viewer shows "Queued until {name}'s agent finishes".
- For each entry in `permissions` the mirror asks the local thread for
  authorization with those options, so the usual buttons appear. Picking one
  posts the answer. When a request leaves `permissions` without a local
  answer (answered on the sharer's Mac or by another teammate) the mirror
  clears the waiting state without posting. `AcpThread` needs a new method
  for that: a status update alone keeps a tool call waiting.
- A failed action shows an error notification with the server's reason. A
  message that could not be sent goes back into the editor if it is still
  empty.
- When the chat is no longer open on the sharer's Mac, or the sharer cannot
  be reached, the mirror stops showing the agent as working and clears its
  waiting permission requests without posting an answer.
- Escape does not stop a teammate's agent; only the Stop button does.
- With `step_in` false everything stays as in step 1.

## Left to the builder

- How the foreground task and the request thread exchange the action and its
  answer.
- The exact shape of the chat view hook for sending, as long as the viewer
  adds no entries to the mirrored thread itself.
- Where the queued and error notices render, using what the app already has
  for notifications.

## Tests

Each test names the production behaviour that breaks it.

- Server: an allowed teammate's stop/message/permission reaches the action
  handler; the same calls are refused with 403 when the workspace has step-in
  off, and when the caller is not on the allow list.
- Sharer: a teammate's message is queued while the agent is busy and sent
  once it is idle, with the sender's label; a permission answer naming an
  option the agent never offered, or an "always" option, is refused.
- Viewer: a mirrored chat reports generating while the sharer's does;
  permission buttons appear for a waiting request and clear without posting
  when it is answered elsewhere.
