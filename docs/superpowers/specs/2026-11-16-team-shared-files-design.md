# Team sharing step 3: a teammate's workspace files

## Intent

Jay wants a collaborative workspace with teammates over Tailscale, peer to
peer, with no login or SSH access to the other Mac. Step 1 (PR #27) mirrors a
teammate's chats and diff read-only. Step 2 (PR #28) lets a teammate stop,
message and answer the agent there. Neither carries the workspace's files.

The files matter most where git can't reach them: `.context/` holds the
handover, plans, notes and analysis output an agent works from, and in most
repos it is gitignored. Uncommitted work is the same. Step 3 makes a shared
workspace's files available to a teammate (Dan) so he can read them, point
his own agent at them, and edit them. His edits come back to Jay's
workspace marked as his, the way git records who wrote a commit.

## Goals

- Dan gets a local copy of Jay's shared workspace: tracked, untracked and
  gitignored files, including `.context/`. The copy is a git worktree of
  Dan's own clone of the repo, checked out at Jay's branch, with Jay's
  current files laid over it.
- The copy keeps following Jay's workspace while it is shared and Dan is
  connected, so Dan's agent works from Jay's latest files.
- Dan opens the copy as a workspace in his own Mnemonic and runs his own
  agent in it, on his Mac, with his own account. Jay's agent is not involved.
  Dan's chats there are his, not mirrored to Jay. His agent can use git
  (`git diff` against main, `git log`) and his branch, PR and Changes panel
  work as for any workspace.
- Dan can edit files in the copy (by hand or with his agent) and send them
  to Jay as one batch, with an optional short note.
- On Jay's side a batch applies straight away and shows as one entry, such
  as "Dan changed 3 files · 2:14pm", naming Dan's login and display name. Jay
  can open it to see what changed and undo it as a whole.
- A file both sides changed since Dan's copy of it is held back. Jay sees
  both versions and picks one; the rest of the batch applies.
- A teammate's messages always run on the teammate's own agent. Messaging
  Jay's agent from the mirror (step 2) is removed; the mirror's message box
  is replaced by "Work on this in your copy", which opens Dan's copy of the
  workspace and starts a new chat of his own there. Stopping Jay's agent and
  answering its permission requests from the mirror stay as in step 2.
- Two per-workspace switches on Jay's side, both off by default: "Share
  files" (Dan can get the copy) and "Teammates can edit files" (Dan can send
  changes; needs "Share files").

## Non-goals

- Live co-editing or merging inside a file. A file both sides changed is
  held back whole, never merged.
- Git operations on Jay's repo: no commits, branches, pushes or index
  changes. Dan's changes arrive as file edits in the working tree.
- Jay's terminals, Setup and Run.
- Sending Jay's changes back to Dan in the other direction as batches; Dan's
  copy simply follows.
- Compatibility with protocol v3. Both Macs update (protocol v4).

## Anti-goals

- Nothing is written outside the shared workspace's folder on Jay's Mac,
  and nothing inside `.git`. Paths with `..`, absolute paths, and paths
  through a symlink are refused.
- Dan's edits never silently overwrite a change Jay (or Jay's agent) made.
- Files of a workspace are never sent unless "Share files" is on for it.
  Turning on chat sharing alone does not expose files.
- Secrets in the workspace are not sent by default (see Decided).
- Syncing files never slows Jay's Mnemonic or his agent.
- No SSH, no central server.

## Constraints

- Peer to peer over the existing share server (Tailscale address, port
  47231), with the same checks: `tailscale whois` against `team.allow`, and
  the browser-request refusals from step 2 on every POST.
- The server runs on plain threads (`tiny_http`); workspaces and the
  GPUI app live on the foreground thread.
- Live updates use the existing versioned long-poll model, so reconnecting
  never drops or repeats a change.
- Code: `crates/mnemonic-core/src/services/team_share_service.rs` and
  `team_share_server.rs` (wire format, hub, client, server),
  `crates/mnemonic-panels/src/team.rs` (sharing side, switches, persisted as
  `team_step_in_workspaces` is), `team_connection.rs` and
  `team_workspace_view.rs` (viewer side), `workspace_view.rs`,
  `workspace_detail_panel.rs`.

## Approaches

- **A. Extend the share server (picked).** Jay's Mac publishes a list of the
  workspace's files with a fingerprint each; Dan's Mac fetches what changed
  into its own folder and follows over long-poll. Dan's batch names, for
  each file, the fingerprint of Jay's version he started from; a match
  applies, a mismatch is held back. Jay's Mac keeps the record of who
  changed what. History and undo are built by hand; ignored folders and
  large files are easy.
- **B. Carry it in git.** Snapshot the workspace, ignored files included,
  onto a hidden ref; Dan fetches it and returns real commits merged with
  git. Real history and in-file merges, but forces ignored files and large
  data into git objects and adds hidden refs to Jay's repo that a stray push
  could publish.
- **C. A sync tool (Syncthing, Mutagen).** Dropped: last save wins and it
  records nobody's name.

Jay picked A. Jay also picked sending edits back as a batch rather than live
(safer than two agents saving over each other).

## Decided

- Dan's copy has git behind it (Jay's choice). Dan's Mnemonic creates a
  worktree from his own registered clone of the same repo, at the commit
  Jay's workspace is on (sent in the workspace list), then lays Jay's files
  over it. Dan needs the repo cloned and registered in Mnemonic; if he
  doesn't, Mnemonic tells him which repo to add. Dan's commits or pushes in
  his copy are his own business and never reach Jay's repo through this
  feature. When Jay commits and his branch moves, Dan's copy follows the
  new commit as long as Dan has no unsent edits; otherwise it waits and
  says so.
- The agent working in Dan's copy is Dan's own, running on Dan's Mac with
  Dan's account and his own global agent instructions (Jay's choice). The
  project's instruction files come from the copy, so they match Jay's.
- Step 2's teammate message route (`/v1/chats/{id}/message`), its client
  call, the queued-message handling and the "load an unloaded chat to
  receive a message" path are removed, so a teammate can never run Jay's
  agent (Jay's choice). Stop and permission routes stay.
- "Work on this in your copy" starts an empty chat; it does not carry over
  the mirrored chat's history. (Claude's call.)
- Approach A, protocol v4. `/v1/workspaces` carries both new switches per
  workspace so the viewer knows what to offer.
- Conflict: a file Jay's side changed since Dan's base is held back whole;
  Jay sees both versions and keeps one. Same for a file deleted on one side
  and edited on the other, and for a file with unsaved edits in an editor on
  Jay's side. (Claude's call; Jay asked Claude to decide.)
- Dan's copy follows Jay's: a file Dan hasn't touched is replaced when Jay's
  changes; a file Dan has touched is kept and marked "changed on both
  sides" until he sends or discards it. (Claude's call.)
- Sending is a deliberate action ("Send changes to Jay", listing the files),
  not on every save. Dan can also discard his edits to a file, taking
  Jay's current version. (Claude's call.)
- First open fetches everything with progress shown; no overall size limit.
  (Claude's call.)
- What is not sent, by default: `.git`, regenerable build and dependency
  folders (`target`, `node_modules`, `.venv`, `venv`, `__pycache__`, `.next`,
  `dist`, `build`), symlinks pointing outside the workspace, and likely
  secrets (`.env` and
  `.env.*`, `*.pem`, `*.key`). Jay can edit the skip list per workspace.
  (Claude's call; flagged to Jay.)
- Symlinks pointing to another path inside the workspace are recreated on
  Dan's side (this repo's `CLAUDE.md` and `AGENTS.md` link to `.rules`);
  ones pointing outside are skipped. (Claude's call.)
- Files that make something run without a button press (`.mnemonic/`,
  `.claude/`, `.zed/`, `.vscode/`, `.envrc`) are held back in a batch for
  Jay to approve, like a conflict. Dan's Mnemonic does not auto-run Setup or
  Jay's agent hooks in a teammate's copy. Dan cannot write paths on the skip
  list. (Claude's call.)
- Undo of a batch restores each file to what it was before the batch, but
  only where the file still holds what the batch wrote; any file changed
  since is listed and left alone. (Claude's call.)
- The record of batches (who, when, note, files, the before and after
  contents) lives in Mnemonic's own data on Jay's Mac, not in the workspace
  or git. (Claude's call.)
- Dan's copy lives in Mnemonic's own data on Dan's Mac, one folder per
  sharer and workspace, and is deleted when Jay stops sharing files or Dan
  removes it, after warning Dan if he has unsent edits. (Claude's call.)
- Jay is told when a batch arrives, the way step 2 tells him a teammate
  acted. (Claude's call.)

## Left to the builder

What was chosen:

- Wire routes: `GET /v1/workspaces/{id}/files` (long-poll file list),
  `GET /file?path=` (streamed contents), `GET /bundle?have=` (git bundle
  without commits the copy has), `POST /changes` (a batch, 512 MB body limit
  so large `.context/` data fits).
- Fingerprints are sha256, hashed in a stream off the foreground thread; the
  shared state is rescanned every 2 s (`PUBLISH_INTERVAL` in `team.rs`).
- The copy is registered as a workspace with `workspace_source` `team_copy`
  on branch `team/<peer-slug>/<branch>`.
- Batch history and held-back conflicts appear as a "Teammate changes"
  section in the workspace right panel, with Keep mine, Take <Name>'s,
  Approve and Undo.
- Rulings: the secret floor is skipped in any case spelling; batches refuse
  paths spelled differently from disk; `.claude/settings.local.json` is
  withheld while the committed `.claude/settings.json` arrives through git;
  messaging the sharer's agent is removed, while Stop and permission answers
  stay.
