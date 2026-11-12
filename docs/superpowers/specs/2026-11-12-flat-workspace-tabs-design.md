# One tab row per workspace, a Team window, and a Setup / Run / Terminal panel

## Why

Each window has one outer tab row (Zed's workspace center). A Mnemonic workspace's agent chats live in a second tab row inside one outer tab (`WorkspaceHost`). Everything else - files from the file tree, transcripts, the Team tab, teammate views, editors opened by the detail panel - is added to the outer row, which never changes on a workspace switch. So workspace A's files stay on screen in workspace B, and two rows of tabs stack on top of each other.

Jay wants it to work like Conductor: the top of the screen shows only the current workspace's tabs, one row.

## Goals

- One visible tab row, belonging to the selected workspace. Switching workspaces swaps the whole row; nothing from another workspace is ever shown.
- Row contents, left to right: one **file tab**, then agent chat tabs (chats and Claude/Codex CLI sessions), then `+`.
- **File tab:** opening any file (file tree, Changes, diffs, transcripts, `open_abs_path`) shows it in this single tab, replacing the previous file. File icon plus file name. It appears once a file has been opened in the workspace; each workspace keeps its own.
- **`+`:** one click opens a new chat with the default agent in this workspace. Right-click (or a small chevron next to it) gives the other kinds: each chat agent, Claude CLI, Codex CLI. Plain terminals are no longer offered here.
- **Splits stay**, reached by right-clicking a tab (Split Right / Split Down, already in `pane.rs`'s tab menu) and by dragging a tab to an edge. The dedicated split button goes.
- **Auto-fix (Keep CI green) toggle** moves from the tab row to the right panel's header (`workspace_detail_panel.rs`, beside All files / Changes / Checks), acting on the selected workspace, with the same tooltips.
- **Team window:** "team: open team settings" and the sidebar gear open a separate window, like Zed's settings window. Teammate workspace views (`team_view.rs`) open as tabs inside that window (a "Team" tab plus one per teammate workspace opened). Sidebar clicks on a teammate workspace open or focus that window on that tab.
- **Setup / Run / Terminal panel:** a new panel stacked under the right panel in the right dock (reusing the stacking from #20), with a collapse chevron and draggable divider. It always shows the selected workspace.
  - **Setup:** output of the workspace's setup commands (`.mnemonic/config.json` `setup`, run by `run_workspace_setup`). When there are none, an empty state: "Add setup script", "Run commands when a workspace is created to install dependencies or set up the environment", with **Ask agent** (opens a new chat in the workspace asking the agent to add setup commands to `.mnemonic/config.json`) and **Add manually** (opens Project settings, which gains a Setup script field next to the existing Run script field and is used the same way).
  - **Run:** the run commands' terminals, which today open as tabs in the chat row. The Run button and its restart/stop controls (`render_run_controls`) move from the workspace header to the right end of this panel's tab strip.
  - **Terminal:** plain shells in the workspace folder; `+` adds another shell tab. Shell sessions that opened in the chat row today open here instead.

## Non-goals

- Keeping the file tab across app restarts.
- Changing how chats, agents, tmux persistence or the composer work.
- Changing the sidebar, the right panel's existing tabs, or sharing.
- New keyboard shortcuts.

## Anti-goals

- Losing unsaved edits when the file tab is replaced.
- Any tab, file or terminal of one workspace showing while another is selected.

## Decided approach

- Keep `WorkspaceHost` and each `WorkspaceView`'s own `PaneGroup`; those panes become the single visible row. Hide the outer pane's tab bar whenever its only item is the `WorkspaceHost` (it stays visible as a fallback when no workspace is selected).
- Adopt outer items: when any item other than `WorkspaceHost` is added to an outer pane while a workspace is selected, move it into that workspace's active pane. Items with a project path take the file slot; anything else (diagnostics, keymap editor and similar) becomes an ordinary tab in that row. If the same path is already open in the workspace, activate that tab and drop the new one.
- File slot replacement: the old file item is closed only if it has no unsaved changes; otherwise it stays as an ordinary tab and the new file takes the slot.
- Items Zed restores into the outer pane at launch are closed if clean; dirty ones are adopted into the first workspace that gets selected.
- The Setup / Run / Terminal terminals stay owned by each `WorkspaceView` (so they follow it on switch); the panel renders the selected workspace's set.

## Left to the builder

- Exact styling, as long as it reads like the Conductor screenshots (file tab first with a file icon; `+` at the end; Setup / Run / Terminal tab strip with Run on the right).
- How the panel reads the selected workspace's terminals, and how the Setup tab shows core-managed setup sessions' output.
- Any other controls found in `render_pane_controls` get a sensible new home; note them in the PR.

## My calls (not discussed with Jay)

Ask agent / Add manually behaviour; the dirty-file rule; restored-item handling; the Team window's tab layout; dropping "Terminal" from the `+` menu.
