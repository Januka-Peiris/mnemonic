# A teammate's shared workspace that looks like your own

## Why

The end goal is a collaborative workspace: Jay and a teammate (Dan) working in the same Mnemonic workspace, where either can watch, step in on the other's agent, or point their own agent at the work. Today a shared workspace opens as a tab in a separate Team window, with a Chat/Changes switch, a simplified copy of the chat and one long diff. It does not feel like a workspace, and its chat copy cannot grow into something you can act in.

This is the first of three projects, in order:

1. **Read-only mirror (this document).** A teammate's shared workspace opens in the main window and looks like one of your own.
2. **Step in on their agent.** Send it messages, approve or reject its tool calls, stop it. Needs a "can act" permission on the sharer's side, separate from "can see".
3. **Your agent on their work.** Your agent works on their changes. How it gets their files is decided then.

Project 1 is built so that project 2 is mostly sending the viewer's actions back over the same connection, without redrawing the chat a second way.

## Goals

- Clicking a teammate's shared workspace in the sidebar's Team section opens it in the main window, in the same place your own workspaces open, and selects it in the sidebar the way your own do.
- Each of the teammate's chats in that workspace is a tab in the workspace's tab row, drawn with the same chat view as your own chats (`ConversationView`/`ThreadView`): messages, thinking, plans, and tool calls with their diffs and terminal output. Chats update live while their agent works.
- Subagents a chat starts are mirrored too, so their tool calls open the same way they do in your own chats.
- A chat that is not open on the teammate's machine still gets a tab, showing that it appears once they open it (as today).
- The right panel shows the teammate's changed files, built from the diff their Mac already sends, with the same look as your own Changes list. Clicking a file opens that file's diff, read-only.
- The workspace's name, branch and PR state show where your own workspace shows them.
- It is plainly read-only: the chat's message box is not usable and says it is the teammate's workspace. Nothing you do in the mirror changes anything on their Mac.
- When the teammate stops sharing or goes offline, the open mirror says so rather than closing or showing stale content as live.

## Non-goals

- Sending messages, approving tool calls or stopping their agent (project 2).
- Your own agent working on their changes (project 3).
- Their Setup/Run panel, terminals, or browsing or opening their files beyond the diffs.
- The chat's own "files changed" summary under the message box. It is built from local buffers the agent wrote through, which no agent update carries, and its Keep/Reject buttons change files. The right panel's changed-file list covers the workspace instead.
- Compatibility with the current sharing format. The team protocol version goes up; both Macs update to see each other again. The "speaks team protocol N" message already covers the mismatch.

## Anti-goals

- Any login, SSH or remote-shell access to a teammate's Mac. A mirror is a copy of what their Mnemonic chooses to send, nothing more.
- A central server. It stays peer-to-peer over Tailscale, using the existing share server, for as long as that works.
- The mirror writing to the sharer's machine in any way in this project. Every route stays a GET.
- Viewers slowing down the sharer's Mnemonic or its agent.

## Constraints

- Access control stays as it is: the share server listens only on the Tailscale address, and requests are checked with `tailscale whois` against `team.allow`.
- The share server only runs while at least one workspace is shared (unchanged).
- Live updates keep the existing versioned long-poll model, so reconnecting never drops or repeats an entry.
- Code: `crates/mnemonic-panels/src/team.rs` (sharing side and sidebar), `team_view.rs` (today's viewer), `team_settings.rs` (Team window), `crates/mnemonic-core/src/services/team_share_service.rs` and `team_share_server.rs` (wire format, client and server), `workspace_view.rs` (`WorkspaceHost`, `WorkspaceView`), `workspace_detail_panel.rs` (right panel), `crates/agent_servers` (`AgentServer`, `AgentConnection`), `crates/acp_thread` (`AcpThread::handle_session_update`).

## Approaches

- **A. Real chat view fed from the sharer (picked).** The sharer sends each chat as agent updates (ACP session updates), and the viewer plays them into a real `AcpThread` through a teammate connection (an `AgentServer`/`AgentConnection` whose session is fed from the share server rather than an agent process), the same way a resumed chat replays its history. Chats look exactly like your own, and project 2 becomes the connection's prompt, permission and cancel calls forwarded to the sharer. Cost: the wire format changes and both sides must update.
- **B. Keep today's format and restyle the copy.** Quicker and needs no update on the other Mac, but tool calls stay one-line previews without diffs or output, and project 2 would need its own message box and approval buttons, so the chat would be built twice.
- **C. Open the teammate's folder over Tailscale SSH (Zed remote projects).** Gives real files and a terminal but not the agent chat, and needs login access to their Mac, which is an anti-goal.

Jay picked A: it is the only one that grows into project 2 without redrawing the chat.

## Decided

- Approach A, peer-to-peer over the existing share server, team protocol version bumped.
- Teammate workspaces open in the main window from the sidebar. The Team window keeps the Team settings and stops hosting teammate tabs (Claude's call: one place to open a teammate's workspace).
- The changed-file list comes from the existing whole-workspace diff, split per file on the viewer's side.
- The wire format may use the ACP crate's own types (the old format avoided them so an ACP upgrade on one side couldn't break it). To keep a mismatch from failing silently, `/v1/hello` also carries the `agent-client-protocol` crate version, and a viewer treats a different version like a different team protocol: "update both Mnemonics to the same version". Both Macs are expected to run the same build. (Claude's call.)
- Subagents are mirrored, so a chat on the wire is a root session plus the sessions its subagents start, and the teammate connection supports loading a subagent session like `ConversationView::load_subagent_session` does for your own chats. (Claude's call: an empty subagent box would look broken.)
- The chat's "files changed" summary is left out (see Non-goals). (Claude's call.)

## Left to the builder

- The exact wire format for a chat: forwarding the ACP session updates the sharer's thread received, or rebuilding updates from the sharer's `AcpThread` entries. Either is fine as long as the goals above render, a chat that was already open before sharing started is complete, and reconnects neither drop nor repeat entries. Terminal output in tool calls may arrive as text rather than a live terminal.
- How `WorkspaceHost` holds a teammate workspace beside local ones, and how the right panel switches to the teammate's file list.
- How the per-file diff opens (an editor on the file's diff text, or the existing diff view if it fits without a local repository).
- What to remove from `team_view.rs` once nothing uses it.
