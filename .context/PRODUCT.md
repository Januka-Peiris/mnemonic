# Mnemonic — Product Model

This is the canonical description of what Mnemonic is and how it must behave.
Read this before changing panel/terminal/workspace behavior. Update it when
the model changes. (HANDOVER.md is per-session state; this file is the
durable vision.)

## What Mnemonic is

A Conductor.build replacement, built as a Zed fork, for running CLI coding
agents (claude, codex) in parallel git worktrees. Conductor stops being
usable for us from June 2026; Mnemonic replaces it. The earlier attempt
(Forge: Tauri + Rust backend + web frontend, at ~/dev/forge) was abandoned
for performance reasons, but its backend logic lives on as
`crates/mnemonic-core` (services, repos, SQLite models) and is the brain
behind the panels.

## Auth model

Agents authenticate through their own CLIs (`claude login`, `codex login`).
Mnemonic itself needs no AI provider keys and no Zed account. The built-in
Zed AI surface is disabled (`disable_ai: true` default); the zed.dev cloud
provider is unregistered.

## The core interaction model (Conductor parity)

Three regions, like Conductor:

1. **Left panel — Projects.** Projects (repos), each with its workspaces
   (worktrees) listed under it, with per-workspace diff stats. `+` on a
   project creates a workspace: new git worktree + branch, agent starts
   immediately. History at top.

2. **Center — the WORKSPACE, not just a terminal.** Clicking a workspace
   opens the *workspace's own view* in the center. The primary terminal
   (running claude) lives *inside* that view, and the user can open more
   terminals within the same workspace view (Conductor's Terminal "+").
   The workspace view is the unit of switching: click another workspace,
   you switch to *its* view; click back, everything is exactly as you left
   it. The terminal is a component of the workspace, not the thing itself.

   **Workspaces do NOT stack as center tabs.** Jay works across 10–20
   workspaces; a tab per workspace is unusable. The left panel is the
   switcher; the center has ONE `WorkspaceHost` item that swaps which
   workspace's view it shows. Views are cached inside the host so
   switching back is instant with terminals and scrollback intact.

   Below the terminals sits the **agent composer** (from Forge): type an
   instruction, Enter sends it into the claude TUI. Dispatch must be the
   prompt text and the carriage return as SEPARATE PTY writes ~250ms apart
   (`ENTER_DELAY_MILLIS`), or the TUI treats Enter as pasted newline —
   this is Forge's proven pattern from
   `terminal_service/queue.rs::dispatch_prompt_entry`.

3. **Right panel — Workspace detail.** For the selected workspace: status
   header (name, branch, ahead/behind, status, PR number), Changes list
   (per-file +/−), and later: file tree of the worktree, Checks, and
   Setup/Run script tabs (mnemonic-core's `workspace_script_service`
   already implements the script logic from Forge).

## Terminal persistence rules (critical, do not regress)

- Every workspace's primary agent terminal runs inside a tmux session on a
  dedicated private socket (`tmux -L mnemonic`), one session per workspace
  (`mnemonic-<workspace_id>-main`). Implemented by
  `terminal_service::embedded_workspace_terminal_command()` in
  mnemonic-core, reusing Forge's tmux module.
- Closing the tab, switching workspaces, or quitting the app must NOT kill
  the agent. Reopening the workspace reattaches to the same live session
  with scrollback and the running claude conversation intact.
- Clicking a workspace must NEVER spawn a duplicate terminal if one exists —
  find and focus the existing one (across all panes/views).
- Falls back to plain `claude` if tmux is not installed.

## What was stripped from Zed (hide-first, crates intact)

- `disable_ai: true` → agent panel, inline assist, AI onboarding gone
  (flip the setting to bring back the ACP Claude panel if ever wanted).
- zed.dev cloud language-model provider unregistered.
- Title-bar sign-in hidden (`show_sign_in: false` default), collab menu
  item removed, Extensions menu item removed, edit predictions off.
- Collab/notification/chat panels were already not registered.

## Build & run

```bash
cd /Users/jay/dev/mnemonic
cargo run -p mnemonic        # or double-click run-mnemonic.command
```

SQLite state: `~/Library/Application Support/Mnemonic/forge.sqlite3`
(delete to reset workspaces). Agent tmux sessions:
`tmux -L mnemonic ls` / `tmux -L mnemonic kill-server` to inspect/reset.

## Key integration points

- `crates/zed/src/zed.rs` `initialize_panels()` — panel registration.
- `crates/mnemonic-panels/` — WorkspacePanel (left), WorkspaceDetailPanel
  (right), CommandApprovalPanel + CoordinatorPanel (bottom).
- `mnemonic_panels::ActiveWorkspace` global — selected workspace id;
  set by the left panel, observed by the right panel.
- `mnemonic_bridge::mnemonic_state(cx)` — AppState (db + services) from
  any panel. `mnemonic_event_hub(cx)` — core→UI events.
