# Mnemonic - Zed Fork Handover

## What exists

A working Zed fork at `/Users/jay/dev/mnemonic/` (GitHub: `Januka-Peiris/mnemonic`, private repo) with mnemonic-core integrated as the backend brain.

### Commits on main

| Hash | What |
|---|---|
| fbab710 | Fork Zed, rename to Mnemonic, integrate mnemonic-core + bridge |
| c2842d7 | Polish workspace panel, seed sample data |
| c6e3736 | Add command approval + coordinator panels, disable collab |
| dde0832 | Rebrand agent from Zed to Mnemonic |
| f644359 | Fix window dragging, restore sidebar, suppress empty buffer |

### Architecture

```
crates/mnemonic-core/     - 29K-line backend (services, repos, models, SQLite)
crates/mnemonic-bridge/   - Event bridge: ZedEventEmitter, Global state, async relay
crates/mnemonic-panels/   - Workspace panel, command approval panel, coordinator panel
crates/zed/               - Main app binary (renamed to "mnemonic")
```

Integration surface in Zed is minimal:
- `crates/zed/src/main.rs` line 656: `mnemonic_bridge::init(cx)` 
- `crates/zed/src/zed.rs` lines 749-756: three panel registrations in `initialize_panels()`
- `crates/paths/src/paths.rs`: APP_NAME = "Mnemonic"
- Agent system prompts: "Mnemonic editor" instead of "Zed editor"
- Provider help text: "Mnemonic's agent" instead of "Zed's agent"
- collab init calls commented out in main.rs (lines 747, 768-770)

### Key files in the bridge

- `crates/mnemonic-bridge/src/lib.rs`: `init()` creates AppState, sets Global, starts event relay thread, seeds sample workspaces on first run
- Access state from any panel: `mnemonic_bridge::mnemonic_state(cx)` (cx derefs to App)
- Subscribe to events: `mnemonic_bridge::mnemonic_event_hub(cx)` returns Entity<MnemonicEventHub> which emits `MnemonicEvent { name, payload }`

### Data

- SQLite at `~/Library/Application Support/Mnemonic/forge.sqlite3`
- 3 seeded sample workspaces (created on first launch if DB is empty)
- Zed's own data alongside in same directory (db/, threads/, extensions/)

## What needs doing next

### Priority 1: Unified left panel

The user wants a single left panel that combines:
1. **Project file tree** (top) - like Zed's ProjectPanel
2. **Workspaces list** (below) - the current workspace panel content

Currently these are separate dock panel tabs. The user's vision (from their old app) is one unified view.

**Approach options:**
- **Option A (cleaner):** Modify `WorkspacePanel` to embed Zed's project tree component above the workspace list. This means depending on `project_panel` internals or re-implementing a simpler file tree.
- **Option B (quicker):** Create a container panel that renders both the ProjectPanel and WorkspacePanel vertically stacked. Needs to handle focus delegation and resizing between the two halves.
- **Option C (most Zed-native):** Use Zed's panel system with the workspace panel as a "sub-panel" within the project panel, or vice versa. Check if Zed has a concept of panel composition.

**Reference:** Look at the user's old app screenshot (Conductor workspace, second image in attachments). It had a sidebar with project list on the left, workspace items listed in the main area.

**Key constraint:** "Extend, don't mash" - build on top of Zed's existing components, don't rewrite them.

### Priority 2: Auth - use existing Claude Code credentials

The user wants to use their existing Claude Code terminal auth (OAuth tokens from `claude` CLI) instead of signing in through Zed's UI. 

Zed's Anthropic provider (`crates/language_models/src/provider/anthropic.rs`) requires an API key set in settings. The simplest path is probably:
1. Check for `ANTHROPIC_API_KEY` env var (often set in shell)
2. Or auto-detect from Claude Code's auth storage
3. Or just document: "set your API key in Mnemonic settings once"

Claude Code's auth is OAuth-based (not a simple API key file), so extracting it may not be straightforward.

### Priority 3: Polish

- The title bar area and workspace panel overlap may still need tweaking depending on sidebar state
- Clean up compiler warnings (6+ unused variable warnings in panels)
- The workspace panel currently refreshes on every render (calls `list_workspaces` each frame) - should debounce or use event-driven refresh
- Seed data should eventually be removed once real workspace creation works

## Build and run

```bash
cd /Users/jay/dev/mnemonic
cargo run -p mnemonic          # debug build
cargo build -p mnemonic        # just build
```

First build: ~5 minutes. Incremental: ~10-15 seconds.

Rust toolchain: 1.95.0 (pinned in rust-toolchain.toml).

## Key Zed panel patterns

Panel trait requires: `persistent_name`, `panel_key`, `position`, `icon`, `toggle_action`, `default_size`, `activation_priority` (must be unique per dock), plus `Focusable`, `EventEmitter<PanelEvent>`, `Render`.

Panels are loaded in `crates/zed/src/zed.rs` `initialize_panels()` function. Each gets `workspace_handle.clone()` and `cx.clone()` (except the last one which consumes cx).

`activation_priority` must be unique per dock position or the app panics on startup. Current assignments:
- 0: AgentPanel (left dock, Zed's)
- 1: ProjectPanel (left dock, Zed's)
- 2: WorkspacePanel (left dock, ours)
- 3: GitPanel (left dock, Zed's)
- 5: CommandApprovalPanel (bottom dock, ours)
- 6: CoordinatorPanel (bottom dock, ours)
