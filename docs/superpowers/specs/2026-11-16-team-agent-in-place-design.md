# Team sharing step 4: a teammate's agent working in your workspace

## Intent

Jay wants as much collaboration as possible with teammates he trusts
completely. Steps 1 to 3 let a teammate (Dan) watch Jay's shared workspace,
stop and answer Jay's agent, and work in a synced copy on his own Mac whose
edits come back as batches. The copy can drift: when both sides change one
file, one whole version wins.

Step 4 lets Dan's own agent work directly in Jay's real workspace folder on
Jay's Mac. The agent runs on Dan's Mac with Dan's Claude login, but every
file read, search, edit and shell command it makes happens in Jay's folder.
There is one copy of the work, so nothing diverges.

## Goals

- From Jay's shared workspace in his sidebar, Dan starts a chat "in Jay's
  workspace". It is Dan's chat: Dan's Claude login, Dan's model and mode,
  stored on Dan's Mac, listed under Jay's mirrored workspace next to Jay's
  read-only chats.
- In that chat the agent's file and shell tools act on Jay's workspace
  folder on Jay's Mac: reading, listing, searching, editing, writing files
  and running shell commands (tests, builds, git) there, with Jay's
  toolchain.
- Approval prompts appear on both Macs. Whoever answers first decides; the
  prompt then closes on the other side and says who answered.
- Jay sees Dan's chat live in his own workspace, read-only, labelled with
  Dan's name, and can stop it from there.
- Commits Dan's agent makes in Jay's repo carry Dan's git name and email as
  author.
- A new per-workspace switch on Jay's side, "Teammates' agents can work
  here", on by default when the workspace is shared (matching the other
  sharing switches), off means the tools refuse.
- Step 3's copy keeps working alongside; Dan chooses per chat.

## Non-goals

- Dan's agent running on Jay's Mac or using Jay's Claude login.
- Codex or other agents. Claude (claude-acp) only for now.
- Long-lived background processes started by Dan's agent (dev servers,
  watchers). Commands run to completion or time out.
- Dan's agent using Jay's MCP servers, hooks, skills or slash commands.
- Merging edits inside a file in the step 3 copy flow (separate work).
- Compatibility with protocol v4. Both Macs update (protocol v5).

## Anti-goals

- Dan's agent quietly working on Dan's own files when it means Jay's. Its
  built-in file and shell tools are off; the only file and shell tools it
  has act on Jay's folder.
- Jay's project hooks or settings running on Dan's Mac.
- Dan's edits silently overwriting text Jay has unsaved in an editor.
- File tools reaching outside Jay's workspace folder, or into `.git`.
- Anything reachable without a Tailscale identity on `team.allow`, or with
  the new switch off, or from a browser (step 2's request checks apply).
- Sharing slowing Jay's Mnemonic or his own agent.

## Constraints

- Peer to peer over the existing share server (Tailscale address, port
  47231), `tiny_http` threads, `tailscale whois` against `team.allow`, step
  2's browser-request refusals on every POST.
- Mnemonic runs Claude through the `claude-acp` adapter (v0.107.2 in
  testing). A spike on 2026-11-16 (`/tmp/spike-remote`, throwaway) proved:
  - `session/new` with `_meta.claudeCode.options.disallowedTools` listing
    Claude's built-ins (Read, Write, Edit, MultiEdit, NotebookEdit, Bash,
    BashOutput, KillShell, Grep, Glob, LS) turns them off. The adapter also
    honours `_meta.disableBuiltInTools: true` (turns off every built-in).
  - MCP servers passed in `session/new` `mcpServers` are loaded, and the
    agent does all its file and shell work through them, correctly.
  - The MCP server name `workspace` is reserved by Claude and silently not
    loaded. Pick another name.
  - Claude defers MCP tools behind ToolSearch, costing one extra step.
  - Approval prompts were not exercised.
- ACP `McpServer::Http` is supported (`agent_servers/src/acp.rs:3969`), so
  Claude on Dan's Mac can reach an MCP endpoint on Jay's share server
  directly, with headers.
- Code: `crates/mnemonic-core/src/services/team_share_server.rs` and
  `team_share_service.rs` (wire, hub, server), `crates/mnemonic-panels/src/team.rs`
  (sharing side, switches), `team_connection.rs` and `team_workspace_view.rs`
  (viewer side), `crates/agent_servers/src/acp.rs` (session setup),
  `workspace_view.rs` (how chats are started).

## Approaches

- **A. Swap Claude's tools for ones that act on Jay's Mac (picked).** Dan's
  Claude runs with its built-in file and shell tools off and a tool set
  served by Jay's share server. One real folder, Dan's login. Cost: the
  tools must be good enough that Claude works as well as with its own, and
  the share server starts accepting writes and commands.
- **B. Mount Jay's folder on Dan's Mac (Taildrive).** Real files, but
  commands run on Dan's Mac with his toolchain, and git and builds over a
  network mount are slow. Dropped.
- **C. Tools swapped for commands only, files via step 3's copy.** Keeps
  the copy's divergence. Dropped.
- Also ruled out: Zed's remote development (starts the agent on the remote
  host, so Jay's login, and has no TCP transport or remote process
  spawning) and Zed collab (switched off in Mnemonic, needs a central
  server, refuses external agents for guests).

Jay picked A after the spike.

## Decided

- Approvals on both Macs, first answer wins (Jay: "both"). What asks
  (Jay picked Claude's proposal): reading and searching (Read, Glob,
  Grep) never ask; edits (Edit, Write) follow Dan's
  mode, asking in ask mode and not in accept-edits mode; Bash asks unless
  Dan's chat is in the skip-all-approvals mode (Mnemonic's current default
  for Claude). Since Claude's accept-edits mode only covers its built-in
  tools, Mnemonic pre-allows the read tools in session setup and answers
  edit prompts itself in accept-edits mode. Dan may answer "always allow"
  for his own chat; Jay's answers are one at a time, as in step 2. Approvals
  ride Claude's own permission system on Dan's side. When Dan's Mnemonic receives a permission request for a
  chat in Jay's workspace, it shows it in Dan's chat and also sends it to
  Jay's Mac; Jay's answer comes back over long-poll. Whichever arrives
  first resolves the request; the other side's prompt closes, naming who
  answered. Jay's share server therefore never asks again when the tool
  call arrives. (Claude's call on the mechanism.)
- The tools are served by Jay's share server as an MCP endpoint over HTTP,
  one per shared workspace, passed to Dan's Claude as an `McpServer::Http`
  with the headers the request checks need. No helper process on Dan's
  Mac. (Claude's call.)
- The tools mirror Claude's own built-ins in name meaning and input shape
  (`file_path`, `offset`/`limit`; `old_string`/`new_string`/`replace_all`;
  `command`/`timeout`; Grep's `pattern`/`path`/`glob`/`output_mode`), so
  Claude uses them as it uses its own: Read, Write, Edit, Glob, Grep,
  Bash (current Claude has no MultiEdit or LS). Paths are absolute paths in Jay's folder, which the tools
  report as the working directory. Edit fails if `old_string` doesn't match
  exactly once, as Claude's does, so concurrent edits by Jay or his agent
  fail cleanly instead of clobbering. (Claude's call.)
- Session setup for these chats: built-in file and shell tools off via
  `disallowedTools` (keep web fetch, web search, todo, task tools), the MCP
  server added, and an appended system prompt saying the workspace lives on
  Jay's Mac at that path and is reached only through these tools, plus the
  contents of Jay's project instruction files (`CLAUDE.md`, `AGENTS.md`)
  read at session start. (Claude's call.)
- The chat's working directory on Dan's Mac is an empty per-workspace
  folder in Mnemonic's own data, so Claude loads none of Jay's project
  settings or hooks on Dan's Mac and the transcript resumes from a stable
  place. (Claude's call.)
- Commands run on Jay's Mac as Jay's user, in the workspace root, with the
  environment Jay's own terminals in that workspace get, plus
  `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` set to Dan's git identity (sent by
  Dan's Mnemonic from his git config). Default timeout 2 minutes, maximum
  10, output truncated like Claude's Bash. A stopped chat kills its running
  command. (Claude's call.)
- Writes and edits go to disk, and an open, unchanged editor on Jay's
  side reloads them; a file with unsaved edits in Jay's editor is refused
  with an error telling the agent so, as step 3 does. (Claude's call.)
- File tools are confined to the workspace folder: no `..`, no paths
  through a symlink out, nothing inside `.git` written. The step 3 secret
  floor does not apply to the agent's tools, since its shell can read
  those files anyway and Jay trusts the team. (Claude's call; flagged to
  Jay.)
- The new switch "Teammates' agents can work here" sits with the other
  per-workspace switches in the Team window and the sidebar menu, on by
  default when Jay shares, independent of "Share files". (Claude's call,
  following Jay's earlier ask that sharing turns everything on.)
- Jay sees Dan's chat: Dan's Mnemonic sends the chat's entries to Jay's
  share server as they change; Jay's Mnemonic shows them as a read-only
  tab in that workspace, built with the step 1 mirror, with Stop and the
  approval prompt. (Claude's call.)
- On Dan's side, Jay's mirrored workspace view gets a "New chat in Jay's
  workspace" action next to "Work on this in your copy". These chats are
  normal chat tabs (Dan can type), shown alongside Jay's read-only ones.
  (Claude's call.)
- Dan's own user-level Claude setup (his MCP servers such as Linear or
  Slack, his skills, his hooks) loads as in any of his chats; only the
  built-in file and shell tools are swapped. A skill that runs a script by
  a path on Dan's Mac will run it on Jay's Mac and may fail; accepted.
  (Claude's call.)
- Dan's chat appears on Jay's Mac only, not to other teammates following
  Jay's workspace. Jay's Mac keeps the chat's transcript in Mnemonic's own
  data as a record of what ran there, and keeps showing it after Dan closes
  it or goes offline, until Jay closes the tab. (Claude's call.)
- The new switch turns on for workspaces already shared when Mnemonic
  updates, as Jay said he wouldn't share anything a teammate shouldn't be
  allowed to do. (Answered by Jay's earlier ask.)
- Dan's chats in Jay's workspace stay in Dan's sidebar under Jay's
  workspace's last known entry while Jay is offline, and work again when
  he is back. (Claude's call.)
- When Jay's Mac can't be reached or the switch is off, tool calls fail
  with a clear message ("Jay's workspace isn't reachable" / "Jay turned
  this off"); the chat stays and works again when Jay is back.
  (Claude's call.)

## Left to the builder

- Wire routes and payloads for the MCP endpoint, the approval relay, and
  chat-entry upload; how long-running Bash calls are kept within MCP HTTP
  timeouts.
- Whether to preload the tools so Claude skips the ToolSearch step.
- How Dan's chat records are stored and tied to (Jay's host, workspace id).
- Phasing: a sensible order is tools and session setup first (Dan's agent
  works in Jay's folder, approvals on Dan's side only), then approvals on
  both sides, then Jay seeing Dan's chat.
