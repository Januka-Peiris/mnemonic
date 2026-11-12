# Flat Workspace Tabs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One tab row per workspace (file tab + agent chats + `+`), Team in its own window, and a Setup / Run / Terminal panel under the right panel.

**Architecture:** Keep `WorkspaceHost` (the single outer tab, whose tab bar already hides when alone via `hides_tab_bar_when_alone`) and each `WorkspaceView`'s own `PaneGroup`. Anything else added to the outer workspace is adopted into the selected `WorkspaceView`'s panes, so the outer row never shows. Shells and run/setup terminals leave the `WorkspaceView` panes and render in a new right-dock panel that stacks under `WorkspaceDetailPanel` (dock stacking from #20).

**Tech Stack:** Rust, GPUI, Zed `workspace` crate (fork).

**Spec:** `docs/superpowers/specs/2026-11-12-flat-workspace-tabs-design.md`

## Global Constraints

- No em dashes anywhere (code, comments, strings, commits).
- No `unwrap()`; no `let _ =` on fallible calls; comments only for non-obvious why.
- No `mod.rs`; prefer existing files; one new file only for the new panel (`crates/mnemonic-panels/src/workspace_tools_panel.rs`).
- Use `./script/clippy`, not `cargo clippy`.
- No new keyboard shortcuts.
- Tooltips for the auto-fix toggle stay word for word as in `workspace_view.rs:3756-3764`.

## Review Focus

- Opening a file that is already open in this workspace: activates that tab, no duplicate.
- Replacing the file tab while it has unsaved edits: the old file stays as a normal tab; nothing is lost.
- Switching workspaces with a file open, then back: each workspace shows only its own file tab.
- No workspace selected (fresh window): opening a file still works and shows in the outer row.
- Archiving a workspace with a running dev server: run terminals in the new panel are still killed (`kill_run_terminals`) and teardown still runs.

---

### Task 1: Team window

**Files:**
- Modify: `crates/mnemonic-panels/src/team_settings.rs` (add `TeamWindow`, drop `Item` impl for `TeamSettingsView`)
- Modify: `crates/mnemonic-panels/src/team_view.rs` (drop `TeamWorkspaceView::open` and its `Item` impl; keep the view)
- Modify: `crates/mnemonic-panels/src/team.rs:162-164, 926, 1143` (call sites)

**Interfaces:**
- Produces: `pub(crate) enum TeamWindowTab { Settings, Teammate { host: String, workspace_id: String } }` and `pub(crate) fn open_team_window(tab: TeamWindowTab, languages: Arc<LanguageRegistry>, cx: &mut App)` in `team_settings.rs`.

- [ ] **Step 1:** Add `TeamWindow` (a `Render` view) holding `settings: Entity<TeamSettingsView>`, `teammates: Vec<Entity<TeamWorkspaceView>>`, `active: TeamWindowTab`. It renders a tab strip ("Team", then one tab per teammate view titled by its `tab_content_text` equivalent, each with a close button) over the active view. A `Global` holds `Option<WindowHandle<TeamWindow>>`; `open_team_window` focuses the existing window (adding/activating the teammate tab, reusing one with the same `host` + `workspace_id`) or opens a new one with the same `WindowOptions` shape as `settings_ui.rs:799` (title "Mnemonic - Team", min size 700x400).
- [ ] **Step 2:** `OpenTeamSettings` (team.rs:162) and the sidebar gear (team.rs:926) call `open_team_window(TeamWindowTab::Settings, ...)`; the teammate click (team.rs:1143) calls it with `Teammate`. Languages come from `workspace.project().read(cx).languages().clone()`.
- [ ] **Step 3:** `cargo check -p mnemonic-panels` passes; commit "Open Team in its own window".

### Task 2: Adopt outer items into the workspace's tab row, with one file tab

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`WorkspaceHost::activate_workspace` ~4096, `WorkspaceView` struct ~691, `is_session_tab_of` ~514, `active_is_chat` ~1856, tests module)

**Interfaces:**
- Produces: `enum FileSlotPlan { ActivateExisting(EntityId), Place { close: Option<EntityId> } }`; `fn plan_file_slot(same_path_item: Option<EntityId>, current_file: Option<(EntityId, bool)>) -> FileSlotPlan` (the bool is "has unsaved changes"); `WorkspaceView::adopt_item(&mut self, item: Box<dyn ItemHandle>, window, cx)`; field `file_item: Option<EntityId>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn file_slot_replaces_clean_file_keeps_dirty_one_and_reuses_open_path() {
    let (a, b) = (EntityId::from(1u64), EntityId::from(2u64));
    assert_eq!(plan_file_slot(Some(b), Some((a, false))), FileSlotPlan::ActivateExisting(b));
    assert_eq!(plan_file_slot(None, Some((a, false))), FileSlotPlan::Place { close: Some(a) });
    assert_eq!(plan_file_slot(None, Some((a, true))), FileSlotPlan::Place { close: None });
    assert_eq!(plan_file_slot(None, None), FileSlotPlan::Place { close: None });
}
```

- [ ] **Step 2:** `cargo test -p mnemonic-panels file_slot` fails (not defined).
- [ ] **Step 3:** Implement `plan_file_slot`; test passes.
- [ ] **Step 4:** `adopt_item`: items with a `project_path` go through `plan_file_slot` (same-path lookup across all of this view's panes; close the old file with `remove_item` only when the plan says so; place the new item at index 0 of `center.first_pane()` and set `file_item`). Items without a project path are added to `active_pane` as ordinary tabs. Allow file/other adopted items in `is_session_tab_of` so they can be dragged and split. `active_is_chat` stays as is, but the composer must only show when the active item is a CLI agent terminal session (not a file).
- [ ] **Step 5:** In `WorkspaceHost::activate_workspace`, subscribe once per window (store the `Subscription` on the host) to `workspace::Event::ItemAdded`; when the added item is not a `WorkspaceHost` and a view is active, take it out of its outer pane (`pane.remove_item(id, false, false, ...)` without closing it) and call `adopt_item`. On first activation also sweep existing outer items: close clean ones, adopt dirty ones. With no workspace selected nothing is adopted.
- [ ] **Step 6:** `cargo test -p mnemonic-panels` and `cargo check -p zed` pass; commit "Keep files in their own workspace's tab row".

### Task 3: Tab row controls

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_view.rs:3724-3843` (`render_pane_controls`)
- Modify: `crates/workspace/src/pane.rs` tab context menu (~3140-3240): add `pub fn set_split_in_tab_menu(&mut self, enabled: bool)`; when enabled, the menu gains "Split Right" and "Split Down", which activate the clicked tab then emit `Event::Split { direction, mode: SplitMode::MovePane }`.

- [ ] **Step 1:** In `new_session_pane` call `pane.set_split_in_tab_menu(true)`. Remove the split `PopoverMenu`, `render_run_controls` and the auto-fix label/toggle from `render_pane_controls`. Keep diagnostics, linked chats and the status chip.
- [ ] **Step 2:** `+` becomes an `IconButton` (tooltip "New chat") that calls `add_session(NewSession::from(AgentSessionKind::ClaudeChat), ...)` on that pane. Beside it a `ChevronDown` `PopoverMenu` lists each enabled chat agent, "Claude CLI" and "Codex CLI" (no "Terminal").
- [ ] **Step 3:** `cargo check -p zed` passes; commit "Simplify the workspace tab row".

### Task 4: Auto-fix toggle in the right panel header

**Files:**
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`WorkspaceHost` helpers near `enable_auto_fix_for_workspace` ~4435)
- Modify: `crates/mnemonic-panels/src/workspace_detail_panel.rs:1320-1420` (header)

**Interfaces:**
- Produces: `WorkspaceHost::auto_fix_for_workspace(workspace: &Workspace, workspace_id: &str, cx: &App) -> Option<(bool, SharedString)>` (enabled, state label) and `WorkspaceHost::toggle_auto_fix_for_workspace(workspace: &mut Workspace, workspace_id: &str, cx: &mut Context<Workspace>)`.

- [ ] **Step 1:** Header gets the state label and `CheckDouble` toggle (same colors and tooltips as before) to the left of the Review button. The detail panel observes the selected `WorkspaceView` entity (re-subscribe on selection change) so the toggle updates.
- [ ] **Step 2:** `cargo check -p zed` passes; commit "Move the Keep CI green toggle to the right panel".

### Task 5: Setup / Run / Terminal panel

**Files:**
- Create: `crates/mnemonic-panels/src/workspace_tools_panel.rs` (`WorkspaceToolsPanel`, a `Panel`: `DockPosition::Right`, `stacks_below_other_panels` true, persistent name "WorkspaceToolsPanel")
- Modify: `crates/mnemonic-panels/src/lib.rs`, `crates/zed/src/zed.rs:744-810` (load and add the panel; after panels load, if the right dock's active panel is `WorkspaceDetailPanel`, activate the tools panel so it stacks)
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`push_terminal` ~1332, `ensure_terminal_pane` ~1399, run commands ~1463-1730, `render_run_controls` ~1788, `add_shell_terminal` ~985)
- Modify: `crates/mnemonic-panels/src/workspace_panel.rs:1138` (`open_project_settings` becomes `pub(crate)`)

**Interfaces:**
- Produces on `WorkspaceView`: fields `shells: Vec<ToolTerminal>`, `run_terminals: Vec<ToolTerminal>`, `setup_terminals: Vec<ToolTerminal>`, `tools_tab: ToolsTab`; `pub(crate) enum ToolsTab { Setup, Run, Shell(usize) }`; `pub(crate) fn render_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement`; `fn is_setup_session_title(title: &str) -> bool` (true for "Repo setup" and titles starting "Setup ").
- Consumes: `WorkspaceHost::view_in` (make `pub(crate)`), `mnemonic_bridge::ActiveWorkspace::get`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn setup_tab_shows_repo_and_config_setup_sessions_only() {
    assert!(is_setup_session_title("Repo setup"));
    assert!(is_setup_session_title("Setup 1 · pnpm install"));
    assert!(!is_setup_session_title("Run 1 · pnpm dev"));
}
```

- [ ] **Step 2:** Run it, see it fail; implement; see it pass.
- [ ] **Step 3:** Terminals with no agent `provider` (shells, run, teardown) no longer become `sessions` or tabs: `push_terminal` puts them in `shells` or `run_terminals` (keep `run_command` semantics), and `ensure_terminal_pane` / `terminal_pane` are deleted. `has_run_terminals`, `kill_run_terminals`, `stop_run_commands`, `run_teardown_commands` work on `run_terminals`. Opening a shell or starting Run switches `tools_tab` and activates the tools panel in the dock.
- [ ] **Step 4:** `render_tools`: a tab strip "Setup", "Run", one "Terminal" / "Terminal N" per shell, `+` (new shell), and at the right end the run controls (`render_run_controls`) and a `ChevronDown` that calls `Dock::close_stacked_panel`. Setup tab: if setup terminals exist, show them; else list core sessions from `terminal_service::list_workspace_terminal_sessions` filtered by `is_setup_session_title`, showing each one's output text from `get_workspace_terminal_output_for_session` (poll every 2s while shown); if there is no project setup script and no `.mnemonic/config.json` setup commands, show the empty state from the spec. A "Run setup" button runs the project setup script (else config `setup`) as visible terminals in `setup_terminals`, the same way `start_run_commands` builds `SpawnInTerminal`. "Ask agent" opens a new default chat and dispatches: "Add this project's setup commands (installing dependencies, copying env files and similar) to the \"setup\" array in .mnemonic/config.json, creating the file if it doesn't exist." "Add manually" calls `WorkspacePanel::open_project_settings` for the workspace's project.
- [ ] **Step 5:** `WorkspaceToolsPanel::render` shows the selected `WorkspaceView`'s `render_tools`, or "Select a workspace". It re-renders on selection change and on the view's `notify`.
- [ ] **Step 6:** `cargo test -p mnemonic-panels`, `cargo check -p zed`, `./script/clippy` pass; commit "Add a Setup, Run and Terminal panel under the right panel".

### Task 6: Verify in the app

- [ ] **Step 1:** Build and launch Mnemonic Dev; check each spec goal by hand: file tab replaces, dirty file kept, switch workspaces and back, `+` and chevron, tab right-click Split Right, auto-fix toggle in the right header, Team window with a teammate tab, Setup / Run / Terminal panel (Run starts and stops, shells add, empty Setup state buttons).
- [ ] **Step 2:** Report what was and wasn't verified.
