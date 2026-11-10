# Chat-first workspaces

## Intent

Mnemonic is moving toward a Conductor-style app: when you open a worktree, the
center of the screen is a chat with Claude or Codex, not a CLI running in a
terminal. Today every session in a workspace is a CLI (claude, codex, review,
shell) running in a tmux-backed terminal, driven by Mnemonic's composer bar.
Jay wants a proper chat (messages, tool calls, diffs, image attachments) as the
default, with the CLI still available when needed.

This first version drops Zed's existing agent chat (`agent_ui::ConversationView`,
the ACP thread UI behind the agent panel) into the workspace view as-is.
Restyling it toward Conductor's look comes later and is not part of this work.

## Goals

- Opening a workspace starts a **Claude chat** session in its center view.
- Chat sessions are a new session kind alongside the existing terminal ones, in
  the same tab row. The "+" menu offers: Claude chat, Codex chat, Claude CLI,
  Codex CLI, Shell.
- Chats use the ACP agents `claude-acp` and `codex-acp` (Zed's agent server
  registry), with the chat's own message box, model picker and Zed's existing
  image paste/drop attachment support.
- Mnemonic's composer bar (Send/Interrupt, model/effort/Bypass chips) is hidden
  on chat tabs and stays on terminal tabs.
- Create PR, Fix errors, the CI auto-fix loop and Review send their
  instructions into the workspace's chat. If the workspace has no chat session,
  they fall back to the claude CLI terminal as today.
- Chats survive an app restart: on relaunch each workspace's chat tabs come
  back and resume the same agent session with its history.

## Non-goals

- No restyling of Zed's chat toward Conductor (collapsed tool-call groups,
  per-turn file chips, "Next unread", Conductor-style composer). Later work.
- No moving the CLI into a bottom-right terminal area like Conductor. CLI
  sessions stay as tabs in the center, as today.
- Zed's separate agent side panel is not brought back as a visible feature.
- The zed.dev cloud model provider stays unregistered; sign-in stays hidden;
  edit predictions stay off.

## Anti-goals

- Breaking the existing CLI sessions, tmux persistence, or auto-fix/PR flows
  for workspaces that use them.
- A chat that silently loses its conversation on restart.

## Constraints and current state

- Workspace UI: `crates/mnemonic-panels/src/workspace_view.rs`.
  `WorkspaceView` holds `terminals: Vec<Entity<TerminalView>>`,
  `terminal_titles`, `active_terminal`, the composer and agent state.
  `WorkspaceHost` shows one workspace at a time and exposes
  `start_review_for_workspace`, `enable_auto_fix_for_workspace`,
  `dispatch_to_workspace_agent` (used by the detail panel and title bar).
  `dispatch_to_agent` / `dispatch_target` route text to a terminal today.
- Zed chat: `crates/agent_ui/src/conversation_view.rs`, `ConversationView::new`
  takes the agent server, `AgentConnectionStore`, `resume_session_id`,
  `thread_id`, `work_dirs`, `workspace`, `project`, `thread_store`, etc., and is
  a standalone `Render` entity. `agent_panel.rs` (~line 4549) is the reference
  for how to construct one.
- Resume: ACP connections expose `supports_load_session` /
  `supports_resume_session` (`crates/acp_thread/src/connection.rs`); pass the
  saved session id as `resume_session_id`.
- `disable_ai` defaults to `true` in Mnemonic (`assets/settings/default.json`,
  commit ac050a8f7a). It hides agent actions and may gate the agent servers.
  Chats must work with Mnemonic's defaults, without the user editing settings.
- `claude-acp` and `codex-acp` must be available by default (Jay's Zed settings
  register them via `agent_servers` with `"type": "registry"`, and claude-acp
  with `default_config_options.mode = "bypassPermissions"`).
- Follow the repo `CLAUDE.md` Rust/GPUI rules (no `unwrap`, no `let _ =` on
  fallible calls, no `mod.rs`).

## Decided by Jay

- Chat is a different kind of session, and the default. CLI is optional.
- Use Zed's chat UI and its own message box for now; restyle later.
- PR / Fix errors / auto-fix / Review go to the chat.
- Chats resume after restart.
- Image paste/drop must work in the chat.
- Existing workspaces (created before this ships) are left as they are: no
  automatic chat tab. They keep their CLI sessions and the CLI fallback for
  PR/auto-fix until the user adds a chat via "+". Only newly created workspaces
  start with a Claude chat. If the user closes a workspace's last chat, no new
  one is created automatically.

## Claude's calls (Jay to check)

- Fallback to the claude CLI when a workspace has no chat.
- Attention dots and the working/idle status chip read the chat's thread state
  (generating vs idle, waiting for approval) instead of terminal output.
- Archiving a workspace closes its chat sessions.
- Review opens (or refocuses) its own dedicated "Review" chat tab with the
  existing review prompt and Sonnet, mirroring today's separate review session;
  it does not go into the main chat.
- Dispatch target for Create PR / Fix errors / auto-fix: the active tab if it is
  a chat (not the Review chat), otherwise the first Claude chat, otherwise the
  first chat, otherwise the claude CLI.
- Both Claude and Codex chats run without approval prompts (bypass / full
  auto), so unattended auto-fix never stalls; the composer's Bypass chip
  applies to CLI tabs only.
- New chats start on the ACP agent's own default model; the chat's model
  picker is the only model control on chat tabs. No bridge to the workspace's
  CLI model/effort state.
- If an ACP agent is unavailable (registry not fetched yet, offline, install
  failure), the chat tab shows the error and an "Open Claude CLI instead"
  action rather than an empty tab.
- Ship `claude-acp` / `codex-acp` and whatever AI-setting change chats need as
  Mnemonic defaults; keep Zed's agent side panel button and "agent" command
  palette namespace hidden.

## Left to the builder

- How the session list is modelled (e.g. an enum over terminal and chat
  sessions) and where chat session ids are persisted (alongside the existing
  workspace/session storage in `mnemonic-core`, or the Zed workspace DB).
- How dispatched instructions are inserted and sent in the chat (via the
  thread's message editor or the ACP thread send API).
- Which agent-ui init and settings changes are needed so chats work while the
  rest of the built-in AI surface stays off.
