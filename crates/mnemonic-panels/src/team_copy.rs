//! A teammate's copy of a shared workspace: a git worktree of this
//! machine's own clone at the sharer's commit, with the sharer's files laid
//! over it, kept following the sharer while they share its files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{AnyElement, App, AppContext as _, Context, Entity, PromptLevel, Task, WeakEntity};
use mnemonic_core::models::{ChatSessionRecord, CreateWorkspaceInput};
use mnemonic_core::repositories::{
    chat_session_repository, repository_repository, workspace_repository,
};
use mnemonic_core::services::team_files_service::{
    self, CopyState, FollowPlan, SkipList, TEAM_COPY_SOURCE, TeamFileManifest,
};
use mnemonic_core::services::team_share_server::MAX_BUNDLE_HAVE;
use mnemonic_core::services::team_share_service::{
    MAX_CHAT_WAIT, TeamClient, TeamClientError, TeamFileUpdate, TeamWorkspace,
};
use mnemonic_core::services::{git_worktree_service, workspace_service};
use mnemonic_core::state::AppState;
use ui::{Button, prelude::*};
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

pub(crate) struct CopyFollow {
    pub state: CopyState,
    pub status: CopyStatus,
    /// Files the teammate changed that the sharer has changed since.
    pub both_changed: Vec<String>,
    _follow: Task<()>,
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
            last_manifest = Some(manifest);
            let updated = this.update(cx, |team, cx| {
                let Some(copy) = team.copies.get_mut(&local_workspace_id) else {
                    return false;
                };
                copy.state = state;
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

/// Runs off the main thread: brings the copy up to the manifest and saves
/// what it now holds. None when the copy's workspace was removed, so
/// following it stops.
fn apply_manifest(
    mut state: CopyState,
    manifest: TeamFileManifest,
    client: TeamClient,
    progress: mpsc::UnboundedSender<(usize, usize)>,
) -> Option<(CopyState, Result<FollowPlan, String>)> {
    let Some(app) = mnemonic_bridge::mnemonic_state_cloned() else {
        return Some((state, Err("Mnemonic isn't ready".to_string())));
    };
    let root = match workspace_repository::get_detail(&app.db, &state.local_workspace_id) {
        Ok(Some(detail)) => PathBuf::from(
            detail
                .summary
                .workspace_root_path
                .unwrap_or(detail.worktree_path),
        ),
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
    if let Err(err) = team_files_service::save_copy_state(&app.data_dir, &state) {
        log::error!("Couldn't save the copy {}: {err}", state.local_workspace_id);
    }
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

/// Opens this machine's copy of a teammate's shared workspace, making it
/// first when there isn't one yet. A new copy opens as soon as its worktree
/// exists; its banner shows the sharer's files arriving.
#[expect(
    dead_code,
    reason = "The mirror's \"Work on this in your copy\" button calls this"
)]
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
    // A copy removed from the sidebar stays listed until its follow loop
    // next notices, so it is checked against the workspaces that exist.
    let existing = team
        .read(cx)
        .copies
        .values()
        .find(|copy| copy.state.host == host && copy.state.workspace_id == shared.id)
        .map(|copy| copy.state.local_workspace_id.clone())
        .filter(|local_workspace_id| {
            matches!(
                workspace_repository::get_detail(&app.db, local_workspace_id),
                Ok(Some(_))
            )
        });
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

/// A line above a teammate's copy when it isn't simply following the
/// sharer. None for any other workspace.
pub(crate) fn render_copy_banner(local_workspace_id: &str, cx: &App) -> Option<AnyElement> {
    let team = team_share(cx)?;
    let copy = team.read(cx).copies.get(local_workspace_id)?;
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
        CopyStatus::Following if copy.both_changed.is_empty() => return None,
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
    let local_workspace_id = local_workspace_id.to_string();
    Some(
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().surface_background)
            .child(
                div().flex_grow(1.0).overflow_hidden().child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .when(can_delete, |banner| {
                banner.child(Button::new("delete-team-copy", "Delete copy").on_click(
                    move |_, window, cx| delete_copy(local_workspace_id.clone(), window, cx),
                ))
            })
            .into_any_element(),
    )
}
