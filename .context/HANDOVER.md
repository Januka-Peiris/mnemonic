# Mnemonic - Session Handover

## What's done this session

### Commits on jay/fix-window-move

| Hash | What |
|---|---|
| 7c487c5 | Fix window dragging, redesign projects panel, clean up warnings |
| 90742ca | Wire workspace click to launch claude in center terminal |

### Changes made

1. **Fixed window dragging** - Added `title_bar::init(cx)` to main.rs (was only called from disabled collab_ui). Changed all `ActiveCall::global()` to `try_global()` in title_bar crate.

2. **Redesigned workspace panel as Projects panel** - Grouped by repo, collapsible sections, History bar at top, "+" buttons for new project and new worktree per project. Removed ProjectPanel and DebugPanel from dock.

3. **Wired workspace click to terminal** - Clicking a workspace row opens a terminal tab in the center pane using `SpawnInTerminal` + `TerminalPanel::add_center_terminal()`. Runs `claude --dangerously-skip-permissions` cd'd to workspace directory.

4. **Cleaned up** - Removed all seed/sample data, fixed all compiler warnings across mnemonic-panels/bridge/main.rs, removed unused imports.

## What needs doing next

### Priority 1: Wire up "+" buttons (Steps 2-3 from plan)

**"+" on project group row** (add worktree):
- Find repository info for that project via mnemonic-core
- Call `workspace_service::create_workspace()` with `CreateWorkspaceInput`
- This calls `git_worktree_service::create_mnemonic_worktree()` which does git fetch + worktree add
- Refresh panel, open terminal with claude in the new worktree
- Key files: `crates/mnemonic-core/src/services/workspace_service/ops.rs`, `git_worktree_service.rs`

**"+" in header** (add project):
- Use Zed's `prompt_for_open_path` to pick a local git repo directory
- Register via `repository_repository` or `repo_scanner_service`
- Refresh panel

### Priority 2: Terminal persistence (Step 5)

When app relaunches, workspaces should remember their terminal state (at minimum the cwd). On workspace click, restore to the right directory. mnemonic-core already stores `worktree_path` per workspace.

### Priority 3: Further cleanup

- The Zed sidebar (multi_workspace sidebar for agent switching) may overlap with our Projects panel concept. Consider whether to keep both or consolidate.
- Auth: use existing Claude Code credentials or just set ANTHROPIC_API_KEY in Mnemonic settings

## Key architecture notes

- `TerminalPanel::add_center_terminal()` (terminal_panel.rs:752) creates terminal tabs in the center pane
- `project.create_terminal_task(SpawnInTerminal)` (project/src/terminals.rs) spawns commands in terminals
- `workspace_service::create_workspace()` (workspace_service/ops.rs:18) handles full workspace creation including git worktree
- `git_worktree_service::create_mnemonic_worktree()` (git_worktree_service.rs:11) creates git worktrees
- Path pattern for managed worktrees: `{managed_root}/{repo_name}/{workspace_id}`

## Build and run

```bash
cd /Users/jay/conductor/workspaces/mnemonic/hyderabad-v1
cargo run -p mnemonic
```

Delete `~/Library/Application Support/Mnemonic/forge.sqlite3` to reset workspace data.
