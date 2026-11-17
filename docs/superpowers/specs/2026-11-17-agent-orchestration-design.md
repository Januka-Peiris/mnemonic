# Agents that start and talk to other agents

## Intent

Jay already has this working in Javen (`/Users/jay/dev/groundhog-loop`, the
`javen-hub` MCP tools `javen_thread_start`, `javen_thread_send`,
`javen_thread_wait`, `javen_threads`): an agent splits big work by opening
other agents' threads, each optionally in its own branch and worktree,
briefs them, waits for their replies and sends follow-ups. He wants the same
in Mnemonic, for the agents running in Mnemonic chats.

Mnemonic's `coordinator_service` was an earlier attempt at agents directing
agents. It never worked and is removed as part of this work ("javen was much
better").

## Goals

- An agent in any Mnemonic chat (Claude or Codex) can:
  - start a new chat with Claude or Codex and give it a brief, either in a new
    workspace (any repository Mnemonic knows, a new branch or an existing
    one) or in an existing workspace;
  - send that chat follow-up messages;
  - wait for one or more chats to finish their turn and read their replies;
  - list the chats it started and where each stands;
  - stop a chat it started;
  - archive a workspace an agent made.
- Chats and workspaces started this way are ordinary Mnemonic chats and
  workspaces: they show in the sidebar, Jay can open them, watch them, type
  into them, answer their approval prompts and stop them, exactly as for a
  chat he started himself.
- When Jay types into a chat that is blocked waiting on others, the wait
  returns at once with his message, so the agent answers him in seconds
  rather than when the wait ends (Javen's fix in commit 2dc5796).
- `coordinator_service` and everything that exists only for it is gone.

## Non-goals

- Agents outside Mnemonic (Claude Code or Codex in a terminal) using these
  tools. Mnemonic chats only (Jay's pick).
- Javen's builder/critic pairing (`javen_hub_*`), lab findings, queue, runs
  and workstream cards.
- Conductor's sidebar sections and editing a repository's setup, run and
  archive scripts (Jay: not needed).
- Workspace statuses set by agents.
- Teammates' chats (the team sharing feature) using these tools.
- Agents other than Claude and Codex. Gemini, OpenCode and Cursor are not
  tested; they get the tools only if they accept MCP servers over HTTP and
  nothing is done to make them work otherwise.

## Anti-goals

- Starting a chat or workspace pulling Jay's view away from what he is
  looking at. New workspaces appear in the sidebar; nothing switches focus.
- An agent losing work: archiving a workspace whose worktree has
  uncommitted changes is refused.
- An agent stopping or archiving Jay's own chats and workspaces. Stop and
  archive act only on chats and workspaces started through these tools.
- The tools being reachable by anything but Mnemonic's own chats on Jay's
  Mac.
- One runaway agent opening dozens of chats.

## Constraints

- Mnemonic runs agents through ACP adapters: `claude-acp` (v0.107.2) and
  `codex-acp` (v2.7.0, `@agentclientprotocol/codex-acp`). Both accept MCP
  servers over HTTP in `session/new`: Claude was proven by the team phase 1
  spike and build; `codex-acp` reports `"mcpCapabilities":{"http":true}`
  (checked 2026-11-17 with Codex CLI 0.169.0).
- The MCP server name `workspace` is reserved by Claude and silently not
  loaded. Pick another.
- Claude defers MCP tools behind ToolSearch, costing one step.
- Chats live in the UI: `WorkspaceView` (`crates/mnemonic-panels/src/workspace_view.rs`)
  builds each chat's `ConversationView` (`build_chat`), queues messages for
  it (`pending_messages`, `flush_pending`), stops it (`stop_chat`), and
  `WorkspaceView::add_session_for_workspace` opens a chat in any workspace,
  creating the workspace's view if needed. `workspace_service::create_workspace`
  (`CreateWorkspaceInput`: repo, base branch, branch, agent, task prompt)
  makes the worktree and runs Setup. `archive_workspace` removes a managed
  worktree.
- Phase 1 of team sharing already has an MCP-over-HTTP handler with SSE
  keep-alives for long calls (`crates/mnemonic-core/src/services/team_agent_tools.rs`,
  `team_share_server.rs`) and per-session additions to ACP session setup
  (`SessionExtras` in `crates/agent_servers/src/acp.rs`).
- A wait can last up to an hour; the MCP HTTP call must survive that
  (phase 1 solved this for long Bash calls with SSE keep-alives).

## Approaches

- **A. Mnemonic serves the tools itself over local HTTP (picked).** A small
  MCP endpoint on 127.0.0.1, added to every Mnemonic chat's session with a
  per-chat secret, so a call says which chat is asking. Calls are carried
  out by Mnemonic's existing workspace and chat code on the UI thread. One
  process, reuses phase 1's MCP handling. Needs agents that take MCP over
  HTTP, which Claude and Codex both do.
- **B. A helper process per chat (stdio MCP), as Javen does.** Works with
  any agent, but adds a second process per chat plus a way for it to reach
  Mnemonic. Only worth it if an agent lacked HTTP MCP. Dropped.
- **C. Run started chats headless in the background and attach the UI when
  opened.** Duplicates all of the chat machinery that lives in the UI.
  Dropped.

Jay picked A.

## Decided

- The tools (Jay agreed the set; names and inputs are Claude's call,
  modelled on Javen's so agents use them the same way). MCP server name
  `mnemonic`.
  - `start_chat`: `task` (the full brief), `agent` (`claude` or `codex`,
    default `claude`), and where: `workspace` (an existing workspace id) or
    `repo` (a repository name as Mnemonic shows it) with optional `branch`
    (an existing branch to work on, or a new branch name) and `base` (base
    for a new branch, default the repository's default branch); optional
    `title`. Returns at once with the chat id, the workspace id and name.
    Workspace setup can take minutes; the chat counts as busy until its
    first turn has started, so a wait covers it, and a failed setup becomes
    the chat's reply ("could not create the workspace: ...").
  - `send`: `chat`, `text`. Returns at once. If the chat is mid-turn the
    message queues and goes when the turn ends.
  - `wait`: `chats` (one or more ids), `any` (return when one finishes,
    default false), `timeout_seconds` (default 1200, max 3600). Returns for
    each chat: title, agent, status (replying, replied, waiting for an
    approval, failed or stopped), workspace path and branch, and the latest
    reply in full up to 6000 characters. On time out it says which are
    still replying, so the agent calls it again.
  - `list`: the repositories Mnemonic knows (names and default branches);
    every workspace (id, name, repository, branch, path) with its chats
    (id, title, agent, status), marking the calling chat's own workspace
    and chat and the chats it started. This is how an agent finds a
    workspace or chat Jay points it at by name. `start_chat`'s `workspace`
    and the `chat` inputs accept an id or an exact name, refused when a
    name matches more than one.
  - `stop_chat`: `chat`. Stops the turn in flight, as its Stop button does.
  - `archive_workspace`: `workspace`. Refused unless an agent made it
    through `start_chat`, and refused if its worktree has uncommitted
    changes.
  (Claude's call.)
- `send` and `wait` work on any Mnemonic chat id, as Javen's do, so an agent
  can talk to a chat Jay points it at; `stop_chat` and `archive_workspace`
  only on agent-started ones. (Claude's call; see anti-goals.)
- Started chats are ordinary chat tabs in their workspace, created without
  switching Jay's view, titled from `title` or the task. The chat records
  which chat started it, and its tab shows "Started by <that chat's
  title>". (Claude's call.)
- New workspaces made by `start_chat` are ordinary workspaces (Setup runs as
  usual) and remember that an agent made them. (Claude's call.)
- Started chats use Mnemonic's default mode for their agent, as a chat Jay
  opens does. Approval prompts appear in that chat's tab; `wait` reports
  "waiting for an approval" so the starting agent can tell Jay. (Claude's
  call.)
- When Jay types into a chat while it is blocked in `wait`, the wait returns
  at once with his message first and a line telling the agent to answer him
  before waiting again; the message is then not delivered a second time.
  (Javen's behaviour.)
- At most 8 chats started by one chat may be busy at once; more is refused
  with a message saying so. Chats started by an agent can start chats of
  their own one level further, but chats two levels down get no
  `start_chat` (they can still `send`, `wait` and `list`), so one chat's
  family is at most 8 x 8 busy at once and cannot grow deeper. The same brief to the same place from the same
  chat within 2 minutes returns the chat already started instead of a
  second one (an agent retrying a call that timed out). (Claude's call,
  after Javen.)
- The endpoint listens only on 127.0.0.1, on a port chosen at startup, and
  requires the per-chat secret Mnemonic gave that chat's session; anything
  else is refused. (Claude's call.)
- Every Mnemonic chat (new and resumed) gets the server; teammates' chats
  in a shared workspace (team phase 1) do not. (Claude's call.)
- Removing the coordinator: `coordinator_service.rs`,
  `coordinator_repository.rs`, `models/coordinator.rs`,
  `coordinator_panel.rs` and its registration and keybinding in
  `crates/zed/src/zed.rs`, the `coordinator_autostep_watch` job in
  `orchestrator_service.rs`, the startup reconcile in
  `app_runtime_service.rs`, and coordinator-only code in `events.rs`,
  `state.rs`, `agent_profile`, `local_llm_service.rs` and
  `terminal_service.rs`. Its database tables are left in place, unused; no
  migration drops them. (Claude's call.)

## Left to the builder

- How MCP calls reach the UI thread and come back (a channel into a GPUI
  task, or similar), and how `wait` is woken (polling chat state or
  notifications).
- How the per-chat secret is given to each session (extending
  `SessionExtras` or a new session-setup hook) and kept across resume.
- Where the parent link and "made by an agent" flag are stored (new columns
  on `chat_sessions` and `workspaces`, with a migration).
- Creating a workspace's view and chat without activating it, where
  `add_session_for_workspace` currently activates.
- How "Jay typed while waiting" is detected and taken off the chat's queue.
- Whether `start_chat` with a `repo` and an existing `branch` that already
  has a Mnemonic workspace reuses that workspace or refuses.
- Sharing the MCP-over-HTTP code with `team_agent_tools.rs` rather than
  copying it.
