//! A teammate's shared chat as an agent connection, so it opens as a real,
//! read-only agent thread that follows the teammate's chat as it happens.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use acp_thread::{
    AcpThread, AgentConnection, ContextCompaction, ContextCompactionId, ContextCompactionStatus,
    UserMessageId,
};
use action_log::ActionLog;
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, Context, Entity, SharedString, Task, WeakEntity};
use markdown::Markdown;
use mnemonic_core::services::team_share_service::{
    MAX_CHAT_WAIT, TeamChatUpdate, TeamClient, TeamEntry,
};
use project::{AgentId, Project};
use ui::IconName;
use util::ResultExt as _;
use util::path_list::PathList;

/// How long to wait before asking again for a chat that isn't open or a
/// teammate that couldn't be reached.
const RETRY_CHAT_AFTER: Duration = Duration::from_secs(5);

const READ_ONLY: &str = "This is a teammate's chat; it is read-only";

fn team_agent_id(host: &str) -> AgentId {
    AgentId(format!("team:{host}").into())
}

pub(crate) struct TeamAgentServer {
    host: String,
    client: TeamClient,
}

impl TeamAgentServer {
    pub(crate) fn new(host: String, client: TeamClient) -> Self {
        Self { host, client }
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
        let connection: Rc<dyn AgentConnection> =
            Rc::new(TeamConnection::new(&self.host, self.client.clone()));
        Task::ready(Ok(connection))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

pub(crate) struct TeamConnection {
    agent_id: AgentId,
    client: TeamClient,
    polls: RefCell<HashMap<acp::SessionId, Task<()>>>,
}

impl TeamConnection {
    pub(crate) fn new(host: &str, client: TeamClient) -> Self {
        Self {
            agent_id: team_agent_id(host),
            client,
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

    fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

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
    cx: &mut App,
) -> Task<()> {
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
                    if thread
                        .update(cx, |thread, cx| mirror.apply(thread, update, cx))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {
                    // A chat closed and reopened restarts its feed, which an
                    // old cursor could be mistaken for.
                    cursor = None;
                    cx.background_executor().timer(RETRY_CHAT_AFTER).await;
                }
                Err(err) => {
                    log::debug!("Couldn't follow teammate chat {chat_id}: {err}");
                    // The chat may have been closed and reopened meanwhile.
                    cursor = None;
                    cx.background_executor().timer(RETRY_CHAT_AFTER).await;
                }
            }
        }
    })
}

/// Applies a teammate's chat updates to a thread, one sharer entry at a time.
#[derive(Default)]
pub(crate) struct ChatMirror {
    applied: Vec<TeamEntry>,
}

impl ChatMirror {
    pub(crate) fn apply(
        &mut self,
        thread: &mut AcpThread,
        update: TeamChatUpdate,
        cx: &mut Context<AcpThread>,
    ) {
        if update.reset {
            thread.truncate_entries(0, cx);
            self.applied.clear();
        }
        for change in update.changes {
            self.apply_change(thread, change.index, change.entry, cx);
        }
        if update.len < self.applied.len() {
            thread.truncate_entries(self.thread_index(update.len), cx);
            self.applied.truncate(update.len);
        }
        thread.update_plan(acp::Plan::new(update.plan), cx);
    }

    fn apply_change(
        &mut self,
        thread: &mut AcpThread,
        index: usize,
        entry: TeamEntry,
        cx: &mut Context<AcpThread>,
    ) {
        let Some(previous) = self.applied.get(index) else {
            if index == self.applied.len() {
                append_entry(thread, index, &entry, cx);
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
            thread.upsert_tool_call(tool_call.clone(), cx).log_err();
            self.applied[index] = entry;
            return;
        }
        let suffix = if index + 1 == self.applied.len() {
            message_suffix(previous, &entry)
        } else {
            None
        };
        if let Some(suffix) = suffix {
            for update in suffix {
                extend_last_entry(thread, update, cx);
            }
            self.applied[index] = entry;
            return;
        }
        thread.truncate_entries(self.thread_index(index), cx);
        let later = self.applied.split_off(index).into_iter().skip(1);
        for (offset, entry) in std::iter::once(entry).chain(later).enumerate() {
            append_entry(thread, index + offset, &entry, cx);
            self.applied.push(entry);
        }
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

fn adds_thread_entry(entry: &TeamEntry) -> bool {
    match entry {
        TeamEntry::Updates { updates } => !updates.is_empty(),
        TeamEntry::CompletedPlan { entries } => !entries.is_empty(),
        TeamEntry::Compaction { .. } => true,
        TeamEntry::Unknown => false,
    }
}

/// Adds a sharer entry to the end of the thread as an entry of its own.
fn append_entry(
    thread: &mut AcpThread,
    index: usize,
    entry: &TeamEntry,
    cx: &mut Context<AcpThread>,
) {
    match entry {
        TeamEntry::Updates { updates } => {
            let mut updates = updates.iter().cloned();
            // `handle_session_update` would merge a message into an adjacent
            // one of the same kind, but the sharer's entries stay separate.
            match updates.next() {
                Some(acp::SessionUpdate::UserMessageChunk(chunk)) => {
                    thread.push_new_user_message(chunk.content, cx)
                }
                Some(acp::SessionUpdate::AgentMessageChunk(chunk)) => {
                    thread.push_new_assistant_message(chunk.content, false, cx)
                }
                Some(acp::SessionUpdate::AgentThoughtChunk(chunk)) => {
                    thread.push_new_assistant_message(chunk.content, true, cx)
                }
                Some(update) => extend_last_entry(thread, update, cx),
                None => {}
            }
            for update in updates {
                extend_last_entry(thread, update, cx);
            }
        }
        TeamEntry::CompletedPlan { entries } => {
            thread.update_plan(acp::Plan::new(entries.clone()), cx);
            thread.snapshot_completed_plan(cx);
        }
        TeamEntry::Compaction { summary } => {
            let languages = thread.project().read(cx).languages().clone();
            let summary = summary.clone().map(|summary| {
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
    use acp_thread::{AgentThreadEntry, StubAgentConnection, ToolCallStatus};
    use gpui::{Entity, TestAppContext};
    use mnemonic_core::services::team_share_service::{DEFAULT_TEAM_PORT, TeamEntryChange};
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
        }
    }

    fn apply(
        mirror: &mut ChatMirror,
        thread: &Entity<AcpThread>,
        update: TeamChatUpdate,
        cx: &mut TestAppContext,
    ) {
        thread.update(cx, |thread, cx| mirror.apply(thread, update, cx));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
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
    async fn prompting_a_teammate_chat_is_refused(cx: &mut TestAppContext) {
        let client = TeamClient::new("teammate-mac", DEFAULT_TEAM_PORT)
            .expect("a plain machine name makes a client");
        let connection = Rc::new(TeamConnection::new("teammate-mac", client));
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
