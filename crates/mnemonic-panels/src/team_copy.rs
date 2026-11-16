//! A teammate's copy of a shared workspace: a git worktree of this
//! machine's own clone at the sharer's commit, with the sharer's files laid
//! over it, kept following the sharer while they share its files.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use editor::Editor;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AnyElement, App, AppContext as _, Context, Entity, PromptLevel, Task, WeakEntity};
use mnemonic_core::models::{ChatSessionRecord, CreateWorkspaceInput};
use mnemonic_core::repositories::{
    chat_session_repository, repository_repository, workspace_repository,
};
use mnemonic_core::services::team_files_service::{
    self, CopyState, FollowPlan, SkipList, TEAM_COPY_SOURCE, TeamFileManifest, file_hash,
};
use mnemonic_core::services::team_share_server::MAX_BUNDLE_HAVE;
use mnemonic_core::services::team_share_service::{
    MAX_CHAT_WAIT, TeamClient, TeamClientError, TeamFileUpdate, TeamWorkspace,
};
use mnemonic_core::services::{git_worktree_service, workspace_service};
use mnemonic_core::state::AppState;
use ui::{Button, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::team::{PeerStatus, TeamShare, team_share};
use crate::workspace_panel::WorkspacePanel;
use crate::workspace_view::ChatAgent;

/// How long to wait before asking again after the sharer couldn't be
/// reached, wasn't sharing, or a step failed.
const RETRY_AFTER: Duration = Duration::from_secs(15);
/// Rounds writing fewer files than this don't show progress, unless the
/// copy is being filled for the first time.
const SHOW_PROGRESS_FROM: usize = 20;
/// How many files pass between progress updates.
const PROGRESS_STEP: usize = 10;

pub(crate) enum CopyStatus {
    /// Writing the sharer's files, `done` of `total`; both 0 until the
    /// first list of files arrives.
    Copying {
        done: usize,
        total: usize,
    },
    Following,
    /// The copy stays on the commit it started from, for this reason.
    Waiting(SharedString),
    Unreachable(SharedString),
    /// The sharer turned off sharing this workspace's files.
    SharingStopped,
    Failed(SharedString),
}

/// The only owner of a copy's `CopyState`: the follow loop, sending and
/// discarding all change it here, and saves go through `save_state`.
pub(crate) struct CopyFollow {
    pub state: CopyState,
    pub status: CopyStatus,
    /// Files the teammate changed that the sharer has changed since.
    pub both_changed: Vec<String>,
    /// The sharer's file list as last followed, for discarding an edit.
    last_manifest: Option<TeamFileManifest>,
    /// Files the sharer held back from a send, with the hash sent. Shown as
    /// held until the teammate changes or discards them.
    held: HashMap<String, String>,
    send: Option<SendPanel>,
    save_count: u64,
    /// The newest save written, so a slower older save can't land after it.
    saved: Arc<Mutex<u64>>,
    _follow: Task<()>,
}

/// The open "Send changes" list under the copy's banner.
struct SendPanel {
    note: Entity<Editor>,
    /// Each unsent edit with its hash now; None while they are read.
    edits: Option<BTreeMap<String, String>>,
    busy: bool,
    message: Option<SharedString>,
}

/// Saves the copy's state off the main thread.
fn save_state(copy: &mut CopyFollow, cx: &mut App) {
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    copy.save_count += 1;
    let count = copy.save_count;
    let state = copy.state.clone();
    let saved = copy.saved.clone();
    cx.background_spawn(async move {
        let mut written = saved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *written > count {
            return;
        }
        if let Err(err) = team_files_service::save_copy_state(&app.data_dir, &state) {
            log::error!("Couldn't save the copy {}: {err}", state.local_workspace_id);
        }
        *written = count;
    })
    .detach();
}

/// Starts following every copy saved on this machine.
pub(crate) fn resume_copies(team: &Entity<TeamShare>, cx: &mut App) {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    let team = team.downgrade();
    cx.spawn(async move |cx| {
        let copies = cx
            .background_spawn(async move { team_files_service::load_copy_states(&state.data_dir) })
            .await;
        let copies = match copies {
            Ok(copies) => copies,
            Err(err) => {
                log::error!("Couldn't read teammates' copies: {err}");
                return;
            }
        };
        team.update(cx, |team, cx| {
            for copy in copies {
                start_following(team, copy, CopyStatus::Following, cx);
            }
        })
        .log_err();
    })
    .detach();
}

pub(crate) fn start_following(
    team: &mut TeamShare,
    state: CopyState,
    status: CopyStatus,
    cx: &mut Context<TeamShare>,
) {
    let local_workspace_id = state.local_workspace_id.clone();
    let follow = follow(local_workspace_id.clone(), cx);
    team.copies.insert(
        local_workspace_id,
        CopyFollow {
            state,
            status,
            both_changed: Vec::new(),
            last_manifest: None,
            held: HashMap::new(),
            send: None,
            save_count: 0,
            saved: Arc::new(Mutex::new(0)),
            _follow: follow,
        },
    );
    cx.notify();
}

fn set_status(
    team: &WeakEntity<TeamShare>,
    local_workspace_id: &str,
    status: CopyStatus,
    cx: &mut gpui::AsyncApp,
) {
    team.update(cx, |team, cx| {
        if let Some(copy) = team.copies.get_mut(local_workspace_id) {
            copy.status = status;
            cx.notify();
        }
    })
    .log_err();
}

/// What the sharer's polled workspace list says about this workspace: None
/// while the teammate isn't online, else whether it is listed with its
/// files shared.
fn listed_with_files(team: &TeamShare, state: &CopyState) -> Option<bool> {
    let peer = team.peer(&state.host)?;
    if !matches!(peer.status, PeerStatus::Online) {
        return None;
    }
    Some(
        peer.workspaces
            .iter()
            .any(|workspace| workspace.id == state.workspace_id && workspace.share_files),
    )
}

fn follow(local_workspace_id: String, cx: &mut Context<TeamShare>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut cursor: Option<String> = None;
        let mut last_manifest: Option<TeamFileManifest> = None;
        // A plan that is waiting on the teammate is tried again after each
        // long poll, as sending or discarding edits changes nothing on the
        // sharer's side.
        let mut recheck = false;
        loop {
            let Ok(Some((state, client, listed))) = this.read_with(cx, |team, _| {
                let copy = team.copies.get(&local_workspace_id)?;
                let client = team
                    .peer(&copy.state.host)
                    .and_then(|peer| peer.client.clone());
                Some((
                    copy.state.clone(),
                    client,
                    listed_with_files(team, &copy.state),
                ))
            }) else {
                break;
            };
            let Some(client) = client else {
                set_status(
                    &this,
                    &local_workspace_id,
                    CopyStatus::Unreachable(
                        format!("{} isn't one of your teammates", state.sharer_name).into(),
                    ),
                    cx,
                );
                cx.background_executor().timer(RETRY_AFTER).await;
                continue;
            };
            if listed == Some(false) {
                set_status(&this, &local_workspace_id, CopyStatus::SharingStopped, cx);
                cursor = None;
                cx.background_executor().timer(RETRY_AFTER).await;
                continue;
            }

            let response = cx
                .background_spawn({
                    let client = client.clone();
                    let workspace_id = state.workspace_id.clone();
                    let cursor = cursor.clone();
                    async move { client.files(&workspace_id, cursor.as_deref(), MAX_CHAT_WAIT) }
                })
                .await;
            let manifest = match response {
                Ok(Some(update)) => {
                    cursor = Some(update.cursor);
                    match update.manifest {
                        Some(manifest) => manifest,
                        None => match last_manifest.clone().filter(|_| recheck) {
                            Some(manifest) => manifest,
                            None => continue,
                        },
                    }
                }
                Ok(None) => {
                    set_status(
                        &this,
                        &local_workspace_id,
                        CopyStatus::Unreachable(
                            format!("{} isn't sharing these files right now", state.sharer_name)
                                .into(),
                        ),
                        cx,
                    );
                    cx.background_executor().timer(RETRY_AFTER).await;
                    continue;
                }
                Err(TeamClientError::Forbidden) => {
                    set_status(&this, &local_workspace_id, CopyStatus::SharingStopped, cx);
                    cursor = None;
                    cx.background_executor().timer(RETRY_AFTER).await;
                    continue;
                }
                Err(err) => {
                    set_status(
                        &this,
                        &local_workspace_id,
                        CopyStatus::Unreachable(err.to_string().into()),
                        cx,
                    );
                    cx.background_executor().timer(RETRY_AFTER).await;
                    continue;
                }
            };

            let (progress_sender, mut progress) = mpsc::unbounded::<(usize, usize)>();
            let show_progress = cx.spawn({
                let this = this.clone();
                let local_workspace_id = local_workspace_id.clone();
                async move |cx| {
                    while let Some((done, total)) = progress.next().await {
                        this.update(cx, |team, cx| {
                            let Some(copy) = team.copies.get_mut(&local_workspace_id) else {
                                return;
                            };
                            // Small rounds pass too quickly to be worth a line;
                            // a first copy always shows how far it has got.
                            let copying = matches!(copy.status, CopyStatus::Copying { .. });
                            if copying || total >= SHOW_PROGRESS_FROM {
                                copy.status = CopyStatus::Copying { done, total };
                                cx.notify();
                            }
                        })
                        .log_err();
                    }
                }
            });
            // A send or discard during the long poll changed the live state.
            let Ok(Some(state)) = this.read_with(cx, |team, _| {
                team.copies
                    .get(&local_workspace_id)
                    .map(|copy| copy.state.clone())
            }) else {
                break;
            };
            let started_from = state.clone();
            let outcome = cx
                .background_spawn({
                    let manifest = manifest.clone();
                    async move { apply_manifest(state, manifest, client, progress_sender) }
                })
                .await;
            show_progress.await;
            let Some((state, result)) = outcome else {
                this.update(cx, |team, cx| {
                    team.copies.remove(&local_workspace_id);
                    cx.notify();
                })
                .log_err();
                break;
            };
            let retry = result.is_err();
            recheck = result
                .as_ref()
                .is_ok_and(|plan| plan.waiting_reason.is_some() || !plan.both_changed.is_empty());
            last_manifest = Some(manifest.clone());
            let updated = this.update(cx, |team, cx| {
                let Some(copy) = team.copies.get_mut(&local_workspace_id) else {
                    return false;
                };
                copy.state =
                    team_files_service::merge_follow_round(&started_from, state, &copy.state);
                copy.last_manifest = Some(manifest);
                save_state(copy, cx);
                match result {
                    Ok(plan) => {
                        copy.status = match plan.waiting_reason {
                            Some(reason) => CopyStatus::Waiting(reason.into()),
                            None => CopyStatus::Following,
                        };
                        copy.both_changed = plan.both_changed;
                    }
                    Err(err) => {
                        log::error!("Couldn't follow {}: {err}", copy.state.workspace_id);
                        copy.status = CopyStatus::Failed(err.into());
                    }
                }
                cx.notify();
                true
            });
            if !matches!(updated, Ok(true)) {
                break;
            }
            if retry {
                // The next poll then answers at once with the whole manifest.
                cursor = None;
                cx.background_executor().timer(RETRY_AFTER).await;
            }
        }
    })
}

/// The copy's folder, None when its workspace no longer exists.
fn copy_root(app: &AppState, local_workspace_id: &str) -> Result<Option<PathBuf>, String> {
    Ok(
        workspace_repository::get_detail(&app.db, local_workspace_id)?.map(|detail| {
            PathBuf::from(
                detail
                    .summary
                    .workspace_root_path
                    .unwrap_or(detail.worktree_path),
            )
        }),
    )
}

/// Runs off the main thread: brings the copy up to the manifest. None when
/// the copy's workspace was removed, so following it stops.
fn apply_manifest(
    mut state: CopyState,
    manifest: TeamFileManifest,
    client: TeamClient,
    progress: mpsc::UnboundedSender<(usize, usize)>,
) -> Option<(CopyState, Result<FollowPlan, String>)> {
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return Some((state, Err("Mnemonic isn't ready".to_string())));
    };
    let root = match copy_root(&app, &state.local_workspace_id) {
        Ok(Some(root)) => root,
        Ok(None) => {
            if let Err(err) =
                team_files_service::remove_copy_state(&app.data_dir, &state.local_workspace_id)
            {
                log::error!(
                    "Couldn't forget the copy {}: {err}",
                    state.local_workspace_id
                );
            }
            return None;
        }
        Err(err) => return Some((state, Err(err))),
    };
    let result = ensure_commit(
        &root,
        &client,
        &state.workspace_id,
        state.base_branch.as_deref(),
        &manifest.head_commit,
    )
    .and_then(|()| {
        let workspace_id = state.workspace_id.clone();
        let fetch = |path: &str| {
            client
                .file(&workspace_id, path)
                .map_err(|err| format!("Couldn't fetch {path}: {err}"))
        };
        let on_progress = |done: usize, total: usize| {
            if done == total || done.is_multiple_of(PROGRESS_STEP) {
                // The receiver only goes away when following stops.
                progress.unbounded_send((done, total)).ok();
            }
        };
        team_files_service::follow_manifest(&root, &mut state, &manifest, &fetch, &on_progress)
    });
    Some((state, result))
}

/// Makes sure the sharer's commit is in the repository, from origin when
/// it has been pushed, else from a bundle of what the sharer hasn't pushed.
fn ensure_commit(
    repo: &Path,
    client: &TeamClient,
    workspace_id: &str,
    base_branch: Option<&str>,
    commit: &str,
) -> Result<(), String> {
    if team_files_service::has_commit(repo, commit) {
        return Ok(());
    }
    if let Err(err) = team_files_service::fetch_origin(repo) {
        log::warn!("Couldn't fetch origin in {}: {err}", repo.display());
    }
    if team_files_service::has_commit(repo, commit) {
        return Ok(());
    }
    let have = team_files_service::have_commits(repo, base_branch, MAX_BUNDLE_HAVE);
    let bundle = client
        .bundle(workspace_id, &have)
        .map_err(|err| format!("Couldn't get the sharer's commits: {err}"))?;
    team_files_service::fetch_bundle(repo, &bundle, commit)
}

/// A teammate's name as a branch path part.
fn branch_slug(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect();
    if slug.trim_matches('-').is_empty() {
        "teammate".to_string()
    } else {
        slug
    }
}

struct NewCopy {
    host: String,
    sharer_name: String,
    shared: TeamWorkspace,
    remote_url: String,
    client: TeamClient,
    agent_id: &'static str,
    agent_label: &'static str,
}

/// Runs off the main thread: makes the worktree at the sharer's commit and
/// saves the copy, for the follow loop to fill.
fn create_copy(app: &AppState, copy: NewCopy) -> Result<CopyState, String> {
    let NewCopy {
        host,
        sharer_name,
        shared,
        remote_url,
        client,
        agent_id,
        agent_label,
    } = copy;
    let repository = repository_repository::list(&app.db)?
        .into_iter()
        .find(|repository| {
            team_files_service::origin_url(Path::new(&repository.path)).as_deref()
                == Some(remote_url.as_str())
        })
        .ok_or_else(|| format!("Add {remote_url} to Mnemonic to work on it"))?;
    let manifest = match client.files(&shared.id, None, Duration::ZERO) {
        Ok(Some(TeamFileUpdate {
            manifest: Some(manifest),
            ..
        })) => manifest,
        Ok(_) => {
            return Err(format!(
                "{sharer_name} hasn't shared this workspace's files yet"
            ));
        }
        Err(TeamClientError::Forbidden) => {
            return Err(format!(
                "{sharer_name} isn't sharing this workspace's files"
            ));
        }
        Err(err) => return Err(format!("Couldn't reach {sharer_name}: {err}")),
    };
    let repository_path = Path::new(&repository.path);
    ensure_commit(
        repository_path,
        &client,
        &shared.id,
        shared.base_branch.as_deref(),
        &manifest.head_commit,
    )?;
    let branch = team_files_service::unused_branch_name(
        repository_path,
        &format!("team/{}/{}", branch_slug(&sharer_name), shared.branch),
    )?;
    let detail = workspace_service::create_workspace(
        app,
        CreateWorkspaceInput {
            name: format!("{sharer_name}'s {}", shared.name),
            repo: repository.name.clone(),
            base_branch: manifest.head_commit.clone(),
            branch: Some(branch),
            agent: "claude".to_string(),
            task_prompt: String::new(),
            open_in_cursor: false,
            run_tests: false,
            create_pr: false,
            repository_id: Some(repository.id.clone()),
            selected_worktree_id: None,
            selected_branch: None,
            parent_workspace_id: None,
            source_workspace_id: None,
            derived_from_branch: None,
            workspace_source: Some(TEAM_COPY_SOURCE.to_string()),
        },
    )?;
    let state = CopyState {
        host,
        sharer_name,
        workspace_id: shared.id.clone(),
        local_workspace_id: detail.summary.id.clone(),
        base_branch: shared.base_branch.clone(),
        head_commit: manifest.head_commit,
        synced: Default::default(),
    };
    // The follow loop writes the sharer's files, starting from the whole
    // list, and shows its progress above the copy.
    team_files_service::save_copy_state(&app.data_dir, &state)?;
    let record = ChatSessionRecord {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id: detail.summary.id.clone(),
        agent: agent_id.to_string(),
        title: agent_label.to_string(),
        acp_session_id: None,
        is_review: false,
        is_primary: true,
        position: 0,
    };
    if let Err(err) = chat_session_repository::insert(&app.db, &record) {
        log::error!("Failed to save the copy's first chat session: {err}");
    }
    Ok(state)
}

fn select_local_workspace(
    workspace: Option<Entity<Workspace>>,
    local_workspace_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(panel) =
        workspace.and_then(|workspace| workspace.read(cx).panel::<WorkspacePanel>(cx))
    else {
        return;
    };
    panel.update(cx, |panel, cx| {
        panel.refresh(cx);
        panel.select_workspace(local_workspace_id, window, cx);
    });
}

/// The local workspace holding this machine's copy of a teammate's workspace.
/// A copy removed from the sidebar stays listed until its follow loop next
/// notices, so it is checked against the workspaces that exist.
pub(crate) fn existing_copy(
    team: &Entity<TeamShare>,
    app: &AppState,
    host: &str,
    workspace_id: &str,
    cx: &App,
) -> Option<String> {
    team.read(cx)
        .copies
        .values()
        .find(|copy| copy.state.host == host && copy.state.workspace_id == workspace_id)
        .map(|copy| copy.state.local_workspace_id.clone())
        .filter(|local_workspace_id| {
            matches!(
                workspace_repository::get_detail(&app.db, local_workspace_id),
                Ok(Some(_))
            )
        })
}

/// Opens this machine's copy of a teammate's shared workspace, making it
/// first when there isn't one yet. A new copy opens as soon as its worktree
/// exists; its banner shows the sharer's files arriving.
pub(crate) fn open_teammate_copy(
    host: &str,
    shared: &TeamWorkspace,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<String>> {
    let Some(team) = team_share(cx) else {
        return Task::ready(Err(anyhow!("Team sharing isn't running")));
    };
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return Task::ready(Err(anyhow!("Mnemonic isn't ready")));
    };
    let workspace = Workspace::for_window(window, cx);
    let existing = existing_copy(&team, &app, host, &shared.id, cx);
    if let Some(local_workspace_id) = existing {
        select_local_workspace(workspace, local_workspace_id.clone(), window, cx);
        return Task::ready(Ok(local_workspace_id));
    }
    let Some(peer) = team.read(cx).peer(host) else {
        return Task::ready(Err(anyhow!("{host} isn't one of your teammates")));
    };
    let sharer_name = peer.peer.name.clone();
    let Some(client) = peer.client.clone() else {
        return Task::ready(Err(anyhow!("Can't reach {sharer_name}")));
    };
    if !shared.share_files {
        return Task::ready(Err(anyhow!(
            "{sharer_name} hasn't shared this workspace's files"
        )));
    }
    let Some(remote_url) = shared.remote_url.clone() else {
        return Task::ready(Err(anyhow!(
            "{sharer_name}'s workspace has no origin remote, so it can't be copied"
        )));
    };
    let agent = ChatAgent::default_agent(cx);
    let copy = NewCopy {
        host: host.to_string(),
        sharer_name,
        shared: shared.clone(),
        remote_url,
        client,
        agent_id: agent.agent_id(),
        agent_label: agent.label(),
    };
    window.spawn(cx, async move |cx| {
        let state = cx
            .background_spawn(async move { create_copy(&app, copy) })
            .await
            .map_err(|err| anyhow!(err))?;
        let local_workspace_id = state.local_workspace_id.clone();
        team.update(cx, |team, cx| {
            start_following(team, state, CopyStatus::Copying { done: 0, total: 0 }, cx)
        });
        cx.update(|window, cx| {
            select_local_workspace(workspace, local_workspace_id.clone(), window, cx);
        })?;
        Ok(local_workspace_id)
    })
}

/// Deletes the copy after asking, warning about edits that weren't sent.
fn delete_copy(local_workspace_id: String, window: &mut Window, cx: &mut App) {
    let Some(team) = team_share(cx) else {
        return;
    };
    let Some(state) = team
        .read(cx)
        .copies
        .get(&local_workspace_id)
        .map(|copy| copy.state.clone())
    else {
        return;
    };
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    let workspace = Workspace::for_window(window, cx);
    window
        .spawn(cx, async move |cx| {
            let detail = cx
                .background_spawn({
                    let app = app.clone();
                    let state = state.clone();
                    async move {
                        let detail =
                            workspace_repository::get_detail(&app.db, &state.local_workspace_id)?
                                .ok_or_else(|| "The copy no longer exists".to_string())?;
                        let root = detail
                            .summary
                            .workspace_root_path
                            .clone()
                            .unwrap_or_else(|| detail.worktree_path.clone());
                        let edits = team_files_service::local_edits(
                            &state,
                            Path::new(&root),
                            &SkipList::default(),
                        )?;
                        Ok::<_, String>((detail, root, edits))
                    }
                })
                .await;
            let (detail, root, edits) = detail.map_err(|err| anyhow!(err))?;
            let message = match edits.len() {
                0 => "This removes the copy's folder from this Mac.".to_string(),
                1 => "You have 1 edit you haven't sent. It will be lost.".to_string(),
                count => format!("You have {count} edits you haven't sent. They will be lost."),
            };
            let answer = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    &format!("Delete your copy of {}'s workspace?", state.sharer_name),
                    Some(&message),
                    &["Delete", "Cancel"],
                    cx,
                )
            })?;
            if answer.await? != 0 {
                return Ok(());
            }
            cx.background_spawn({
                let local_workspace_id = local_workspace_id.clone();
                async move {
                    workspace_service::delete_workspace(&app, &local_workspace_id)?;
                    if let Some(repository_path) = detail.summary.repository_path.as_deref() {
                        git_worktree_service::remove_mnemonic_worktree(repository_path, &root)?;
                    }
                    if let Err(err) =
                        chat_session_repository::delete_for_workspace(&app.db, &local_workspace_id)
                    {
                        log::error!(
                            "Failed to delete chat sessions for {local_workspace_id}: {err}"
                        );
                    }
                    team_files_service::remove_copy_state(&app.data_dir, &local_workspace_id)
                }
            })
            .await
            .map_err(|err| anyhow!(err))?;
            team.update(cx, |team, cx| {
                team.copies.remove(&local_workspace_id);
                cx.notify();
            });
            cx.update(|_, cx| {
                let panel =
                    workspace.and_then(|workspace| workspace.read(cx).panel::<WorkspacePanel>(cx));
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| {
                        panel.forget_workspace(&local_workspace_id, cx)
                    });
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

/// Why the teammate can't send changes to the sharer now, None when they
/// can.
fn why_cant_send(team: &TeamShare, state: &CopyState) -> Option<String> {
    let name = &state.sharer_name;
    let Some(peer) = team.peer(&state.host) else {
        return Some(format!("{name} isn't one of your teammates"));
    };
    if !matches!(peer.status, PeerStatus::Online) || peer.client.is_none() {
        return Some(format!("{name} isn't connected"));
    }
    match peer
        .workspaces
        .iter()
        .find(|workspace| workspace.id == state.workspace_id)
    {
        Some(workspace) if workspace.share_files && workspace.edit_files => None,
        Some(workspace) if workspace.share_files => Some(format!(
            "{name} hasn't turned on \"Teammates can edit files\""
        )),
        _ => Some(format!("{name} isn't sharing these files")),
    }
}

fn update_copy(
    team: &Entity<TeamShare>,
    local_workspace_id: &str,
    cx: &mut App,
    update: impl FnOnce(&mut CopyFollow, &mut Context<TeamShare>),
) {
    team.update(cx, |team, cx| {
        if let Some(copy) = team.copies.get_mut(local_workspace_id) {
            update(copy, cx);
            cx.notify();
        }
    });
}

fn set_send_message(
    team: &Entity<TeamShare>,
    local_workspace_id: &str,
    message: String,
    cx: &mut App,
) {
    update_copy(team, local_workspace_id, cx, |copy, _| {
        if let Some(panel) = copy.send.as_mut() {
            panel.busy = false;
            panel.message = Some(message.into());
        }
    });
}

fn open_send_panel(local_workspace_id: String, window: &mut Window, cx: &mut App) {
    let Some(team) = team_share(cx) else {
        return;
    };
    let Some(sharer_name) = team
        .read(cx)
        .copies
        .get(&local_workspace_id)
        .map(|copy| copy.state.sharer_name.clone())
    else {
        return;
    };
    let note = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(&format!("Optional note for {sharer_name}"), window, cx);
        editor
    });
    update_copy(&team, &local_workspace_id, cx, |copy, _| {
        copy.send = Some(SendPanel {
            note,
            edits: None,
            busy: false,
            message: None,
        });
    });
    refresh_edits(&team, local_workspace_id, cx);
}

/// Reads the copy's unsent edits again for the open send list, and stops
/// showing a file as held once it has changed since it was sent.
fn refresh_edits(team: &Entity<TeamShare>, local_workspace_id: String, cx: &mut App) {
    let Some(state) = team
        .read(cx)
        .copies
        .get(&local_workspace_id)
        .map(|copy| copy.state.clone())
    else {
        return;
    };
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    let team = team.downgrade();
    cx.spawn(async move |cx| {
        let edits = cx
            .background_spawn(async move {
                let root = copy_root(&app, &state.local_workspace_id)?
                    .ok_or_else(|| "The copy no longer exists".to_string())?;
                team_files_service::local_edit_hashes(&state, &root, &SkipList::default())
            })
            .await;
        team.update(cx, |team, cx| {
            let Some(copy) = team.copies.get_mut(&local_workspace_id) else {
                return;
            };
            let Some(panel) = copy.send.as_mut() else {
                return;
            };
            match edits {
                Ok(edits) => {
                    copy.held
                        .retain(|path, sent_hash| edits.get(path) == Some(sent_hash));
                    panel.edits = Some(edits);
                }
                Err(err) => {
                    panel.edits = Some(BTreeMap::new());
                    panel.message = Some(format!("Couldn't read your changes: {err}").into());
                }
            }
            cx.notify();
        })
        .log_err();
    })
    .detach();
}

/// The unsent edits worth sending: all but files the sharer held that
/// haven't changed since they were sent, as sending them again would only
/// be held again.
fn sendable_paths(copy: &CopyFollow, edits: &BTreeMap<String, String>) -> Vec<String> {
    edits
        .iter()
        .filter(|(path, hash)| copy.held.get(*path) != Some(*hash))
        .map(|(path, _)| path.clone())
        .collect()
}

fn sent_message(sharer_name: &str, applied: usize, held: usize, left_out: &[String]) -> String {
    let mut message = format!("Sent to {sharer_name}: {applied} applied");
    if held > 0 {
        message.push_str(&format!(", {held} held by {sharer_name}"));
    }
    if !left_out.is_empty() {
        message.push_str(&format!(". Not sent: {}", left_out.join("; ")));
    }
    message
}

/// Sends every unsent edit in the list to the sharer as one batch.
fn send_changes(local_workspace_id: String, window: &mut Window, cx: &mut App) {
    let Some(team) = team_share(cx) else {
        return;
    };
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    let Some((state, client, paths, note_editor, reason)) = ({
        let team = team.read(cx);
        team.copies.get(&local_workspace_id).and_then(|copy| {
            let panel = copy.send.as_ref().filter(|panel| !panel.busy)?;
            let paths = sendable_paths(copy, panel.edits.as_ref()?);
            let client = team
                .peer(&copy.state.host)
                .and_then(|peer| peer.client.clone());
            Some((
                copy.state.clone(),
                client,
                paths,
                panel.note.clone(),
                why_cant_send(team, &copy.state),
            ))
        })
    }) else {
        return;
    };
    let client = match (client, reason) {
        (Some(client), None) => client,
        (_, reason) => {
            let reason = reason.unwrap_or_else(|| format!("{} isn't connected", state.sharer_name));
            set_send_message(&team, &local_workspace_id, reason, cx);
            return;
        }
    };
    let note = note_editor.read(cx).text(cx).trim().to_string();
    let note = (!note.is_empty()).then_some(note);
    update_copy(&team, &local_workspace_id, cx, |copy, _| {
        if let Some(panel) = copy.send.as_mut() {
            panel.busy = true;
            panel.message = Some("Sending...".into());
        }
    });
    window
        .spawn(cx, async move |cx| {
            let sent = cx
                .background_spawn({
                    let state = state.clone();
                    async move {
                        let root = copy_root(&app, &state.local_workspace_id)?
                            .ok_or_else(|| "The copy no longer exists".to_string())?;
                        let outgoing =
                            team_files_service::build_batch(&state, &root, &paths, note)?;
                        if outgoing.batch.changes.is_empty() {
                            return Err("There is nothing to send".to_string());
                        }
                        let outcome = client
                            .send_changes(&state.workspace_id, &outgoing.batch)
                            .map_err(|err| err.to_string())?;
                        Ok((outgoing, outcome))
                    }
                })
                .await;
            let sent_well = sent.is_ok();
            team.update(cx, |team, cx| {
                let Some(copy) = team.copies.get_mut(&local_workspace_id) else {
                    return;
                };
                let message = match sent {
                    Ok((outgoing, outcome)) => {
                        team_files_service::record_sent(&mut copy.state, &outgoing.batch, &outcome);
                        for held in &outcome.held {
                            if let Some(change) = outgoing
                                .batch
                                .changes
                                .iter()
                                .find(|change| change.path == held.path)
                            {
                                let sent_hash =
                                    change.content.as_deref().map(file_hash).unwrap_or_default();
                                copy.held.insert(held.path.clone(), sent_hash);
                            }
                        }
                        save_state(copy, cx);
                        sent_message(
                            &copy.state.sharer_name,
                            outcome.applied.len(),
                            outcome.held.len(),
                            &outgoing.left_out,
                        )
                    }
                    Err(err) => format!("Couldn't send your changes: {err}"),
                };
                if let Some(panel) = copy.send.as_mut() {
                    panel.busy = false;
                    panel.message = Some(message.into());
                }
                cx.notify();
            });
            if sent_well {
                cx.update(|window, cx| {
                    note_editor.update(cx, |editor, cx| editor.clear(window, cx));
                    refresh_edits(&team, local_workspace_id, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

/// Replaces the teammate's edit to `path` with the sharer's current
/// version, after asking.
fn discard_change(local_workspace_id: String, path: String, window: &mut Window, cx: &mut App) {
    let Some(team) = team_share(cx) else {
        return;
    };
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    let Some((state, manifest, client)) = ({
        let team = team.read(cx);
        team.copies.get(&local_workspace_id).map(|copy| {
            let client = team
                .peer(&copy.state.host)
                .and_then(|peer| peer.client.clone());
            (copy.state.clone(), copy.last_manifest.clone(), client)
        })
    }) else {
        return;
    };
    let sharer_name = state.sharer_name.clone();
    let Some(manifest) = manifest else {
        set_send_message(
            &team,
            &local_workspace_id,
            format!("Wait until this copy has caught up with {sharer_name}"),
            cx,
        );
        return;
    };
    let entry = manifest
        .entries
        .into_iter()
        .find(|entry| entry.path == path);
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Discard your changes to {path}?"),
        Some(&format!("This takes {sharer_name}'s current version.")),
        &["Discard", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await? != 0 {
                return Ok(());
            }
            let discarded = cx
                .background_spawn({
                    let path = path.clone();
                    async move {
                        let root = copy_root(&app, &state.local_workspace_id)?
                            .ok_or_else(|| "The copy no longer exists".to_string())?;
                        let fetch = |wanted: &str| match &client {
                            Some(client) => client
                                .file(&state.workspace_id, wanted)
                                .map_err(|err| format!("Couldn't fetch {wanted}: {err}")),
                            None => Err(format!("Can't reach {}", state.sharer_name)),
                        };
                        team_files_service::discard_edit(&root, &path, entry.as_ref(), &fetch)
                    }
                })
                .await;
            team.update(cx, |team, cx| {
                let Some(copy) = team.copies.get_mut(&local_workspace_id) else {
                    return;
                };
                match discarded {
                    Ok(synced) => {
                        match synced {
                            Some(hash) => {
                                copy.state.synced.insert(path.clone(), hash);
                            }
                            None => {
                                copy.state.synced.remove(&path);
                            }
                        }
                        copy.held.remove(&path);
                        save_state(copy, cx);
                    }
                    Err(err) => {
                        if let Some(panel) = copy.send.as_mut() {
                            panel.message = Some(format!("Couldn't discard {path}: {err}").into());
                        }
                    }
                }
                cx.notify();
            });
            cx.update(|_, cx| refresh_edits(&team, local_workspace_id, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

fn render_send_panel(
    local_workspace_id: &str,
    copy: &CopyFollow,
    panel: &SendPanel,
    cant_send: Option<String>,
    cx: &App,
) -> AnyElement {
    let name = &copy.state.sharer_name;
    let count = panel
        .edits
        .as_ref()
        .map_or(0, |edits| sendable_paths(copy, edits).len());
    let list = match &panel.edits {
        None => Label::new("Looking for your changes...")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        Some(edits) if edits.is_empty() => Label::new("You have no changes to send.")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        Some(edits) => v_flex()
            .id("team-copy-edits")
            .max_h(px(200.))
            .overflow_y_scroll()
            .children(edits.iter().map(|(path, hash)| {
                let held = copy.held.get(path) == Some(hash);
                let local_workspace_id = local_workspace_id.to_string();
                let discard_path = path.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(path.clone()).size(LabelSize::Small))
                    .when(held, |row| {
                        row.child(
                            Label::new(format!("held by {name}"))
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("discard-{path}")), "Discard")
                            .disabled(panel.busy)
                            .on_click(move |_, window, cx| {
                                discard_change(
                                    local_workspace_id.clone(),
                                    discard_path.clone(),
                                    window,
                                    cx,
                                )
                            }),
                    )
            }))
            .into_any_element(),
    };
    let send_label = if count == 1 {
        "Send 1 file".to_string()
    } else {
        format!("Send {count} files")
    };
    let send_id = local_workspace_id.to_string();
    let close_id = local_workspace_id.to_string();
    v_flex()
        .w_full()
        .px_2()
        .pb_1()
        .gap_1()
        .child(list)
        .child(
            div()
                .px_1()
                .py_0p5()
                .border_1()
                .rounded_sm()
                .border_color(cx.theme().colors().border_variant)
                .child(panel.note.clone()),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("send-team-copy-changes", send_label)
                        .disabled(panel.busy || count == 0 || cant_send.is_some())
                        .on_click(move |_, window, cx| send_changes(send_id.clone(), window, cx)),
                )
                .child(
                    Button::new("close-team-copy-send", "Close").on_click(move |_, _, cx| {
                        if let Some(team) = team_share(cx) {
                            update_copy(&team, &close_id, cx, |copy, _| copy.send = None);
                        }
                    }),
                )
                .when_some(cant_send, |row, reason| {
                    row.child(
                        Label::new(reason)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .when_some(panel.message.clone(), |row, message| {
                    row.child(
                        Label::new(message)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                }),
        )
        .into_any_element()
}

/// The lines above a teammate's copy: how following the sharer is going,
/// and sending changes back. None for any other workspace.
pub(crate) fn render_copy_banner(local_workspace_id: &str, cx: &App) -> Option<AnyElement> {
    let team = team_share(cx)?;
    let team = team.read(cx);
    let copy = team.copies.get(local_workspace_id)?;
    let name = &copy.state.sharer_name;
    let (message, can_delete): (SharedString, bool) = match &copy.status {
        CopyStatus::Copying { total: 0, .. } => {
            (format!("Copying {name}'s files...").into(), false)
        }
        CopyStatus::Copying { done, total } => (
            format!("Copying {name}'s files: {done} of {total}").into(),
            false,
        ),
        CopyStatus::SharingStopped => (
            format!("{name} stopped sharing these files, so this copy no longer follows them.")
                .into(),
            true,
        ),
        CopyStatus::Waiting(reason) => (reason.clone(), false),
        CopyStatus::Unreachable(reason) => {
            (format!("Not following {name}: {reason}").into(), false)
        }
        CopyStatus::Failed(error) => (
            format!("Couldn't follow {name}'s files: {error}").into(),
            false,
        ),
        CopyStatus::Following if copy.both_changed.is_empty() => {
            (format!("Following {name}'s files.").into(), false)
        }
        CopyStatus::Following => {
            let count = copy.both_changed.len();
            let files = copy
                .both_changed
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let more = if count > 3 { ", ..." } else { "" };
            (
                format!(
                    "Changed here and by {name}: {files}{more}. Yours are kept until you send or discard them."
                )
                .into(),
                false,
            )
        }
    };
    let can_send = matches!(copy.status, CopyStatus::Following | CopyStatus::Waiting(_));
    let cant_send = why_cant_send(team, &copy.state);
    let delete_id = local_workspace_id.to_string();
    let send_id = local_workspace_id.to_string();
    let line = h_flex()
        .w_full()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(
            div().flex_grow(1.0).overflow_hidden().child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
        )
        .when(can_send && copy.send.is_none(), |banner| {
            banner.child(
                Button::new("send-team-copy", format!("Send changes to {name}"))
                    .disabled(cant_send.is_some())
                    .when_some(cant_send.clone(), |button, reason| {
                        button.tooltip(Tooltip::text(reason))
                    })
                    .on_click(move |_, window, cx| open_send_panel(send_id.clone(), window, cx)),
            )
        })
        .when(can_delete, |banner| {
            banner.child(
                Button::new("delete-team-copy", "Delete copy")
                    .on_click(move |_, window, cx| delete_copy(delete_id.clone(), window, cx)),
            )
        });
    Some(
        v_flex()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().surface_background)
            .child(line)
            .when_some(copy.send.as_ref(), |banner, panel| {
                banner.child(render_send_panel(
                    local_workspace_id,
                    copy,
                    panel,
                    cant_send,
                    cx,
                ))
            })
            .into_any_element(),
    )
}
