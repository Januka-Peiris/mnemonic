//! A teammate's shared chat as an agent connection, so it opens as a real,
//! read-only agent thread that follows the teammate's chat as it happens.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use acp_thread::{
    AcpThread, AgentConnection, AgentThreadEntry, AuthorizationKind, ContextCompaction,
    ContextCompactionId, ContextCompactionStatus, PermissionOptions, RequestPermissionOutcome,
    ToolCall, ToolCallStatus, UserMessageId,
};
use action_log::ActionLog;
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::{Result, anyhow};
use gpui::{App, AppContext, Context, Entity, SharedString, Task, WeakEntity};
use markdown::Markdown;
use mnemonic_core::services::team_share_service::{
    MAX_CHAT_WAIT, TeamChatUpdate, TeamClient, TeamClientError, TeamEntry, TeamPermissionRequest,
};
use project::{AgentId, Project};
use ui::IconName;
use util::ResultExt as _;
use util::path_list::PathList;

/// How long to wait before asking again for a chat that isn't open or a
/// teammate that couldn't be reached.
const RETRY_CHAT_AFTER: Duration = Duration::from_secs(5);

const READ_ONLY: &str = "This is a teammate's chat; it is read-only";

/// Shows the user why an action on a teammate's chat failed.
pub(crate) type ReportFailure = Rc<dyn Fn(SharedString, &mut App)>;

/// Posts the option the viewer picked for a teammate agent's tool call, as
/// `(tool_call_id, option_id)`, failing when the sharer is still waiting.
type AnswerPermission = Rc<dyn Fn(String, String, &mut App) -> Task<Result<()>>>;

/// A `ReportFailure` that can live in an `AgentServer`, which must be `Send`.
pub(crate) type SendableReportFailure = Arc<dyn Fn(SharedString, &mut App) + Send + Sync>;

fn team_agent_id(host: &str) -> AgentId {
    AgentId(format!("team:{host}").into())
}

pub(crate) struct TeamAgentServer {
    host: String,
    client: TeamClient,
    report: SendableReportFailure,
}

impl TeamAgentServer {
    pub(crate) fn new(host: String, client: TeamClient, report: SendableReportFailure) -> Self {
        Self {
            host,
            client,
            report,
        }
    }
}

impl AgentServer for TeamAgentServer {
    fn logo(&self) -> IconName {
        IconName::UserGroup
    }

    fn agent_id(&self) -> AgentId {
        team_agent_id(&self.host)
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        _project: Entity<Project>,
        _cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let report = self.report.clone();
        let connection: Rc<dyn AgentConnection> = Rc::new(TeamConnection::new(
            &self.host,
            self.client.clone(),
            Rc::new(move |message, cx| report(message, cx)),
        ));
        Task::ready(Ok(connection))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

pub(crate) struct TeamConnection {
    agent_id: AgentId,
    client: TeamClient,
    report: ReportFailure,
    polls: RefCell<HashMap<acp::SessionId, Task<()>>>,
}

impl TeamConnection {
    pub(crate) fn new(host: &str, client: TeamClient, report: ReportFailure) -> Self {
        Self {
            agent_id: team_agent_id(host),
            client,
            report,
            polls: RefCell::new(HashMap::new()),
        }
    }
}

impl AgentConnection for TeamConnection {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn telemetry_id(&self) -> SharedString {
        "team".into()
    }

    fn new_session(
        self: Rc<Self>,
        _project: Entity<Project>,
        _work_dirs: PathList,
        _cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        Task::ready(Err(anyhow!(READ_ONLY)))
    }

    fn supports_load_session(&self) -> bool {
        true
    }

    fn load_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let thread = cx.new(|cx| {
            AcpThread::new(
                None,
                title,
                Some(work_dirs),
                self.clone(),
                project,
                action_log,
                session_id.clone(),
                watch::Receiver::constant(acp::PromptCapabilities::new()),
                cx,
            )
        });
        let poll = poll_chat(
            self.client.clone(),
            session_id.0.to_string(),
            thread.downgrade(),
            self.report.clone(),
            cx,
        );
        self.polls.borrow_mut().insert(session_id, poll);
        Task::ready(Ok(thread))
    }

    fn supports_close_session(&self) -> bool {
        true
    }

    fn close_session(
        self: Rc<Self>,
        session_id: &acp::SessionId,
        _cx: &mut App,
    ) -> Task<Result<()>> {
        self.polls.borrow_mut().remove(session_id);
        Task::ready(Ok(()))
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }

    fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(READ_ONLY)))
    }

    fn prompt(
        &self,
        _user_message_id: UserMessageId,
        _params: acp::PromptRequest,
        _cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        Task::ready(Err(anyhow!(READ_ONLY)))
    }

    fn cancel(&self, session_id: &acp::SessionId, cx: &mut App) {
        let client = self.client.clone();
        let chat_id = session_id.0.to_string();
        let report = self.report.clone();
        cx.spawn(async move |cx| {
            let result = cx
                .background_spawn(async move { client.stop(&chat_id) })
                .await;
            if let Err(err) = result {
                cx.update(|cx| report(format!("Couldn't stop the agent: {err}").into(), cx));
            }
        })
        .detach();
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

/// Follows a chat until the thread is dropped. Holds the thread weakly, as the
/// thread holds the connection that holds this task.
fn poll_chat(
    client: TeamClient,
    chat_id: String,
    thread: WeakEntity<AcpThread>,
    report: ReportFailure,
    cx: &mut App,
) -> Task<()> {
    let answer: AnswerPermission = Rc::new({
        let client = client.clone();
        let chat_id = chat_id.clone();
        move |tool_call_id, option_id, cx| {
            let client = client.clone();
            let chat_id = chat_id.clone();
            let report = report.clone();
            cx.spawn(async move |cx| {
                let result = cx
                    .background_spawn(async move {
                        client.answer_permission(&chat_id, &tool_call_id, &option_id)
                    })
                    .await;
                match result {
                    // A 409 means someone else answered first, and the next
                    // update withdraws the request.
                    Ok(()) | Err(TeamClientError::Refused { status: 409, .. }) => Ok(()),
                    Err(err) => {
                        let message = format!("Couldn't answer the agent: {err}");
                        cx.update(|cx| report(message.clone().into(), cx));
                        Err(anyhow!(message))
                    }
                }
            })
        }
    });
    cx.spawn(async move |cx| {
        let mut mirror = ChatMirror::default();
        let mut cursor: Option<String> = None;
        loop {
            let result = cx
                .background_spawn({
                    let client = client.clone();
                    let chat_id = chat_id.clone();
                    let cursor = cursor.clone();
                    async move { client.chat(&chat_id, cursor.as_deref(), MAX_CHAT_WAIT) }
                })
                .await;
            match result {
                Ok(Some(update)) => {
                    cursor = Some(update.cursor.clone());
                    if apply_chat_update(&mut mirror, &thread, update, &answer, cx).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    // A chat closed and reopened restarts its feed, which an
                    // old cursor could be mistaken for.
                    cursor = None;
                    if show_teammate_unreachable(&thread, cx).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_CHAT_AFTER).await;
                }
                Err(err) => {
                    log::debug!("Couldn't follow teammate chat {chat_id}: {err}");
                    // The chat may have been closed and reopened meanwhile.
                    cursor = None;
                    if show_teammate_unreachable(&thread, cx).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_CHAT_AFTER).await;
                }
            }
        }
    })
}

/// Applies each change in its own `thread.update`: the thread's `NewEntry`
/// events are delivered only once an update ends, and their handlers take the
/// last entry as the new one, so two pushes in one update break the chat view.
///
/// Permission requests are matched against the thread once the entries are
/// in, as rebuilding entries drops the waiting state a request had.
fn apply_chat_update<C: AppContext>(
    mirror: &mut ChatMirror,
    thread: &WeakEntity<AcpThread>,
    mut update: TeamChatUpdate,
    answer: &AnswerPermission,
    cx: &mut C,
) -> Result<()> {
    let permissions = std::mem::take(&mut update.permissions);
    for operation in mirror.apply(update) {
        thread.update(cx, |thread, cx| operation.apply(thread, cx))?;
    }
    let (requests, withdrawn) = thread.read_with(cx, |thread, _| {
        mirror.permission_changes(thread, permissions)
    })?;
    for tool_call_id in withdrawn {
        thread.update(cx, |thread, cx| {
            thread.clear_tool_call_authorization(&tool_call_id, cx)
        })?;
    }
    for request in requests {
        let answered_here = mirror.answered_here.clone();
        let answer = answer.clone();
        thread.update(cx, |thread, cx| {
            request_authorization(thread, request, answered_here, answer, cx)
        })?;
    }
    Ok(())
}

/// Shows a chat the teammate closed, or a teammate who can't be reached, as
/// idle with nothing waiting, so no Stop or permission button is offered
/// that can only fail. The next update from the teammate shows the chat
/// again as it is.
fn show_teammate_unreachable<C: AppContext>(
    thread: &WeakEntity<AcpThread>,
    cx: &mut C,
) -> Result<()> {
    thread.update(cx, |thread, cx| thread.set_remote_generating(false, cx))?;
    let waiting: Vec<acp::ToolCallId> = thread.read_with(cx, |thread, _| {
        thread
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                AgentThreadEntry::ToolCall(ToolCall {
                    id,
                    status: ToolCallStatus::WaitingForConfirmation { .. },
                    ..
                }) => Some(id.clone()),
                _ => None,
            })
            .collect()
    })?;
    for tool_call_id in waiting {
        thread.update(cx, |thread, cx| {
            thread.clear_tool_call_authorization(&tool_call_id, cx)
        })?;
    }
    Ok(())
}

/// Shows a sharer's waiting request as the thread's own, and passes on the
/// option the viewer picks.
fn request_authorization(
    thread: &mut AcpThread,
    request: TeamPermissionRequest,
    answered_here: Rc<RefCell<HashSet<String>>>,
    answer: AnswerPermission,
    cx: &mut Context<AcpThread>,
) {
    let tool_call_id = request.tool_call_id;
    let outcome = thread.request_tool_call_authorization(
        acp::ToolCallUpdate::new(
            acp::ToolCallId::new(tool_call_id.clone()),
            acp::ToolCallUpdateFields::new(),
        ),
        PermissionOptions::Flat(request.options),
        AuthorizationKind::PermissionGrant,
        cx,
    );
    let Some(outcome) = outcome.log_err() else {
        return;
    };
    cx.spawn(async move |_, cx| {
        // A request withdrawn here resolves as cancelled, which the sharer
        // must not hear about.
        if let RequestPermissionOutcome::Selected(selected) = outcome.await {
            answered_here.borrow_mut().insert(tool_call_id.clone());
            let posted =
                cx.update(|cx| answer(tool_call_id.clone(), selected.option_id.0.to_string(), cx));
            // The failure is already reported; the sharer is still waiting,
            // so the next update shows the buttons again.
            if posted.await.is_err() {
                answered_here.borrow_mut().remove(&tool_call_id);
            }
        }
    })
    .detach();
}

/// Turns a teammate's chat updates into changes to a thread, one sharer entry
/// at a time.
#[derive(Default)]
pub(crate) struct ChatMirror {
    applied: Vec<TeamEntry>,
    /// Requests the viewer answered, which stay listed until the sharer's
    /// agent takes the answer and must not be asked again meanwhile.
    answered_here: Rc<RefCell<HashSet<String>>>,
}

impl ChatMirror {
    pub(crate) fn apply(&mut self, update: TeamChatUpdate) -> Vec<MirrorOperation> {
        let mut operations = Vec::new();
        if update.reset {
            operations.push(MirrorOperation::Truncate(0));
            self.applied.clear();
        }
        for change in update.changes {
            self.apply_change(
                change.index,
                without_locations(change.entry),
                &mut operations,
            );
        }
        if update.len < self.applied.len() {
            operations.push(MirrorOperation::Truncate(self.thread_index(update.len)));
            self.applied.truncate(update.len);
        }
        operations.push(MirrorOperation::UpdatePlan(update.plan));
        operations.push(MirrorOperation::SetGenerating(update.generating));
        operations
    }

    fn apply_change(
        &mut self,
        index: usize,
        entry: TeamEntry,
        operations: &mut Vec<MirrorOperation>,
    ) {
        let Some(previous) = self.applied.get(index) else {
            if index == self.applied.len() {
                append_entry(index, &entry, operations);
                self.applied.push(entry);
            } else {
                log::warn!(
                    "Teammate chat skipped from {} to entry {index}",
                    self.applied.len()
                );
            }
            return;
        };
        if *previous == entry {
            return;
        }
        if let Some(tool_call) = same_tool_call(previous, &entry) {
            operations.push(MirrorOperation::UpsertToolCall(tool_call.clone()));
            self.applied[index] = entry;
            return;
        }
        let suffix = if index + 1 == self.applied.len() {
            message_suffix(previous, &entry)
        } else {
            None
        };
        if let Some(suffix) = suffix {
            operations.extend(suffix.into_iter().map(MirrorOperation::ExtendLastEntry));
            self.applied[index] = entry;
            return;
        }
        operations.push(MirrorOperation::Truncate(self.thread_index(index)));
        let later = self.applied.split_off(index).into_iter().skip(1);
        for (offset, entry) in std::iter::once(entry).chain(later).enumerate() {
            append_entry(index + offset, &entry, operations);
            self.applied.push(entry);
        }
    }

    /// The requests to show and the tool calls whose request went away
    /// without an answer here.
    fn permission_changes(
        &self,
        thread: &AcpThread,
        permissions: Vec<TeamPermissionRequest>,
    ) -> (Vec<TeamPermissionRequest>, Vec<acp::ToolCallId>) {
        let mut answered_here = self.answered_here.borrow_mut();
        answered_here.retain(|tool_call_id| {
            permissions
                .iter()
                .any(|request| request.tool_call_id == *tool_call_id)
        });
        let is_listed = |tool_call_id: &acp::ToolCallId| {
            permissions
                .iter()
                .any(|request| *request.tool_call_id == *tool_call_id.0)
        };
        let withdrawn = thread
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                AgentThreadEntry::ToolCall(ToolCall {
                    id,
                    status: ToolCallStatus::WaitingForConfirmation { .. },
                    ..
                }) if !is_listed(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let requests = permissions
            .iter()
            .filter(|request| {
                !answered_here.contains(&request.tool_call_id)
                    && thread
                        .tool_call(&acp::ToolCallId::new(request.tool_call_id.clone()))
                        .is_some_and(|(_, tool_call)| {
                            !matches!(
                                tool_call.status,
                                ToolCallStatus::WaitingForConfirmation { .. }
                            )
                        })
            })
            .cloned()
            .collect();
        (requests, withdrawn)
    }

    /// Where the sharer's entry `index` sits in the thread, which skips
    /// entries this version can't show.
    fn thread_index(&self, index: usize) -> usize {
        self.applied
            .iter()
            .take(index)
            .filter(|entry| adds_thread_entry(entry))
            .count()
    }
}

/// One change to a mirrored thread, adding at most one entry to it.
pub(crate) enum MirrorOperation {
    Truncate(usize),
    /// Starts a new entry with a message chunk.
    NewMessage(acp::SessionUpdate),
    ExtendLastEntry(acp::SessionUpdate),
    UpsertToolCall(acp::ToolCall),
    CompletedPlan(Vec<acp::PlanEntry>),
    Compaction {
        index: usize,
        summary: Option<String>,
    },
    UpdatePlan(Vec<acp::PlanEntry>),
    SetGenerating(bool),
}

impl MirrorOperation {
    fn apply(self, thread: &mut AcpThread, cx: &mut Context<AcpThread>) {
        match self {
            MirrorOperation::Truncate(len) => thread.truncate_entries(len, cx),
            // `handle_session_update` would merge a message into an adjacent
            // one of the same kind, but the sharer's entries stay separate.
            MirrorOperation::NewMessage(update) => match update {
                acp::SessionUpdate::UserMessageChunk(chunk) => {
                    thread.push_new_user_message(chunk.content, cx)
                }
                acp::SessionUpdate::AgentMessageChunk(chunk) => {
                    thread.push_new_assistant_message(chunk.content, false, cx)
                }
                acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                    thread.push_new_assistant_message(chunk.content, true, cx)
                }
                update => extend_last_entry(thread, update, cx),
            },
            MirrorOperation::ExtendLastEntry(update) => extend_last_entry(thread, update, cx),
            MirrorOperation::UpsertToolCall(tool_call) => {
                thread.upsert_tool_call(tool_call, cx).log_err();
            }
            MirrorOperation::CompletedPlan(entries) => {
                thread.update_plan(acp::Plan::new(entries), cx);
                thread.snapshot_completed_plan(cx);
            }
            MirrorOperation::Compaction { index, summary } => {
                let languages = thread.project().read(cx).languages().clone();
                let summary = summary.map(|summary| {
                    cx.new(|cx| Markdown::new(summary.into(), Some(languages), None, cx))
                });
                thread.push_context_compaction(
                    ContextCompaction {
                        id: ContextCompactionId(format!("team-{index}").into()),
                        status: ContextCompactionStatus::Completed,
                        summary,
                    },
                    cx,
                );
            }
            MirrorOperation::UpdatePlan(entries) => thread.update_plan(acp::Plan::new(entries), cx),
            MirrorOperation::SetGenerating(generating) => {
                thread.set_remote_generating(generating, cx)
            }
        }
    }
}

/// A tool call's locations are paths on the teammate's machine. Resolving
/// them would open buffers and add worktrees in the local project.
fn without_locations(entry: TeamEntry) -> TeamEntry {
    let TeamEntry::Updates { mut updates } = entry else {
        return entry;
    };
    for update in &mut updates {
        match update {
            acp::SessionUpdate::ToolCall(tool_call) => tool_call.locations.clear(),
            acp::SessionUpdate::ToolCallUpdate(tool_call_update) => {
                tool_call_update.fields.locations = None
            }
            _ => {}
        }
    }
    TeamEntry::Updates { updates }
}

fn adds_thread_entry(entry: &TeamEntry) -> bool {
    match entry {
        TeamEntry::Updates { updates } => !updates.is_empty(),
        TeamEntry::CompletedPlan { entries } => !entries.is_empty(),
        TeamEntry::Compaction { .. } => true,
        TeamEntry::Unknown => false,
    }
}

/// The operations that add a sharer entry to the end of the thread as an
/// entry of its own.
fn append_entry(index: usize, entry: &TeamEntry, operations: &mut Vec<MirrorOperation>) {
    match entry {
        TeamEntry::Updates { updates } => {
            let mut updates = updates.iter().cloned();
            if let Some(first) = updates.next() {
                operations.push(if message_chunk(&first).is_some() {
                    MirrorOperation::NewMessage(first)
                } else {
                    MirrorOperation::ExtendLastEntry(first)
                });
            }
            operations.extend(updates.map(MirrorOperation::ExtendLastEntry));
        }
        TeamEntry::CompletedPlan { entries } => {
            operations.push(MirrorOperation::CompletedPlan(entries.clone()));
        }
        TeamEntry::Compaction { summary } => operations.push(MirrorOperation::Compaction {
            index,
            summary: summary.clone(),
        }),
        TeamEntry::Unknown => {}
    }
}

fn extend_last_entry(
    thread: &mut AcpThread,
    update: acp::SessionUpdate,
    cx: &mut Context<AcpThread>,
) {
    match update {
        // `handle_session_update` drops a user chunk the message already
        // holds, but the sharer's message can repeat one.
        acp::SessionUpdate::UserMessageChunk(chunk) => {
            thread.push_user_content_block(None, chunk.content, cx)
        }
        update => {
            thread.handle_session_update(update, cx).log_err();
        }
    }
}

/// The new tool call when both entries are snapshots of the same one.
fn same_tool_call<'a>(previous: &TeamEntry, entry: &'a TeamEntry) -> Option<&'a acp::ToolCall> {
    let (
        TeamEntry::Updates { updates: previous },
        TeamEntry::Updates { updates: current },
    ) = (previous, entry)
    else {
        return None;
    };
    match (previous.as_slice(), current.as_slice()) {
        (
            [acp::SessionUpdate::ToolCall(previous)],
            [acp::SessionUpdate::ToolCall(current)],
        ) if previous.tool_call_id == current.tool_call_id => Some(current),
        _ => None,
    }
}

fn message_chunk(update: &acp::SessionUpdate) -> Option<&acp::ContentChunk> {
    match update {
        acp::SessionUpdate::UserMessageChunk(chunk)
        | acp::SessionUpdate::AgentMessageChunk(chunk)
        | acp::SessionUpdate::AgentThoughtChunk(chunk) => Some(chunk),
        _ => None,
    }
}

fn chunk_text(chunk: &acp::ContentChunk) -> Option<&str> {
    match &chunk.content {
        acp::ContentBlock::Text(content) => Some(&content.text),
        _ => None,
    }
}

/// When a message entry has only grown (its last chunk's text extended, then
/// any new chunks), the updates that add what's new.
fn message_suffix(previous: &TeamEntry, entry: &TeamEntry) -> Option<Vec<acp::SessionUpdate>> {
    let (
        TeamEntry::Updates { updates: previous },
        TeamEntry::Updates { updates: current },
    ) = (previous, entry)
    else {
        return None;
    };
    let is_user = |update: &acp::SessionUpdate| {
        matches!(update, acp::SessionUpdate::UserMessageChunk(_))
    };
    let first = previous.first()?;
    let same_message = previous
        .iter()
        .chain(current)
        .all(|update| message_chunk(update).is_some() && is_user(update) == is_user(first));
    if !same_message || current.len() < previous.len() {
        return None;
    }
    let last_index = previous.len() - 1;
    if previous[..last_index] != current[..last_index] {
        return None;
    }
    let (old_last, new_last) = (&previous[last_index], &current[last_index]);
    if std::mem::discriminant(old_last) != std::mem::discriminant(new_last) {
        return None;
    }
    let mut suffix = Vec::new();
    if old_last != new_last {
        let old_text = message_chunk(old_last).and_then(chunk_text)?;
        let new_text = message_chunk(new_last).and_then(chunk_text)?;
        let added = new_text.strip_prefix(old_text)?;
        let mut added_update = new_last.clone();
        if let acp::SessionUpdate::UserMessageChunk(chunk)
        | acp::SessionUpdate::AgentMessageChunk(chunk)
        | acp::SessionUpdate::AgentThoughtChunk(chunk) = &mut added_update
        {
            chunk.content = added.to_string().into();
        }
        suffix.push(added_update);
    }
    suffix.extend(current[previous.len()..].iter().cloned());
    Some(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{
        AgentThreadEntry, SelectedPermissionOutcome, StubAgentConnection, ThreadStatus,
        ToolCallStatus,
    };
    use gpui::{Entity, TestAppContext};
    use mnemonic_core::services::team_share_service::{
        DEFAULT_TEAM_PORT, TeamEntryChange, TeamPermissionRequest,
    };
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::rc::Rc;
    use std::time::Duration;
    use util::path_list::PathList;

    async fn new_thread(cx: &mut TestAppContext) -> Entity<AcpThread> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        cx.update(|cx| connection.new_session(project, PathList::default(), cx))
            .await
            .expect("the stub connection starts a session")
    }

    fn user(text: &str) -> TeamEntry {
        TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                text.into(),
            ))],
        }
    }

    fn assistant(text: &str) -> TeamEntry {
        TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                text.into(),
            ))],
        }
    }

    fn tool_call(status: acp::ToolCallStatus) -> TeamEntry {
        TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new("read-login"), "Read login.rs")
                    .kind(acp::ToolKind::Read)
                    .status(status),
            )],
        }
    }

    fn update(reset: bool, len: usize, changes: Vec<(usize, TeamEntry)>) -> TeamChatUpdate {
        TeamChatUpdate {
            cursor: String::new(),
            reset,
            len,
            changes: changes
                .into_iter()
                .map(|(index, entry)| TeamEntryChange { index, entry })
                .collect(),
            plan: Vec::new(),
            generating: false,
            permissions: Vec::new(),
        }
    }

    fn no_answer() -> AnswerPermission {
        Rc::new(|_, _, _| Task::ready(Ok(())))
    }

    fn apply(
        mirror: &mut ChatMirror,
        thread: &Entity<AcpThread>,
        update: TeamChatUpdate,
        cx: &mut TestAppContext,
    ) {
        apply_answering(mirror, thread, update, &no_answer(), cx);
    }

    fn apply_answering(
        mirror: &mut ChatMirror,
        thread: &Entity<AcpThread>,
        update: TeamChatUpdate,
        answer: &AnswerPermission,
        cx: &mut TestAppContext,
    ) {
        apply_chat_update(mirror, &thread.downgrade(), update, answer, cx)
            .expect("the thread is still open");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
    }

    type Answers = Rc<RefCell<Vec<(String, String)>>>;

    fn recording_answer() -> (Answers, AnswerPermission) {
        let answers = Answers::default();
        let answer: AnswerPermission = Rc::new({
            let answers = answers.clone();
            move |tool_call_id, option_id, _| {
                answers.borrow_mut().push((tool_call_id, option_id));
                Task::ready(Ok(()))
            }
        });
        (answers, answer)
    }

    fn run_tests_call() -> TeamEntry {
        TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new("call-1"), "Run the tests")
                    .kind(acp::ToolKind::Execute)
                    .status(acp::ToolCallStatus::Pending),
            )],
        }
    }

    fn asking_about_run_tests(reset: bool, changes: Vec<(usize, TeamEntry)>) -> TeamChatUpdate {
        TeamChatUpdate {
            permissions: vec![TeamPermissionRequest {
                tool_call_id: "call-1".to_string(),
                options: vec![
                    acp::PermissionOption::new(
                        "allow",
                        "Allow",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        "reject",
                        "Reject",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ],
            }],
            ..update(reset, 1, changes)
        }
    }

    /// The options of `call-1` when it is waiting for confirmation.
    fn waiting_options(thread: &Entity<AcpThread>, cx: &mut TestAppContext) -> Option<Vec<String>> {
        thread.read_with(cx, |thread, _| {
            let (_, tool_call) = thread.tool_call(&acp::ToolCallId::new("call-1"))?;
            match &tool_call.status {
                ToolCallStatus::WaitingForConfirmation {
                    options: PermissionOptions::Flat(options),
                    ..
                } => Some(
                    options
                        .iter()
                        .map(|option| option.option_id.0.to_string())
                        .collect(),
                ),
                _ => None,
            }
        })
    }

    async fn waiting_on_run_tests(
        cx: &mut TestAppContext,
    ) -> (Entity<AcpThread>, ChatMirror, Answers, AnswerPermission) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        let (answers, answer) = recording_answer();
        apply_answering(
            &mut mirror,
            &thread,
            asking_about_run_tests(true, vec![(0, run_tests_call())]),
            &answer,
            cx,
        );
        (thread, mirror, answers, answer)
    }

    #[gpui::test]
    async fn streaming_reply_appends_only_the_new_text(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        apply(
            &mut mirror,
            &thread,
            update(true, 2, vec![(0, user("Say hello")), (1, assistant("Hel"))]),
            cx,
        );
        apply(
            &mut mirror,
            &thread,
            update(false, 2, vec![(1, assistant("Hello"))]),
            cx,
        );

        thread.read_with(cx, |thread, cx| {
            let replies: Vec<String> = thread
                .entries()
                .iter()
                .filter_map(|entry| match entry {
                    AgentThreadEntry::AssistantMessage(message) => Some(
                        message
                            .chunks
                            .iter()
                            .map(|chunk| match chunk {
                                acp_thread::AssistantMessageChunk::Message { block }
                                | acp_thread::AssistantMessageChunk::Thought { block } => {
                                    block.to_markdown(cx).to_string()
                                }
                            })
                            .collect(),
                    ),
                    _ => None,
                })
                .collect();
            assert_eq!(replies, vec!["Hello".to_string()]);
        });
    }

    #[gpui::test]
    async fn tool_call_update_in_the_middle_keeps_later_entries(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        apply(
            &mut mirror,
            &thread,
            update(
                true,
                3,
                vec![
                    (0, user("Look at the login page")),
                    (1, tool_call(acp::ToolCallStatus::Pending)),
                    (2, assistant("Reading it now.")),
                ],
            ),
            cx,
        );
        apply(
            &mut mirror,
            &thread,
            update(
                false,
                3,
                vec![(1, tool_call(acp::ToolCallStatus::Completed))],
            ),
            cx,
        );

        thread.read_with(cx, |thread, cx| {
            let entries = thread.entries();
            assert_eq!(entries.len(), 3);
            match &entries[1] {
                AgentThreadEntry::ToolCall(call) => {
                    assert!(matches!(call.status, ToolCallStatus::Completed));
                }
                other => panic!("expected a tool call, got {other:?}"),
            }
            assert!(entries[2].to_markdown(cx).contains("Reading it now."));
        });
    }

    #[gpui::test]
    async fn reset_rebuilds_the_thread(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        apply(
            &mut mirror,
            &thread,
            update(
                true,
                3,
                vec![
                    (0, user("First question")),
                    (1, assistant("First answer")),
                    (2, user("Second question")),
                ],
            ),
            cx,
        );
        apply(
            &mut mirror,
            &thread,
            update(true, 1, vec![(0, user("Only question"))]),
            cx,
        );

        thread.read_with(cx, |thread, cx| {
            assert_eq!(thread.entries().len(), 1);
            assert!(thread.entries()[0].to_markdown(cx).contains("Only question"));
        });
    }

    #[gpui::test]
    async fn each_new_entry_is_announced_after_only_that_entry_is_added(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let lengths_at_new_entry = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let lengths_at_new_entry = lengths_at_new_entry.clone();
            cx.subscribe(&thread, move |thread, event, cx| {
                if let acp_thread::AcpThreadEvent::NewEntry = event {
                    lengths_at_new_entry
                        .borrow_mut()
                        .push(thread.read(cx).entries().len());
                }
            })
        });
        let mut mirror = ChatMirror::default();
        // Called outside an app update, as `poll_chat` does from its task,
        // so each thread update delivers its events when it ends.
        apply_chat_update(
            &mut mirror,
            &thread.downgrade(),
            update(
                true,
                3,
                vec![
                    (0, user("First question")),
                    (1, assistant("First answer")),
                    (2, user("Second question")),
                ],
            ),
            &no_answer(),
            cx,
        )
        .expect("the thread is still open");
        cx.run_until_parked();

        assert_eq!(*lengths_at_new_entry.borrow(), vec![1, 2, 3]);
    }

    #[gpui::test]
    async fn teammate_tool_call_locations_are_not_opened_here(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        let read_secret = TeamEntry::Updates {
            updates: vec![acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new("read-secret"), "Read secret.rs")
                    .kind(acp::ToolKind::Read)
                    .locations(vec![acp::ToolCallLocation::new("/teammate/secret.rs")]),
            )],
        };
        apply(
            &mut mirror,
            &thread,
            update(true, 1, vec![(0, read_secret)]),
            cx,
        );

        thread.read_with(cx, |thread, cx| {
            match thread.entries() {
                [AgentThreadEntry::ToolCall(call)] => assert!(call.locations.is_empty()),
                other => panic!("expected one tool call, got {other:?}"),
            }
            assert!(thread.project().read(cx).agent_location().is_none());
        });
    }

    #[gpui::test]
    async fn a_mirrored_chat_is_working_while_the_sharers_is(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        let working = TeamChatUpdate {
            generating: true,
            ..update(true, 0, Vec::new())
        };
        apply(&mut mirror, &thread, working, cx);
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.status(), ThreadStatus::Generating);
            assert!(thread.entries().is_empty());
        });

        apply(&mut mirror, &thread, update(false, 0, Vec::new()), cx);
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.status(), ThreadStatus::Idle);
            assert!(thread.entries().is_empty());
        });
    }

    #[gpui::test]
    async fn permission_buttons_appear_for_a_waiting_request(cx: &mut TestAppContext) {
        let (thread, _mirror, _answers, _answer) = waiting_on_run_tests(cx).await;
        assert_eq!(
            waiting_options(&thread, cx),
            Some(vec!["allow".to_string(), "reject".to_string()])
        );
    }

    fn pick_allow(thread: &Entity<AcpThread>, cx: &mut TestAppContext) {
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                acp::ToolCallId::new("call-1"),
                SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("allow"),
                    acp::PermissionOptionKind::AllowOnce,
                ),
                cx,
            )
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn picking_an_option_posts_the_answer_once(cx: &mut TestAppContext) {
        let (thread, mut mirror, answers, answer) = waiting_on_run_tests(cx).await;
        pick_allow(&thread, cx);
        assert_eq!(
            *answers.borrow(),
            vec![("call-1".to_string(), "allow".to_string())]
        );

        apply_answering(
            &mut mirror,
            &thread,
            asking_about_run_tests(false, Vec::new()),
            &answer,
            cx,
        );
        assert_eq!(waiting_options(&thread, cx), None);
        assert_eq!(answers.borrow().len(), 1);
    }

    #[gpui::test]
    async fn a_failed_answer_asks_again(cx: &mut TestAppContext) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        let failing_answer: AnswerPermission =
            Rc::new(|_, _, _| Task::ready(Err(anyhow!("The teammate couldn't be reached"))));
        apply_answering(
            &mut mirror,
            &thread,
            asking_about_run_tests(true, vec![(0, run_tests_call())]),
            &failing_answer,
            cx,
        );
        pick_allow(&thread, cx);

        apply_answering(
            &mut mirror,
            &thread,
            asking_about_run_tests(false, Vec::new()),
            &failing_answer,
            cx,
        );
        assert_eq!(
            waiting_options(&thread, cx),
            Some(vec!["allow".to_string(), "reject".to_string()])
        );
    }

    #[gpui::test]
    async fn a_request_answered_elsewhere_clears_without_posting(cx: &mut TestAppContext) {
        let (thread, mut mirror, answers, answer) = waiting_on_run_tests(cx).await;
        apply_answering(
            &mut mirror,
            &thread,
            update(false, 1, Vec::new()),
            &answer,
            cx,
        );
        assert_eq!(waiting_options(&thread, cx), None);
        assert!(answers.borrow().is_empty());
    }

    #[gpui::test]
    async fn a_reset_asks_again_for_requests_still_waiting(cx: &mut TestAppContext) {
        let (thread, mut mirror, answers, answer) = waiting_on_run_tests(cx).await;
        apply_answering(
            &mut mirror,
            &thread,
            asking_about_run_tests(true, vec![(0, run_tests_call())]),
            &answer,
            cx,
        );
        assert_eq!(
            waiting_options(&thread, cx),
            Some(vec!["allow".to_string(), "reject".to_string()])
        );
        assert!(answers.borrow().is_empty());
    }

    #[gpui::test]
    async fn an_unreachable_teammate_leaves_the_chat_idle_with_nothing_to_answer(
        cx: &mut TestAppContext,
    ) {
        let thread = new_thread(cx).await;
        let mut mirror = ChatMirror::default();
        let (answers, answer) = recording_answer();
        let working_and_asking = TeamChatUpdate {
            generating: true,
            ..asking_about_run_tests(true, vec![(0, run_tests_call())])
        };
        apply_answering(&mut mirror, &thread, working_and_asking, &answer, cx);
        assert_eq!(
            thread.read_with(cx, |thread, _| thread.status()),
            ThreadStatus::Generating
        );
        assert!(waiting_options(&thread, cx).is_some());

        show_teammate_unreachable(&thread.downgrade(), cx).expect("the thread is still open");
        cx.run_until_parked();

        assert_eq!(
            thread.read_with(cx, |thread, _| thread.status()),
            ThreadStatus::Idle
        );
        assert_eq!(waiting_options(&thread, cx), None);
        assert!(answers.borrow().is_empty());
    }

    #[gpui::test]
    async fn prompting_a_teammate_chat_is_refused(cx: &mut TestAppContext) {
        let client = TeamClient::new("teammate-mac", DEFAULT_TEAM_PORT)
            .expect("a plain machine name makes a client");
        let connection = Rc::new(TeamConnection::new(
            "teammate-mac",
            client,
            Rc::new(|_, _| {}),
        ));
        let result = cx
            .update(|cx| {
                connection.prompt(
                    acp_thread::UserMessageId::new(),
                    acp::PromptRequest::new(
                        acp::SessionId::new("chat-1"),
                        vec!["Can I steer this?".into()],
                    ),
                    cx,
                )
            })
            .await;
        assert!(result.is_err());
    }
}
