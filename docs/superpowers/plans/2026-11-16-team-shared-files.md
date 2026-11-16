# Team Shared Files Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A teammate gets a git-backed copy of a shared workspace (including `.context/` and uncommitted files), runs his own agent in it, and sends edits back as attributed batches that the sharer can review, resolve and undo.

**Architecture:** The sharer's existing share server gains file routes: a long-polled manifest of files that differ from the workspace's HEAD commit, file contents, a git bundle of commits not on `origin`, and a POST for a batch of edits. The viewer builds a Mnemonic worktree from his own clone at the sharer's HEAD, overlays the manifest's files, and follows it. Batches are applied on the sharer's side against per-file base fingerprints; mismatches and command-running files are held back. Step 2's "message the sharer's agent" route is removed.

**Tech Stack:** Rust, GPUI, `tiny_http` (server), `reqwest::blocking` (client), `git` CLI, `sha2` for fingerprints.

**Spec:** `docs/superpowers/specs/2026-11-16-team-shared-files-design.md`

## Global Constraints

- Peer to peer over the existing share server, Tailscale address, port 47231; every request passes `tailscale whois` against `team.allow`; every POST passes `check_request` (no `Origin`, plain Host, JSON content type, `X-Mnemonic-Team` header).
- `TEAM_PROTOCOL_VERSION` goes from 3 to 4. No compatibility with v3.
- Both switches ("Share files", "Teammates can edit files") are per workspace and off by default; edit requires share.
- Nothing is written outside the workspace root, nothing inside `.git`; `..`, absolute paths and paths through a symlink are refused.
- Default skip list, never sent and never writable by a teammate: `.git`, `target`, `node_modules`, `.venv`, `venv`, `__pycache__`, `.next`, `dist`, `build` (any path component), `.env` and `.env.*`, `*.pem`, `*.key`, `.claude/settings.local.json`, symlinks pointing outside the workspace.
- Held for approval when touched by a batch: anything under `.mnemonic/`, `.claude/`, `.zed/`, `.vscode/`, and `.envrc`.
- File scanning, hashing and git commands run off the GPUI foreground thread.
- No em dashes anywhere. Rust rules from `CLAUDE.md`: no `unwrap()`, no `let _ =` on fallible calls, full-word names, no `mod.rs`.
- Lint with `./script/clippy`.

## Review Focus

1. **A file the sharer has open with unsaved edits** is hit by a batch: it must be held as a conflict, not written under the open buffer. Test in Task 7.
2. **A teammate's batch path that escapes the root**, through `..`, an absolute path, or an in-tree symlink to a directory outside: refused with 400 and nothing written. Test in Task 2 (`safe_relative_path`) and Task 7 (apply).
3. **The sharer commits while the teammate has unsent edits**: the copy must not move HEAD or overwrite his edits; it shows "waiting". Test in Task 6.
4. **The teammate's clone does not have the sharer's HEAD** (commits not pushed): the bundle route supplies them; a missing `origin` on either side falls back to a full bundle. Test in Task 5.
5. **Undo after the sharer's agent changed one of the batch's files again**: that file is left alone and listed; the others revert. Test in Task 8.

---

### Task 1: Remove messaging the sharer's agent

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_share_server.rs` (POST route list, `parse_action`, `MAX_MESSAGE_BYTES` use, module doc, tests at ~628 and ~695)
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs` (`TeamActionKind::Message`, `TeamClient::send_message`, `TeamActionOutcome::queued`)
- Modify: `crates/mnemonic-panels/src/team.rs` (`run_action` Message arm, `wake_chat`, `asleep_chats`, `teammate_message_text`, test `a_teammates_message_carries_their_name`)
- Modify: `crates/mnemonic-panels/src/workspace_view.rs` (`queue_teammate_message`, `PendingMessages::push_from_teammate`, `from_teammate`, `MAX_WAITING_TEAMMATE_MESSAGES`, test `at_most_two_teammate_messages_wait_per_chat`)
- Modify: `crates/mnemonic-panels/src/team_workspace_view.rs` (`message_sender` and its `set_send_handler` call)
- Modify: `docs/superpowers/specs/2026-11-14-team-step-in-design.md` (one line under Goals: messaging was removed in step 3)

**Interfaces:**
- Produces: `TeamActionKind` has only `Stop` and `Permission { tool_call_id, option_id }`. `POST /v1/chats/{id}/message` returns 404.

- [ ] **Step 1: Change the server test** `an_allowed_teammate_can_stop_message_and_answer_when_step_in_is_on` to `an_allowed_teammate_can_stop_and_answer_when_step_in_is_on` (drop the message part) and add `a_message_to_the_owners_agent_is_not_found`: a raw POST to `/v1/chats/chat-1/message` with valid team headers and step-in on returns status 404. Delete `an_empty_or_oversized_message_is_refused`.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core team_share_server` and expect the new test to FAIL (it gets 200).
- [ ] **Step 3: Remove the message path end to end** as listed under Files. `ConversationView::set_send_handler` and `restore_unsent_message` stay (other callers may use them); if `cargo build` shows they become unused, remove them too.
- [ ] **Step 4: Run** `cargo test -p mnemonic-core team_share && cargo test -p mnemonic-panels team` and expect PASS; `./script/clippy` clean.
- [ ] **Step 5: Commit** "Stop teammates messaging the sharer's agent".

### Task 2: File selection, fingerprints and path safety (core)

**Files:**
- Create: `crates/mnemonic-core/src/services/team_files_service.rs` (register in `services/mod.rs`)
- Modify: `crates/mnemonic-core/Cargo.toml` (`sha2` if not already a dependency; `tempfile` under dev-dependencies if missing)

**Interfaces:**
- Produces:
  ```rust
  pub struct TeamFileEntry { pub path: String, pub kind: TeamFileKind, pub size: u64, pub hash: String }
  pub enum TeamFileKind { File { executable: bool }, Symlink { target: String }, Deleted }
  pub struct TeamFileManifest { pub head_commit: String, pub entries: Vec<TeamFileEntry> }
  pub struct SkipList { pub patterns: Vec<String> }      // Default = the Global Constraints list
  impl SkipList { pub fn skips(&self, path: &str) -> bool }
  pub fn scan_workspace(root: &Path, skip: &SkipList) -> Result<TeamFileManifest, String>
  pub fn safe_relative_path(root: &Path, path: &str) -> Result<PathBuf, String>
  pub fn needs_approval(path: &str) -> bool
  pub fn file_hash(bytes: &[u8]) -> String               // lowercase hex sha256
  ```
  All serde-serializable (`#[serde(rename_all = "snake_case")]`, `kind` tagged).

- [ ] **Step 1: Write tests** in the same file against a `tempfile` git repo with one commit:
  - `scan_lists_only_what_differs_from_head`: a modified tracked file, an untracked file, an ignored `.context/notes.md`, a deleted tracked file appear (the last as `Deleted`); an unchanged tracked file does not; `head_commit` equals `git rev-parse HEAD`.
  - `scan_skips_build_dirs_and_secrets`: ignored `target/x`, `node_modules/y`, `.env`, `.env.local`, `key.pem`, `.claude/settings.local.json` are absent.
  - `scan_keeps_in_tree_symlinks_and_drops_outside_ones`: `CLAUDE.md -> .rules` is `Symlink { target: ".rules" }`; a link to `/tmp` is absent.
  - `safe_relative_path_refuses_escapes`: `../x`, `/etc/passwd`, `.git/config`, `a/../../x`, and a path through an in-tree symlink to a directory all return `Err`; `.context/plan.md` returns `root.join(".context/plan.md")`.
  - `needs_approval_matches_command_running_files`: true for `.mnemonic/config.json`, `.claude/settings.json`, `.zed/settings.json`, `.vscode/tasks.json`, `.envrc`; false for `.context/plan.md`.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core team_files_service` and expect FAIL (module missing).
- [ ] **Step 3: Implement.** `scan_workspace` runs `git status --porcelain=v2 -z --untracked-files=all --ignored=traditional`, walks collapsed ignored directories itself (pruning skipped components), and hashes each listed file. `safe_relative_path` rejects any component that is `..`, root, prefix or `.git`, and any existing ancestor that is a symlink.
- [ ] **Step 4: Run** the tests and expect PASS.
- [ ] **Step 5: Commit** "Scan a shared workspace's files for teammates".

### Task 3: Wire format, hub and server routes

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_share_service.rs`
- Modify: `crates/mnemonic-core/src/services/team_share_server.rs`

**Interfaces:**
- Consumes: Task 2 types.
- Produces:
  - `TEAM_PROTOCOL_VERSION = 4`.
  - `TeamWorkspace` gains `share_files: bool`, `edit_files: bool`, `remote_url: Option<String>`, `base_branch: Option<String>`.
  - Hub: `set_file_manifest(workspace_id, TeamFileManifest)`, `wait_file_manifest(workspace_id, cursor: Option<&str>, wait) -> Option<TeamFileUpdate>`, where `TeamFileUpdate { cursor: String, manifest: TeamFileManifest }` (whole manifest each time; cursor follows the chat cursor scheme with the hub's epoch).
  - Server config gains `files: FileSource` = `Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>` (workspace id to root, `None` unless "Share files" is on), `bundle_source: BundleSource` = `Arc<dyn Fn(&str, &[String]) -> Result<Vec<u8>, String> + Send + Sync>`, and the action handler gains a batch variant (Task 7 fills it).
  - Routes: `GET /v1/workspaces/{id}/files?cursor&wait`, `GET /v1/workspaces/{id}/file?path=` (raw bytes, streamed, path checked with `safe_relative_path` and the skip list, 404 if not in the current manifest and not tracked), `GET /v1/workspaces/{id}/bundle?have=<commit>,<commit>`, `POST /v1/workspaces/{id}/changes` (body limit 512 MB).
  - Client: `files(workspace_id, cursor, wait)`, `file(workspace_id, path) -> Result<Vec<u8>, _>`, `bundle(workspace_id, have: &[String]) -> Result<Vec<u8>, _>`, `send_changes(workspace_id, &TeamChangeBatch) -> Result<TeamBatchOutcome, _>` (types defined in Task 7; declare them here as plain serde structs so the client compiles).
- [ ] **Step 1: Write server tests**: `file_routes_are_refused_when_share_files_is_off` (403 for files, file, bundle); `a_teammate_reads_the_manifest_and_a_file` (manifest round-trips; file bytes match); `a_file_outside_the_manifest_or_on_the_skip_list_is_not_served` (`.env` and `../x` give 404/400); `changes_are_refused_when_edit_files_is_off` (403).
- [ ] **Step 2: Run** `cargo test -p mnemonic-core team_share_server` and expect FAIL.
- [ ] **Step 3: Implement** the routes and client calls. The `/v1/hello` mismatch message is unchanged.
- [ ] **Step 4: Run** and expect PASS.
- [ ] **Step 5: Commit** "Serve a shared workspace's files to teammates".

### Task 4: Sharer switches, skip list and publishing the manifest

**Files:**
- Modify: `crates/mnemonic-panels/src/team.rs` (`TeamShare`, `publish`, `start_server`, `start_publishing`)
- Modify: wherever `set_workspace_step_in` is offered in the UI (same menu/row; find with `rg set_workspace_step_in crates/mnemonic-panels`)

**Interfaces:**
- Produces: `pub(crate) fn workspace_shares_files(id, cx) -> bool`, `set_workspace_share_files(id, bool, cx)`, `workspace_allows_file_edits(id, cx) -> bool`, `set_workspace_file_edits(id, bool, cx)`, `workspace_skip_list(id, cx) -> SkipList`, `set_workspace_skip_list(id, SkipList, cx)`. Persisted with `save_workspace_ids` under keys `team_share_files_workspaces`, `team_edit_files_workspaces`; skip lists as JSON under `team_skip_lists`. Turning off share also turns off edit; unsharing a workspace clears both.
- [ ] **Step 1: Write test** `turning_off_share_files_turns_off_edits` on `TeamShare` state.
- [ ] **Step 2: Run** `cargo test -p mnemonic-panels team` and expect FAIL.
- [ ] **Step 3: Implement** the switches, a "Skip these files" editor (one pattern per line, defaults prefilled), and publishing: while "Share files" is on and a viewer polled within the existing `has_viewers_within` window, rescan with `scan_workspace` on the background executor every 2 s and call `hub.set_file_manifest` only when it changed. `publish` fills `share_files`, `edit_files`, `remote_url` (`git remote get-url origin`, normalized: strip `.git`, `git@host:` to `host/`), `base_branch`.
- [ ] **Step 4: Run** and expect PASS; check by hand that toggling shows in a second Mnemonic's `/v1/workspaces`.
- [ ] **Step 5: Commit** "Let a sharer share a workspace's files".

### Task 5: Commits the teammate's clone lacks (bundle)

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_files_service.rs`

**Interfaces:**
- Produces: `pub fn bundle_for(root: &Path, have: &[String]) -> Result<Vec<u8>, String>` (`git bundle create - HEAD --not <have...> --remotes=origin`; when that yields no refs, an empty Vec), `pub fn fetch_bundle(repo: &Path, bundle: &[u8], head_commit: &str) -> Result<(), String>` (writes to a temp file, `git fetch <file> HEAD`, then checks `git cat-file -e <head_commit>`).
- [ ] **Step 1: Write test** `an_unpushed_commit_reaches_the_teammate`: repo A with origin B; A has one commit not in B; clone C of B lacks it; `fetch_bundle(C, bundle_for(A, &[]), head)` makes `head` present in C. Second case: A has no `origin` remote, bundle still contains HEAD.
- [ ] **Step 2: Run** and expect FAIL.
- [ ] **Step 3: Implement**; wire `bundle_source` in `start_server` to it.
- [ ] **Step 4: Run** and expect PASS.
- [ ] **Step 5: Commit** "Send a teammate the commits their clone lacks".

### Task 6: The teammate's copy and following it

**Files:**
- Create: `crates/mnemonic-panels/src/team_copy.rs` (viewer side copy lifecycle)
- Modify: `crates/mnemonic-core/src/services/team_files_service.rs` (pure follow logic)
- Modify: `crates/mnemonic-panels/src/team.rs` (start/stop following per copy)

**Interfaces:**
- Consumes: Task 3 client, Task 5 `fetch_bundle`, `workspace_service::create_workspace` with `CreateWorkspaceInput { repository_id, branch: Some("team/<login>/<branch>"), base_branch: <head_commit>, .. }`.
- Produces:
  - `pub struct CopyState { pub host: String, pub workspace_id: String, pub local_workspace_id: String, pub head_commit: String, pub synced: HashMap<String, String> /* path -> hash last written from the sharer */ }`, stored as JSON in `paths::data_dir().join("team-copies").join(<local_workspace_id>.json)`.
  - `pub fn plan_follow(state: &CopyState, local: &HashMap<String, String>, incoming: &TeamFileManifest, has_local_commits: bool) -> FollowPlan` with `FollowPlan { pub write: Vec<String>, pub delete: Vec<String>, pub both_changed: Vec<String>, pub move_head: bool, pub waiting_reason: Option<String> }`. A path is "touched by the teammate" when its local hash differs from `synced` (or from HEAD's blob when not in `synced`).
  - `pub fn local_edits(state: &CopyState, root: &Path, skip: &SkipList) -> Result<Vec<String>, String>`.
  - `pub(crate) fn open_teammate_copy(host: &str, workspace: &TeamWorkspace, window, cx) -> Task<Result<String /* local workspace id */>>`: finds a registered repository whose normalized origin equals `remote_url` (else errors "Add <remote_url> to Mnemonic to work on it"), fetches the bundle if HEAD is missing, creates the worktree on branch `team/<login>/<branch>` at `head_commit`, writes every manifest file, saves `CopyState`, opens it. The copy's workspace is marked (`workspace_source = "team_copy"`) so Setup does not auto-run and the sidebar shows "<Name>'s copy".
- [ ] **Step 1: Write tests** for `plan_follow`: `an_untouched_file_follows_the_sharer`; `a_touched_file_is_kept_and_marked_both_changed`; `a_new_head_waits_while_there_are_unsent_edits` (`move_head == false`, `waiting_reason` set); `a_new_head_waits_while_the_teammate_has_local_commits`.
- [ ] **Step 2: Run** `cargo test -p mnemonic-core team_files_service` and expect FAIL.
- [ ] **Step 3: Implement** `plan_follow`, `local_edits`, then `team_copy.rs`: a long-poll loop on `files` per open copy applies the plan (`git reset --keep <head>` on the copy branch when `move_head`), writes files atomically (temp file plus rename), updates `CopyState`, and shows `waiting_reason` in the copy's header. Stops and offers to delete the copy (warning about unsent edits) when the sharer turns off "Share files".
- [ ] **Step 4: Run** and expect PASS; by hand, open a copy of a real shared workspace and see `.context/` arrive.
- [ ] **Step 5: Commit** "Give a teammate a git copy of a shared workspace".

### Task 7: Sending a batch and applying it on the sharer's side

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_files_service.rs`
- Modify: `crates/mnemonic-panels/src/team.rs` (action handler batch arm, notice)
- Modify: `crates/mnemonic-panels/src/team_copy.rs` (Send and Discard)

**Interfaces:**
- Produces:
  ```rust
  pub struct TeamFileChange { pub path: String, pub base_hash: Option<String> /* None = new file */, pub content: Option<Vec<u8>> /* None = delete; base64 on the wire */ }
  pub struct TeamChangeBatch { pub note: Option<String>, pub changes: Vec<TeamFileChange> }
  pub struct TeamBatchOutcome { pub batch_id: String, pub applied: Vec<String>, pub held: Vec<HeldFile> }
  pub struct HeldFile { pub path: String, pub reason: HeldReason }
  pub enum HeldReason { ChangedSinceYourCopy, NeedsApproval, UnsavedInEditor }
  pub struct BatchRecord { pub id: String, pub login: String, pub name: String, pub at: String, pub note: Option<String>, pub files: Vec<BatchFile> }
  pub struct BatchFile { pub path: String, pub before: Option<Vec<u8>>, pub after: Option<Vec<u8>>, pub state: BatchFileState }
  pub enum BatchFileState { Applied, Held(HeldReason), Kept /* owner kept theirs */, Taken /* owner took teammate's */, Undone }
  pub fn apply_batch(root: &Path, skip: &SkipList, batch: &TeamChangeBatch, unsaved: &HashSet<String>, caller: &TeamCaller, now: &str) -> Result<BatchRecord, String>
  ```
  Records are saved as JSON under `paths::data_dir().join("team-batches").join(<workspace_id>)`, one file per batch.
- [ ] **Step 1: Write tests** for `apply_batch`: `a_matching_base_applies_and_records_before_and_after`; `a_changed_base_is_held_and_untouched`; `a_command_running_file_is_held_for_approval`; `a_file_with_unsaved_edits_is_held` (Review Focus 1); `a_skipped_or_escaping_path_rejects_the_whole_batch` (Review Focus 2, `Err`, nothing written); `a_new_file_that_now_exists_is_held`.
- [ ] **Step 2: Run** and expect FAIL.
- [ ] **Step 3: Implement** `apply_batch` (validate every path first, then write atomically). The panels handler collects paths of dirty buffers in that workspace's project as `unsaved`, calls it on the background executor, saves the record, and shows "<name> (<login>) changed N files in <workspace>" via `show_step_in_notice`, adding "M held for you" when any are held. On the copy: a "Send changes to <Name>" button lists `local_edits` with an optional note; after a send, applied paths update `synced`; held ones stay as local edits and show "held by <Name>". "Discard" on a file rewrites it from the sharer's current version.
- [ ] **Step 4: Run** and expect PASS; by hand, send a batch between two Macs.
- [ ] **Step 5: Commit** "Send a teammate's edits back as one batch".

### Task 8: Batch history, conflicts and undo on the sharer's side

**Files:**
- Modify: `crates/mnemonic-core/src/services/team_files_service.rs`
- Modify: `crates/mnemonic-panels/src/workspace_detail_panel.rs` (a "Teammate changes" section under Changes)

**Interfaces:**
- Produces: `pub fn undo_batch(root: &Path, record: &mut BatchRecord) -> Result<Vec<String> /* left alone */, String>`, `pub fn resolve_held(root: &Path, record: &mut BatchRecord, path: &str, take_teammates: bool) -> Result<(), String>`.
- [ ] **Step 1: Write tests**: `undo_restores_files_that_still_hold_the_batch` and `undo_leaves_a_file_changed_since` (Review Focus 5); `taking_a_held_file_writes_the_teammates_version`; `keeping_a_held_file_changes_nothing`.
- [ ] **Step 2: Run** and expect FAIL.
- [ ] **Step 3: Implement**, then the panel section: one row per batch ("Dan changed 3 files · 2:14pm", note beneath), expanding to its files, each opening a read-only diff of before and after (reuse how `TeammateWorkspaceView::open_file_diff` renders a diff); held files show both versions with "Keep mine" and "Take Dan's"; an "Undo" button per batch reports any files left alone.
- [ ] **Step 4: Run** and expect PASS.
- [ ] **Step 5: Commit** "Show, resolve and undo a teammate's changes".

### Task 9: "Work on this in your copy" in the mirror

**Files:**
- Modify: `crates/mnemonic-panels/src/team_workspace_view.rs` (`build_chat_view` notice, header)
- Modify: `crates/agent_ui/src/conversation_view/thread_view.rs` (`render_message_editor` read-only branch: an optional action button)
- Modify: `crates/agent_ui/src/conversation_view.rs` (`set_read_only` gains `action: Option<(SharedString, Rc<dyn Fn(&mut Window, &mut App)>)>`, replacing `send_handler` if Task 1 left it unused)

**Interfaces:**
- Consumes: Task 6 `open_teammate_copy`.
- [ ] **Step 1: Implement.** When the workspace has `share_files`, the read-only area shows "<Name>'s agent. Read-only." plus a "Work on this in your copy" button that calls `open_teammate_copy` (or opens the existing copy) and starts a new empty chat there with the default agent. Without `share_files` the notice adds "<Name> hasn't shared this workspace's files." Stop and permission buttons are unchanged.
- [ ] **Step 2: Verify by hand** on two Macs: the button opens the copy and a fresh chat runs Dan's agent with the copy as its working directory; `./script/clippy` clean.
- [ ] **Step 3: Commit** "Work on a teammate's workspace in your own copy".

### Task 10: Final checks

- [ ] **Step 1: Run** `cargo test -p mnemonic-core team_ && cargo test -p mnemonic-panels team && ./script/clippy`, expect all PASS.
- [ ] **Step 2: Grep** the diff for em dashes: `git diff origin/main | LC_ALL=C grep -c $'\xe2\x80\x94'` expect `0`.
- [ ] **Step 3: Update** the spec's "Left to the builder" with what was chosen (wire routes, sha256, 2 s rescan, panel placement) and commit "Record the shared files build choices".
