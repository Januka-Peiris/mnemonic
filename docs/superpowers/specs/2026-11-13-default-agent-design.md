# Default agent setting

## Why

The first chat of a new workspace, the `+` new chat button and the new chat shortcut are hard-wired to Claude. Jay wants one default agent that applies everywhere, while still being able to open a chat with another agent in a workspace without changing the default. Chats already remember their own agent across restarts.

## Decided

- `workspaces.default_agent` in the settings file: `claude` (default), `codex`, `gemini`, `opencode`, `cursor`, shown as a Default Agent dropdown in Settings > Workspaces > Agents.
- Used by a new workspace's first chat, the `+` button, the new chat shortcut (`NewDefaultAgentSession`, via the new `AgentSessionKind::DefaultChat`) and the setup tab's Ask agent chat.
- The `+` menu's explicit agent choices open a chat with that agent and never write the setting.

## Approaches

1. A settings-file enum like the existing Review Agent setting (picked): persists with every other setting and gets a dropdown for free.
2. Remember the last agent picked: rejected, it changes the default when Jay only wanted a one-off.

## Left alone

- App instructions (CI fixes, review comments) still prefer the first Claude chat, then any chat.
- CLI sessions are unaffected.
