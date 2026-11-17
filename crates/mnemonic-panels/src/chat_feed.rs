//! Publishing workspaces and their chats into a `TeamShareHub`, and following
//! open chats' threads so the hub stays current between publishes. The team
//! side feeds its hub with shared workspaces; the phone side feeds its own
//! with every workspace.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use acp_thread::{AcpThread, AcpThreadEvent, ThreadStatus};
use agent_client_protocol::schema as acp;
use gpui::{App, Context, Entity, EntityId, EventEmitter, Subscription};
use mnemonic_core::models::{ChatSessionRecord, WorkspaceSummary};
use mnemonic_core::repositories::chat_session_repository;
use mnemonic_core::services::team_files_service::SkipList;
use mnemonic_core::services::team_share_service::{
    TeamChat, TeamEntry, TeamShareHub, TeamWorkspace, saved_chat_entries,
};
use mnemonic_core::services::{chat_transcript_service, workspace_service};

use crate::team::{
    agent_state, team_entry, thread_entries, thread_permissions, thread_plan, workspace_root,
};
use crate::workspace_view::{LiveWorkspaceAgents, WorkspaceHost, chat_agent_label, open_windows};

/// Which of an agent's permission options a hub offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionScope {
    /// Once-only answers, and only in chats whose workspace allows stepping in.
    StepInOnceOnly,
    /// Every option the agent offers, in every chat.
    All,
}

/// What a workspace shares beyond its chats.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceFlags {
    pub step_in: bool,
    pub share_files: bool,
    pub edit_files: bool,
    pub agents: bool,
    pub remote_url: Option<String>,
}

/// A shared chat with a saved transcript on disk.
pub(crate) type SavedTranscript = (PathBuf, SystemTime);

/// The workspaces to publish with their chats, as loaded off the main thread.
pub(crate) type SharedRecords = Vec<(
    WorkspaceSummary,
    Vec<(ChatSessionRecord, Option<SavedTranscript>)>,
)>;

/// A closed chat whose saved transcript is newer than what is shared.
pub(crate) struct TranscriptToRead {
    pub workspace_id: String,
    pub chat_id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// The options a one-tap answer from a notification uses.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QuickAnswers {
    pub allow_option_id: String,
    pub deny_option_id: String,
}

/// The first allow-once and the first reject-once among `options`, None
/// unless both exist, since a notification can't offer a lone button.
pub(crate) fn quick_answers(options: &[acp::PermissionOption]) -> Option<QuickAnswers> {
    let first = |wanted| {
        options
            .iter()
            .find(|option| option.kind == wanted)
            .map(|option| option.option_id.0.to_string())
    };
    Some(QuickAnswers {
        allow_option_id: first(acp::PermissionOptionKind::AllowOnce)?,
        deny_option_id: first(acp::PermissionOptionKind::RejectOnce)?,
    })
}

pub(crate) enum ChatFeedEvent {
    PermissionRequested {
        workspace_id: String,
        chat_id: String,
        tool_kind: acp::ToolKind,
        tool_call_id: String,
        quick_answers: Option<QuickAnswers>,
    },
    TurnEnded {
        workspace_id: String,
        chat_id: String,
        chat_title: String,
        stop_reason: acp::StopReason,
    },
    Errored {
        workspace_id: String,
        chat_id: String,
        chat_title: String,
    },
}

struct OpenChat {
    chat_id: String,
    workspace_id: String,
    title: String,
    thread: Entity<AcpThread>,
}

struct ChatWatch {
    thread_id: EntityId,
    workspace_id: String,
    title: String,
    _subscription: Subscription,
}

pub(crate) struct ChatFeed {
    hub: TeamShareHub,
    scope: PermissionScope,
    chat_watches: HashMap<String, ChatWatch>,
    /// Chats shared from their saved transcript, as they aren't loaded in
    /// this app, with when that transcript was written.
    saved_chats: HashMap<String, SystemTime>,
    /// When each chat's last turn ended (unix ms) and whether it errored,
    /// for the phone feed. Lost on restart.
    turn_ends: HashMap<String, (u64, bool)>,
}

impl EventEmitter<ChatFeedEvent> for ChatFeed {}

impl ChatFeed {
    pub fn new(hub: TeamShareHub, scope: PermissionScope) -> Self {
        Self {
            hub,
            scope,
            chat_watches: HashMap::new(),
            saved_chats: HashMap::new(),
            turn_ends: HashMap::new(),
        }
    }

    pub fn hub(&self) -> &TeamShareHub {
        &self.hub
    }

    /// Stops following chats and empties the hub.
    pub fn clear(&mut self) {
        self.chat_watches.clear();
        self.saved_chats.clear();
        self.turn_ends.clear();
        // Wakes any viewer waiting on a chat, so it hears the chat is gone
        // instead of waiting out its long-poll.
        self.hub.retain_chats(|_| false);
        self.hub.set_workspaces(Vec::new());
    }

    /// Publishes the current state of `records` into the hub, and returns
    /// the saved transcripts to read for closed chats.
    pub fn publish(
        &mut self,
        records: SharedRecords,
        flags: &dyn Fn(&str) -> WorkspaceFlags,
        cx: &mut Context<Self>,
    ) -> Vec<TranscriptToRead> {
        let mut live: HashMap<String, LiveWorkspaceAgents> = HashMap::new();
        for workspace in open_windows(cx) {
            for agents in WorkspaceHost::live_agents(workspace.read(cx), cx) {
                live.entry(agents.workspace_id.clone()).or_insert(agents);
            }
        }

        let mut open_chats = Vec::new();
        let mut step_in_chats = HashSet::new();
        let mut closed_chats = HashSet::new();
        let mut to_read = Vec::new();
        let mut workspaces = Vec::with_capacity(records.len());
        for (summary, chat_records) in records {
            let agents = live.get(&summary.id);
            let flags = flags(&summary.id);
            let agent_root = workspace_root(&summary).filter(|_| flags.agents);
            let mut chats = Vec::with_capacity(chat_records.len());
            for (record, transcript) in chat_records {
                let live_chat = agents.and_then(|agents| {
                    agents.chats.iter().find(|chat| chat.record_id == record.id)
                });
                let thread = live_chat.and_then(|chat| chat.thread.clone());
                let title = live_chat
                    .map(|chat| chat.title.to_string())
                    .unwrap_or(record.title);
                chats.push(self.with_turn_info(
                    TeamChat {
                        id: record.id.clone(),
                        title: title.clone(),
                        agent: chat_agent_label(&record.agent).to_string(),
                        open: thread.is_some() || self.hub.has_chat(&record.id),
                        live: thread.is_some(),
                        last_turn_ended_at: None,
                        last_turn_errored: false,
                        waiting: false,
                    },
                    thread.as_ref().map(|thread| thread.read(cx)),
                ));
                if flags.step_in {
                    step_in_chats.insert(record.id.clone());
                }
                if let Some(thread) = thread {
                    open_chats.push(OpenChat {
                        chat_id: record.id,
                        workspace_id: summary.id.clone(),
                        title,
                        thread,
                    });
                    continue;
                }
                // A chat loaded earlier in this session keeps what it showed
                // then, which says more than its saved transcript.
                let shown_live =
                    self.hub.has_chat(&record.id) && !self.saved_chats.contains_key(&record.id);
                if let Some((path, modified)) = transcript {
                    if !shown_live && self.saved_chats.get(&record.id) != Some(&modified) {
                        to_read.push(TranscriptToRead {
                            workspace_id: summary.id.clone(),
                            chat_id: record.id.clone(),
                            path,
                            modified,
                        });
                    }
                }
                closed_chats.insert(record.id);
            }
            workspaces.push(TeamWorkspace {
                pr_number: mnemonic_bridge::pr_number(&summary.id),
                pr_state: mnemonic_bridge::pr_state(&summary.id),
                changed_files: summary.changed_files.len() as u32,
                additions: summary
                    .changed_files
                    .iter()
                    .map(|file| file.additions)
                    .sum(),
                deletions: summary
                    .changed_files
                    .iter()
                    .map(|file| file.deletions)
                    .sum(),
                agent_state: agent_state(&summary.id, agents, cx),
                idle_seconds: mnemonic_bridge::last_agent_activity(&summary.id)
                    .map(|at| at.elapsed().as_secs()),
                chats,
                step_in: flags.step_in,
                share_files: flags.share_files,
                edit_files: flags.edit_files,
                remote_url: flags.remote_url,
                base_branch: Some(summary.branch_health.base_branch.trim().to_string())
                    .filter(|base_branch| !base_branch.is_empty()),
                skip_patterns: SkipList::default().patterns,
                agents: agent_root.is_some(),
                root_path: agent_root.map(|root| root.to_string_lossy().into_owned()),
                id: summary.id,
                project: summary.repo,
                name: summary.name,
                branch: summary.branch,
            });
        }
        self.hub.set_workspaces(workspaces);
        self.hub.set_step_in_chats(step_in_chats);
        self.watch_chats(open_chats, &closed_chats, cx);
        to_read
    }

    /// Follows the loaded chats. Closed chats keep what they last showed,
    /// but idle and with nothing to answer, and chats no longer published
    /// are dropped.
    fn watch_chats(
        &mut self,
        open_chats: Vec<OpenChat>,
        closed_chats: &HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        let open_ids: HashSet<String> =
            open_chats.iter().map(|chat| chat.chat_id.clone()).collect();
        self.chat_watches.retain(|id, _| open_ids.contains(id));
        self.saved_chats.retain(|id, _| closed_chats.contains(id));
        self.turn_ends
            .retain(|id, _| open_ids.contains(id) || closed_chats.contains(id));
        self.hub
            .retain_chats(|id| open_ids.contains(id) || closed_chats.contains(id));
        for chat_id in closed_chats.difference(&open_ids) {
            self.hub.set_chat_activity(chat_id, false, Vec::new());
        }
        for OpenChat {
            chat_id,
            workspace_id,
            title,
            thread,
        } in open_chats
        {
            let watch = self
                .chat_watches
                .get_mut(&chat_id)
                .filter(|watch| watch.thread_id == thread.entity_id());
            if let Some(watch) = watch {
                watch.workspace_id.clone_from(&workspace_id);
                watch.title.clone_from(&title);
                if self.hub.has_chat(&chat_id) {
                    // A step-in switch flipped since the last event changes
                    // what is shared without the thread saying anything.
                    self.publish_activity(&chat_id, thread.read(cx));
                    continue;
                }
            }
            self.replace_chat(&chat_id, thread.read(cx), cx);
            let subscription = cx.subscribe(&thread, {
                let chat_id = chat_id.clone();
                move |this, thread, event, cx| {
                    this.publish_thread_event(&chat_id, &thread, event, cx);
                }
            });
            self.chat_watches.insert(
                chat_id,
                ChatWatch {
                    thread_id: thread.entity_id(),
                    workspace_id,
                    title,
                    _subscription: subscription,
                },
            );
        }
    }

    /// Fills in the turn info the phone shows. A team feed leaves it unset.
    fn with_turn_info(&self, mut chat: TeamChat, thread: Option<&AcpThread>) -> TeamChat {
        if self.scope != PermissionScope::All {
            return chat;
        }
        if let Some((ended_at, errored)) = self.turn_ends.get(&chat.id) {
            chat.last_turn_ended_at = Some(*ended_at);
            chat.last_turn_errored = *errored;
        }
        chat.waiting =
            thread.is_some_and(|thread| !thread_permissions(thread, self.scope).is_empty());
        chat
    }

    fn record_turn_end(&mut self, chat_id: &str, errored: bool) {
        let ended_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        self.turn_ends
            .insert(chat_id.to_string(), (ended_at, errored));
    }

    fn publish_thread_event(
        &mut self,
        chat_id: &str,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let feed_event = self.feed_event(chat_id, thread.read(cx), event);
        match feed_event {
            Some(ChatFeedEvent::TurnEnded { .. }) => self.record_turn_end(chat_id, false),
            Some(ChatFeedEvent::Errored { .. }) => self.record_turn_end(chat_id, true),
            _ => {}
        }
        let thread = thread.read(cx);
        let entries = thread.entries();
        let published = match event {
            AcpThreadEvent::NewEntry => entries.len().checked_sub(1).is_some_and(|index| {
                self.hub
                    .set_chat_entry(chat_id, index, team_entry(&entries[index], cx))
            }),
            AcpThreadEvent::EntryUpdated(index) => entries.get(*index).is_some_and(|entry| {
                self.hub
                    .set_chat_entry(chat_id, *index, team_entry(entry, cx))
            }),
            AcpThreadEvent::EntriesRemoved(_) => false,
            // These can change a tool call's status without an entry
            // update of their own, so every entry is re-checked. Unchanged
            // entries cost a comparison and wake nobody.
            AcpThreadEvent::ToolAuthorizationRequested(_)
            | AcpThreadEvent::ToolAuthorizationReceived(_)
            | AcpThreadEvent::Stopped(_)
            | AcpThreadEvent::Error => {
                entries.iter().enumerate().all(|(index, entry)| {
                    self.hub
                        .set_chat_entry(chat_id, index, team_entry(entry, cx))
                }) && self.hub.chat_len(chat_id) == Some(entries.len())
            }
            _ => true,
        };
        // A plan update raises no thread event of its own, so the plan is
        // re-checked on every event. An unchanged plan wakes nobody.
        let published = published
            && self.hub.set_chat_plan(chat_id, thread_plan(thread, cx))
            && self.publish_activity(chat_id, thread);
        if !published {
            self.replace_chat(chat_id, thread, cx);
        }
        if let Some(feed_event) = feed_event {
            cx.emit(feed_event);
        }
    }

    fn feed_event(
        &self,
        chat_id: &str,
        thread: &AcpThread,
        event: &AcpThreadEvent,
    ) -> Option<ChatFeedEvent> {
        let watch = self.chat_watches.get(chat_id)?;
        let workspace_id = watch.workspace_id.clone();
        let chat_id = chat_id.to_string();
        match event {
            AcpThreadEvent::ToolAuthorizationRequested(tool_call_id) => {
                let (_, tool_call) = thread.tool_call(tool_call_id)?;
                let tool_call_id = tool_call_id.0.to_string();
                let quick_answers = thread_permissions(thread, self.scope)
                    .into_iter()
                    .find(|request| request.tool_call_id == tool_call_id)
                    .and_then(|request| quick_answers(&request.options));
                Some(ChatFeedEvent::PermissionRequested {
                    workspace_id,
                    chat_id,
                    tool_kind: tool_call.kind,
                    tool_call_id,
                    quick_answers,
                })
            }
            AcpThreadEvent::Stopped(stop_reason) => Some(ChatFeedEvent::TurnEnded {
                workspace_id,
                chat_id,
                chat_title: watch.title.clone(),
                stop_reason: *stop_reason,
            }),
            AcpThreadEvent::Error => Some(ChatFeedEvent::Errored {
                workspace_id,
                chat_id,
                chat_title: watch.title.clone(),
            }),
            _ => None,
        }
    }

    /// Shares closed chats from the transcripts just read, unless meanwhile
    /// a chat was loaded.
    pub fn share_saved_chats(&mut self, saved: Vec<(TranscriptToRead, Vec<TeamEntry>)>) {
        for (transcript, entries) in saved {
            let shown_live = self.hub.has_chat(&transcript.chat_id)
                && !self.saved_chats.contains_key(&transcript.chat_id);
            if shown_live || self.chat_watches.contains_key(&transcript.chat_id) {
                continue;
            }
            self.hub.replace_chat(&transcript.chat_id, entries);
            self.saved_chats
                .insert(transcript.chat_id, transcript.modified);
        }
    }

    fn replace_chat(&self, chat_id: &str, thread: &AcpThread, cx: &App) {
        self.hub.replace_chat(chat_id, thread_entries(thread, cx));
        self.hub.set_chat_plan(chat_id, thread_plan(thread, cx));
        self.publish_activity(chat_id, thread);
    }

    /// Shares whether a chat's agent is mid-turn and what it is waiting on,
    /// which with `StepInOnceOnly` is only shared where viewers may step in.
    /// Returns false when the chat isn't known.
    fn publish_activity(&self, chat_id: &str, thread: &AcpThread) -> bool {
        let permissions = match self.scope {
            PermissionScope::StepInOnceOnly if !self.hub.allows_step_in(chat_id) => Vec::new(),
            scope => thread_permissions(thread, scope),
        };
        self.hub.set_chat_activity(
            chat_id,
            thread.status() == ThreadStatus::Generating,
            permissions,
        )
    }
}

/// The workspaces with their chats, either those in `filter` or, with None,
/// every workspace.
pub(crate) fn load_records(
    state: &mnemonic_core::state::AppState,
    filter: Option<&HashSet<String>>,
) -> Result<SharedRecords, String> {
    workspace_service::list_workspaces(state)?
        .into_iter()
        .filter(|summary| filter.is_none_or(|ids| ids.contains(&summary.id)))
        .map(|summary| {
            let chats = chat_session_repository::list_for_workspace(&state.db, &summary.id)?
                .into_iter()
                .map(|record| {
                    let transcript = chat_transcript_service::saved_transcript_file(
                        state,
                        &summary.id,
                        &record.id,
                    );
                    (record, transcript)
                })
                .collect();
            Ok((summary, chats))
        })
        .collect()
}

pub(crate) fn read_saved_transcripts(
    to_read: Vec<TranscriptToRead>,
) -> Vec<(TranscriptToRead, Vec<TeamEntry>)> {
    to_read
        .into_iter()
        .filter_map(
            |transcript| match std::fs::read_to_string(&transcript.path) {
                Ok(markdown) => {
                    let entries = saved_chat_entries(&markdown);
                    Some((transcript, entries))
                }
                Err(err) => {
                    log::warn!("Couldn't read {}: {err}", transcript.path.display());
                    None
                }
            },
        )
        .collect()
}

#[cfg(test)]
pub(crate) mod test_support {
    use acp_thread::{
        AcpThread, AgentConnection as _, AuthorizationKind, PermissionOptions,
        RequestPermissionOutcome, StubAgentConnection, ToolCallStatus,
    };
    use agent_client_protocol::schema as acp;
    use gpui::{Entity, Task, TestAppContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::rc::Rc;
    use util::path_list::PathList;

    pub(crate) async fn new_thread(cx: &mut TestAppContext) -> Entity<AcpThread> {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        cx.update(|cx| connection.new_session(project, PathList::default(), cx))
            .await
            .expect("the stub connection starts a session")
    }

    /// A thread with one tool call waiting on `options`, and the task that
    /// receives the answer, which must be kept for the request to stay open.
    pub(crate) async fn thread_waiting_on(
        options: PermissionOptions,
        kind: AuthorizationKind,
        cx: &mut TestAppContext,
    ) -> (Entity<AcpThread>, Task<RequestPermissionOutcome>) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let thread = new_thread(cx).await;
        let tool_call_id = acp::ToolCallId::new("run-tests");
        thread
            .update(cx, |thread, cx| {
                thread.handle_session_update(
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(tool_call_id.clone(), "Run the tests")
                            .kind(acp::ToolKind::Execute),
                    ),
                    cx,
                )
            })
            .expect("the thread takes the tool call");
        let answer = thread
            .update(cx, |thread, cx| {
                thread.request_tool_call_authorization(
                    acp::ToolCallUpdate::new(tool_call_id, acp::ToolCallUpdateFields::new()),
                    options,
                    kind,
                    cx,
                )
            })
            .expect("the thread asks for permission");
        (thread, answer)
    }

    /// The choice an agent offers before editing a file with unsaved changes.
    pub(crate) fn save_or_discard_options() -> PermissionOptions {
        PermissionOptions::Flat(vec![
            acp::PermissionOption::new("save", "Save", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new("discard", "Discard", acp::PermissionOptionKind::RejectOnce),
        ])
    }

    pub(crate) fn allow_always_and_once_options() -> PermissionOptions {
        PermissionOptions::Flat(vec![
            acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new(
                "always",
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new("reject", "Reject", acp::PermissionOptionKind::RejectOnce),
        ])
    }

    pub(crate) fn is_waiting(thread: &Entity<AcpThread>, cx: &mut TestAppContext) -> bool {
        thread.read_with(cx, |thread, _| {
            thread
                .tool_call(&acp::ToolCallId::new("run-tests"))
                .is_some_and(|(_, tool_call)| {
                    matches!(
                        tool_call.status,
                        ToolCallStatus::WaitingForConfirmation { .. }
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use acp_thread::AuthorizationKind;
    use gpui::{AppContext as _, TestAppContext};
    use settings::SettingsStore;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn quick_answers_need_both_once_options() {
        let option = |id: &'static str, kind| acp::PermissionOption::new(id, id, kind);
        let both = quick_answers(&[
            option("always", acp::PermissionOptionKind::AllowAlways),
            option("yes", acp::PermissionOptionKind::AllowOnce),
            option("no", acp::PermissionOptionKind::RejectOnce),
        ]);
        assert_eq!(
            both.map(|answers| (answers.allow_option_id, answers.deny_option_id)),
            Some(("yes".to_string(), "no".to_string()))
        );
        assert!(
            quick_answers(&[
                option("always", acp::PermissionOptionKind::AllowAlways),
                option("no", acp::PermissionOptionKind::RejectOnce),
            ])
            .is_none()
        );
    }

    fn published_option_kinds(
        scope: PermissionScope,
        thread: &Entity<AcpThread>,
        cx: &mut TestAppContext,
    ) -> Vec<acp::PermissionOptionKind> {
        let hub = TeamShareHub::new();
        hub.set_step_in_chats(HashSet::from(["chat".to_string()]));
        let feed = cx.new(|_| ChatFeed::new(hub.clone(), scope));
        feed.update(cx, |feed, cx| {
            feed.watch_chats(
                vec![OpenChat {
                    chat_id: "chat".into(),
                    workspace_id: "workspace".into(),
                    title: "Run the tests".into(),
                    thread: thread.clone(),
                }],
                &HashSet::new(),
                cx,
            )
        });
        let update = hub
            .chat_update("chat", None)
            .expect("the watched chat is in the hub");
        update.permissions[0]
            .options
            .iter()
            .map(|option| option.kind)
            .collect()
    }

    #[gpui::test]
    async fn a_permission_request_is_announced_with_its_tool_kind(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let thread = new_thread(cx).await;
        let tool_call_id = acp::ToolCallId::new("run-tests");
        thread
            .update(cx, |thread, cx| {
                thread.handle_session_update(
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(tool_call_id.clone(), "Run the tests")
                            .kind(acp::ToolKind::Execute),
                    ),
                    cx,
                )
            })
            .expect("the thread takes the tool call");
        let feed = cx.new(|_| ChatFeed::new(TeamShareHub::new(), PermissionScope::All));
        feed.update(cx, |feed, cx| {
            feed.watch_chats(
                vec![OpenChat {
                    chat_id: "chat".into(),
                    workspace_id: "workspace".into(),
                    title: "Run the tests".into(),
                    thread: thread.clone(),
                }],
                &HashSet::new(),
                cx,
            )
        });
        let announced = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&feed, {
                let announced = announced.clone();
                move |_, event: &ChatFeedEvent, _| {
                    if let ChatFeedEvent::PermissionRequested {
                        workspace_id,
                        chat_id,
                        tool_kind,
                        ..
                    } = event
                    {
                        announced.borrow_mut().push((
                            workspace_id.clone(),
                            chat_id.clone(),
                            *tool_kind,
                        ));
                    }
                }
            })
        });
        let _answer = thread
            .update(cx, |thread, cx| {
                thread.request_tool_call_authorization(
                    acp::ToolCallUpdate::new(tool_call_id, acp::ToolCallUpdateFields::new()),
                    allow_always_and_once_options(),
                    AuthorizationKind::PermissionGrant,
                    cx,
                )
            })
            .expect("the thread asks for permission");
        cx.run_until_parked();

        assert_eq!(
            *announced.borrow(),
            vec![(
                "workspace".to_string(),
                "chat".to_string(),
                acp::ToolKind::Execute
            )]
        );
    }

    #[gpui::test]
    async fn all_scope_publishes_always_options(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(
            allow_always_and_once_options(),
            AuthorizationKind::PermissionGrant,
            cx,
        )
        .await;

        let all = published_option_kinds(PermissionScope::All, &thread, cx);
        assert!(all.contains(&acp::PermissionOptionKind::AllowAlways));

        let once_only = published_option_kinds(PermissionScope::StepInOnceOnly, &thread, cx);
        assert!(!once_only.is_empty());
        assert!(!once_only.contains(&acp::PermissionOptionKind::AllowAlways));
    }

    #[gpui::test]
    async fn all_scope_lists_an_action_choice_without_options(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(
            save_or_discard_options(),
            AuthorizationKind::ActionChoice,
            cx,
        )
        .await;

        let all = published_option_kinds(PermissionScope::All, &thread, cx);
        assert!(all.is_empty());
    }

    fn plain_chat() -> TeamChat {
        TeamChat {
            id: "chat".into(),
            title: "Run the tests".into(),
            agent: "Claude".into(),
            open: true,
            live: true,
            last_turn_ended_at: None,
            last_turn_errored: false,
            waiting: false,
        }
    }

    async fn feed_after_turn_ends(
        scope: PermissionScope,
        event: AcpThreadEvent,
        cx: &mut TestAppContext,
    ) -> (Entity<ChatFeed>, Entity<AcpThread>) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let thread = new_thread(cx).await;
        let feed = cx.new(|_| ChatFeed::new(TeamShareHub::new(), scope));
        feed.update(cx, |feed, cx| {
            feed.watch_chats(
                vec![OpenChat {
                    chat_id: "chat".into(),
                    workspace_id: "workspace".into(),
                    title: "Run the tests".into(),
                    thread: thread.clone(),
                }],
                &HashSet::new(),
                cx,
            )
        });
        thread.update(cx, |_, cx| cx.emit(event));
        cx.run_until_parked();
        (feed, thread)
    }

    #[gpui::test]
    async fn team_feed_leaves_turn_info_unset(cx: &mut TestAppContext) {
        let (feed, thread) = feed_after_turn_ends(
            PermissionScope::StepInOnceOnly,
            AcpThreadEvent::Stopped(acp::StopReason::EndTurn),
            cx,
        )
        .await;

        let chat = feed.read_with(cx, |feed, cx| {
            feed.with_turn_info(plain_chat(), Some(thread.read(cx)))
        });
        let json = serde_json::to_value(&chat).expect("a chat serializes");
        for key in ["last_turn_ended_at", "last_turn_errored", "waiting"] {
            assert!(json.get(key).is_none(), "the team wire carries {key}");
        }
    }

    #[gpui::test]
    async fn phone_feed_records_a_turn_end(cx: &mut TestAppContext) {
        let (feed, thread) = feed_after_turn_ends(
            PermissionScope::All,
            AcpThreadEvent::Stopped(acp::StopReason::EndTurn),
            cx,
        )
        .await;
        let chat = feed.read_with(cx, |feed, cx| {
            feed.with_turn_info(plain_chat(), Some(thread.read(cx)))
        });
        assert!(chat.last_turn_ended_at.is_some());
        assert!(!chat.last_turn_errored);

        thread.update(cx, |_, cx| cx.emit(AcpThreadEvent::Error));
        cx.run_until_parked();
        let chat = feed.read_with(cx, |feed, cx| {
            feed.with_turn_info(plain_chat(), Some(thread.read(cx)))
        });
        assert!(chat.last_turn_errored);
    }
}
