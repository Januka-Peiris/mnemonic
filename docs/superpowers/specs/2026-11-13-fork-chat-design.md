# Fork an agent chat into a new tab

## Why

Jay often wants to take a chat in a second direction while keeping the first one, on the same branch in the same workspace. Conductor offers this as "fork". The Claude agent Mnemonic runs (claude-agent-acp v0.106) already supports ACP `session/fork`, and Mnemonic's `agent-client-protocol` dependency already has `unstable_session_fork` enabled; nothing in the app calls it.

## Goals

- A **Fork** button in the footer of the agent's latest reply, beside the existing copy button.
- Clicking it forks the agent session and opens a new chat tab in the same workspace (same folder, same branch) with the full conversation. The original chat is untouched.
- The new tab is titled after the original with " (fork)" appended, is stored like any other chat and resumes after a restart.
- The button is disabled while the agent is generating, and not shown when the agent does not advertise the `session.fork` capability (Codex, CLI terminal sessions, other agents).

## Non-goals

- Forking into a new workspace (possible later as a second option on the same button; a new workspace would start on a clean branch with only the conversation).
- Forking from an earlier message. ACP `session/fork` copies the whole session, so the button appears only on the latest reply.

## Anti-goals

- Changing or truncating the original chat's history.
- A forked chat that cannot be resumed after a restart.

## Approaches

1. **ACP `session/fork`** (picked): the agent copies its own session, so the new chat has exactly the original's context, tools and model state.
2. Copy the Claude transcript file and resume it under a new ID: depends on Claude Code's private file layout and breaks when it changes.
3. Start a new chat and paste a summary: loses fidelity, and is not what Jay asked for.

## Decided approach

- Add `supports_fork_session` and `fork_session` to `AgentConnection` (`crates/acp_thread/src/connection.rs`), implemented in `AcpConnection` (`crates/agent_servers/src/acp.rs`) by sending `ForkSessionRequest { session_id, cwd }` and returning the new session ID.
- `workspace_view.rs` gains a fork path: call `fork_session` on the chat's connection, write a new chat record (same agent and kind, title + " (fork)", `acp_session_id` = the forked ID), then open it through the existing resume path, as a new tab after the original.
- The thread view's reply footer shows the Fork button on the latest assistant reply, wired to that path.

## Left to the builder

- How the thread view's footer reaches the workspace view (event, action or callback), following existing patterns.
- Whether the new tab takes focus (default: yes, like a new chat).
- Error handling: a failed fork shows the existing error toast and opens nothing.
