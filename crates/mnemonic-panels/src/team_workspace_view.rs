//! A teammate's shared workspace, shown in the main window like one of your
//! own: a header with its name, branch and PR, then one read-only tab per
//! chat, each a real chat view following the teammate's chat.

use std::any::Any;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::schema as acp;
use agent_servers::AgentServer;
use agent_ui::{Agent, AgentConnectionStore, AgentThreadSource, ConversationView};
use editor::Editor;
use gpui::{
    Action as _, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, NoAction, Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use language::Buffer;
use mnemonic_core::services::team_share_service::{
    split_unified_diff, TeamChat, TeamClient, TeamClientError, TeamFileDiff, TeamWorkspace,
};
use project::Project;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent};
use workspace::{DraggedTab, Pane, Workspace, pane};

use crate::team::{PeerStatus, render_agent_state, show_step_in_notice, team_share};
use crate::team_connection::TeamAgentServer;

/// What the window's selected workspace id holds while a teammate's
/// workspace is selected. It never matches a local workspace's id.
pub(crate) fn teammate_workspace_key(host: &str, workspace_id: &str) -> String {
    format!(
        "{}{host}:{workspace_id}",
        mnemonic_bridge::TEAMMATE_WORKSPACE_PREFIX
    )
}

/// Hands a message typed in a teammate's chat to their agent.

/// One of the teammate's chats as a tab.
pub(crate) struct TeammateChatItem {
    chat_id: String,
    title: SharedString,
    peer_name: SharedString,
    /// None until the chat is open on the teammate's machine.
    view: Option<Entity<ConversationView>>,
    /// The chat is no longer in the teammate's list, or was closed there
    /// after its content was shown here.
    closed: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<ItemEvent> for TeammateChatItem {}

impl Focusable for TeammateChatItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.view {
            Some(view) => view.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Item for TeammateChatItem {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if self.closed {
            format!("{} (closed on {}'s machine)", self.title, self.peer_name).into()
        } else {
            self.title.clone()
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Chat))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for TeammateChatItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.view {
            Some(view) => div().size_full().child(view.clone()).into_any_element(),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .p_4()
                .track_focus(&self.focus_handle)
                .child(
                    Label::new(format!(
                        "This chat isn't open on {}'s machine. It appears here once they open it.",
                        self.peer_name
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
        }
    }
}

/// One changed file's diff as a read-only tab.
struct TeammateDiffItem {
    title: SharedString,
    editor: Entity<Editor>,
}

impl EventEmitter<ItemEvent> for TeammateDiffItem {}

impl Focusable for TeammateDiffItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for TeammateDiffItem {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for TeammateDiffItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.editor.clone())
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum TeammateDiffState {
    Loading,
    Loaded,
    Failed(SharedString),
}

pub(crate) struct TeammateWorkspaceView {
    host: String,
    workspace_id: String,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    pane: Entity<Pane>,
    chats: Vec<Entity<TeammateChatItem>>,
    /// Chats whose tab was closed here. They stay closed until the
    /// workspace is selected in the sidebar again.
    dismissed: HashSet<String>,
    connection_store: Option<Entity<AgentConnectionStore>>,
    /// The teammate address the chat views follow; they are rebuilt when it
    /// changes, since their connection keeps the address it started with.
    client_url: Option<String>,
    /// The teammate was online and listing this workspace at the last sync,
    /// so the diff reloads when they come back.
    available: bool,
    /// The workspace as last listed, kept while the teammate is unreachable.
    last_known: Option<TeamWorkspace>,
    files: Vec<TeamFileDiff>,
    diff_state: TeammateDiffState,
    /// Replacing it cancels a load still in flight.
    diff_task: Option<Task<()>>,
    diff_items: Vec<(String, Entity<TeammateDiffItem>)>,
    _subscriptions: Vec<Subscription>,
}

impl TeammateWorkspaceView {
    pub(crate) fn new(
        host: String,
        workspace_id: String,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane = Self::new_pane(workspace.clone(), project.clone(), window, cx);
        let mut subscriptions = vec![cx.subscribe_in(&pane, window, Self::handle_pane_event)];
        if let Some(team) = team_share(cx) {
            subscriptions.push(cx.observe_in(&team, window, |this, _, window, cx| {
                this.sync_chats(window, cx);
            }));
        }
        let mut view = Self {
            host,
            workspace_id,
            workspace,
            project,
            pane,
            chats: Vec::new(),
            dismissed: HashSet::new(),
            connection_store: None,
            client_url: None,
            available: false,
            last_known: None,
            files: Vec::new(),
            diff_state: TeammateDiffState::Loading,
            diff_task: None,
            diff_items: Vec::new(),
            _subscriptions: subscriptions,
        };
        view.sync_chats(window, cx);
        if view.diff_task.is_none() {
            view.reload_diff(window, cx);
        }
        let first_open = view
            .chats
            .iter()
            .find(|chat| chat.read(cx).view.is_some())
            .cloned();
        if let Some(item) = first_open {
            view.pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(&item) {
                    pane.activate_item(index, false, false, window, cx);
                }
            });
        }
        view
    }

    fn new_pane(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        cx.new(|cx| {
            let this_pane = cx.entity_id();
            let mut pane = Pane::new(
                workspace,
                project,
                Default::default(),
                // Only this pane's own tabs, so they can be reordered; nothing
                // else belongs in a teammate's workspace.
                Some(Arc::new(
                    move |dragged: &dyn Any, _window: &mut Window, _cx: &mut App| {
                        dragged
                            .downcast_ref::<DraggedTab>()
                            .is_some_and(|tab| tab.pane.entity_id() == this_pane)
                    },
                )),
                NoAction.boxed_clone(),
                false,
                window,
                cx,
            );
            pane.set_can_navigate(false, cx);
            pane.display_nav_history_buttons(None);
            pane.set_should_display_tab_bar(|_, _| true);
            pane.set_can_toggle_zoom(false, cx);
            pane.set_close_pane_if_empty(false, cx);
            pane.set_render_tab_bar_buttons(cx, |_, _, _| (None, None));
            pane
        })
    }

    pub(crate) fn files(&self) -> &[TeamFileDiff] {
        &self.files
    }

    pub(crate) fn diff_state(&self) -> &TeammateDiffState {
        &self.diff_state
    }

    pub(crate) fn reload_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = team_share(cx).and_then(|team| {
            team.read(cx)
                .peer(&self.host)
                .and_then(|peer| peer.client.clone())
        });
        let Some(client) = client else {
            self.diff_state = TeammateDiffState::Failed("Can't reach this teammate".into());
            self.diff_task = None;
            cx.notify();
            return;
        };
        self.diff_state = TeammateDiffState::Loading;
        let workspace_id = self.workspace_id.clone();
        self.diff_task = Some(cx.spawn_in(window, async move |this, cx| {
            let diff = cx
                .background_spawn(async move { client.diff(&workspace_id) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match diff {
                    Ok(Some(text)) => {
                        this.files = split_unified_diff(&text);
                        this.diff_state = TeammateDiffState::Loaded;
                        this.refresh_open_diffs(window, cx);
                    }
                    Ok(None) => {
                        this.files.clear();
                        this.diff_state = TeammateDiffState::Failed(
                            "This workspace is no longer shared".into(),
                        );
                    }
                    Err(err) => {
                        this.diff_state = TeammateDiffState::Failed(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    /// Brings open diff tabs up to date with the reloaded diff and closes
    /// those whose file is no longer changed.
    fn refresh_open_diffs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (path, item) in self.diff_items.clone() {
            match self.files.iter().find(|file| file.path == path) {
                Some(file) => {
                    let buffer = item.read(cx).editor.read(cx).buffer().read(cx).as_singleton();
                    if let Some(buffer) = buffer {
                        if buffer.read(cx).text() != file.diff {
                            let text = file.diff.clone();
                            buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
                        }
                    }
                }
                None => {
                    self.pane.update(cx, |pane, cx| {
                        pane.remove_item(item.entity_id(), false, false, window, cx);
                    });
                }
            }
        }
    }

    /// Opens the file's diff as a read-only tab, or focuses the one already
    /// open.
    pub(crate) fn open_file_diff(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((_, item)) = self.diff_items.iter().find(|(open, _)| open == path) {
            let item = item.clone();
            self.pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(&item) {
                    pane.activate_item(index, true, true, window, cx);
                }
            });
            return;
        }
        let Some(file) = self.files.iter().find(|file| file.path == path) else {
            return;
        };
        let text = file.diff.clone();
        let title = SharedString::from(
            path.rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(path)
                .to_string(),
        );
        let languages = self.project.read(cx).languages().clone();
        let path = path.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let language = languages.language_for_name("Diff").await.log_err();
            this.update_in(cx, |this, window, cx| {
                // Another click may have opened the tab while the language loaded.
                if this.diff_items.iter().any(|(open, _)| *open == path) {
                    return;
                }
                let buffer = cx.new(|cx| {
                    let mut buffer = Buffer::local(text, cx);
                    buffer.set_language(language, cx);
                    buffer
                });
                let editor = cx.new(|cx| {
                    let mut editor = Editor::for_buffer(buffer, None, window, cx);
                    editor.set_read_only(true);
                    editor
                });
                let item = cx.new(|_| TeammateDiffItem { title, editor });
                this.pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(item.clone()), true, true, None, window, cx);
                });
                this.diff_items.push((path, item));
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn peer_name(&self, cx: &App) -> String {
        team_share(cx)
            .and_then(|team| {
                team.read(cx)
                    .peer(&self.host)
                    .map(|peer| peer.peer.name.clone())
            })
            .unwrap_or_else(|| self.host.clone())
    }

    pub(crate) fn title(&self, cx: &App) -> SharedString {
        let peer_name = self.peer_name(cx);
        match &self.last_known {
            Some(shared) => format!("{peer_name} / {}", shared.name).into(),
            None => peer_name.into(),
        }
    }

    /// Brings back the tabs closed here, as when the workspace is selected
    /// in the sidebar again.
    pub(crate) fn reopen_closed_chats(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismissed.is_empty() {
            return;
        }
        self.dismissed.clear();
        self.sync_chats(window, cx);
    }

    /// Matches the tabs to the teammate's chats. A chat that is no longer
    /// listed keeps its tab, marked closed, and while the teammate is
    /// unreachable every tab stays as it was.
    fn sync_chats(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(team) = team_share(cx) else {
            return;
        };
        let (peer_name, online, client, live) = {
            let team = team.read(cx);
            let peer = team.peer(&self.host);
            (
                peer.map(|peer| peer.peer.name.clone())
                    .unwrap_or_else(|| self.host.clone()),
                peer.is_some_and(|peer| matches!(peer.status, PeerStatus::Online)),
                peer.and_then(|peer| peer.client.clone()),
                peer.and_then(|peer| {
                    peer.workspaces
                        .iter()
                        .find(|workspace| workspace.id == self.workspace_id)
                        .cloned()
                }),
            )
        };

        let available = online && live.is_some();
        let came_back = available && !self.available;
        self.available = available;
        let client_url = client.as_ref().map(|client| client.base_url().to_string());
        let address_changed = client_url.is_some() && client_url != self.client_url;
        if address_changed {
            if self.client_url.is_some() {
                self.restart_chat_views(cx);
            }
            self.client_url = client_url;
        }
        if address_changed || came_back {
            self.reload_diff(window, cx);
        }

        let Some(live) = live else {
            cx.notify();
            return;
        };

        let peer_name = SharedString::from(peer_name);
        for item in &self.chats {
            let open = live
                .chats
                .iter()
                .find(|chat| chat.id == item.read(cx).chat_id)
                .map(|chat| chat.open);
            item.update(cx, |item, cx| {
                let closed = match open {
                    Some(open) => !open && item.view.is_some(),
                    None => true,
                };
                if item.closed != closed || item.peer_name != peer_name {
                    item.closed = closed;
                    item.peer_name = peer_name.clone();
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
            });
        }

        for chat in &live.chats {
            if self.dismissed.contains(&chat.id) {
                continue;
            }
            let existing = self
                .chats
                .iter()
                .find(|item| item.read(cx).chat_id == chat.id)
                .cloned();
            let item = match existing {
                Some(item) => item,
                None => {
                    let item = cx.new(|cx| TeammateChatItem {
                        chat_id: chat.id.clone(),
                        title: SharedString::from(chat.title.clone()),
                        peer_name: peer_name.clone(),
                        view: None,
                        closed: false,
                        focus_handle: cx.focus_handle(),
                    });
                    self.pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(item.clone()), false, false, None, window, cx);
                    });
                    self.chats.push(item.clone());
                    item
                }
            };
            let title = SharedString::from(chat.title.clone());
            item.update(cx, |item, cx| {
                if item.title != title {
                    item.title = title;
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
            });
            // A chat's view is built once and kept, even if the chat closes
            // again: loading the same chat twice on one connection stops the
            // first copy from updating.
            if !chat.open || item.read(cx).view.is_some() {
                continue;
            }
            if let Some(client) = client.clone() {
                let view = self.build_chat_view(chat, &peer_name, client, window, cx);
                item.update(cx, |item, cx| {
                    item.view = Some(view);
                    cx.notify();
                });
            }
        }

        mnemonic_bridge::TeammateWorkspaceTitles::set(
            &teammate_workspace_key(&self.host, &self.workspace_id),
            mnemonic_bridge::TeammateWorkspaceTitle {
                peer: peer_name.to_string(),
                project: live.project.clone(),
                name: live.name.clone(),
                branch: live.branch.clone(),
                pr_state: live.pr_state.clone(),
                pr_number: live.pr_number,
            },
            cx,
        );
        self.last_known = Some(live);
        cx.notify();
    }

    /// Drops every chat view and the connection they share, so the open
    /// chats reconnect at the teammate's new address.
    fn restart_chat_views(&mut self, cx: &mut Context<Self>) {
        self.connection_store = None;
        for item in &self.chats {
            item.update(cx, |item, cx| {
                if item.view.take().is_some() {
                    cx.notify();
                }
            });
        }
    }

    fn build_chat_view(
        &mut self,
        chat: &TeamChat,
        peer_name: &str,
        client: TeamClient,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ConversationView> {
        let connection_store = match &self.connection_store {
            Some(store) => store.clone(),
            None => {
                let project = self.project.clone();
                let store = cx.new(|cx| AgentConnectionStore::new(project, cx));
                self.connection_store = Some(store.clone());
                store
            }
        };
        let report = {
            let workspace = self.workspace.clone();
            Arc::new(move |message: SharedString, cx: &mut App| {
                show_notice(&workspace, message.to_string(), cx)
            })
        };
        let server: Rc<dyn AgentServer> =
            Rc::new(TeamAgentServer::new(self.host.clone(), client, report));
        let agent = Agent::Custom {
            id: server.agent_id(),
        };
        let session_id = acp::SessionId::new(chat.id.clone());
        let title = SharedString::from(chat.title.clone());
        let notice = SharedString::from(format!("{peer_name}'s workspace. Read-only."));
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        cx.new(|cx| {
            let mut view = ConversationView::new(
                server,
                connection_store,
                agent,
                Some(session_id),
                None,
                None,
                Some(title),
                None,
                workspace,
                project,
                None,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
            view.suppress_notifications();
            view.set_read_only(notice, cx);
            view
        })
    }

    fn handle_pane_event(
        &mut self,
        _pane: &Entity<Pane>,
        event: &pane::Event,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane::Event::RemovedItem { item } = event else {
            return;
        };
        let item_id = item.item_id();
        self.diff_items.retain(|(_, diff)| diff.entity_id() != item_id);
        if let Some(index) = self
            .chats
            .iter()
            .position(|chat| chat.entity_id() == item_id)
        {
            let chat = self.chats.remove(index);
            self.dismissed.insert(chat.read(cx).chat_id.clone());
            cx.notify();
        }
    }

    /// Why the content may be out of date, if it is.
    fn banner(&self, peer_name: &str, cx: &App) -> Option<String> {
        let team = team_share(cx)?;
        let team = team.read(cx);
        match team.peer(&self.host) {
            Some(peer) => match &peer.status {
                PeerStatus::Online => {
                    let listed = peer
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.id == self.workspace_id);
                    (!listed).then(|| format!("{peer_name} stopped sharing this workspace"))
                }
                PeerStatus::Connecting => Some(format!("Connecting to {peer_name}")),
                PeerStatus::Failed(TeamClientError::Unreachable(_)) => {
                    Some(format!("{peer_name} is offline"))
                }
                PeerStatus::Failed(TeamClientError::Forbidden) => {
                    Some(format!("{peer_name} hasn't added you to their allow list"))
                }
                PeerStatus::Failed(TeamClientError::Incompatible { .. }) => {
                    Some(format!("{peer_name} is on a different Mnemonic version"))
                }
                PeerStatus::Failed(
                    TeamClientError::Other(message)
                    | TeamClientError::Refused {
                        reason: message, ..
                    },
                ) => {
                    Some(format!("Can't reach {peer_name}: {message}"))
                }
            },
            None => Some(format!("{peer_name} is offline")),
        }
    }

    fn render_header(&self, peer_name: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let shared = self.last_known.as_ref();
        let colors = cx.theme().colors();
        h_flex()
            .w_full()
            .min_w_0()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .when_some(shared, |row, shared| {
                row.child(render_agent_state(&shared.id, shared.agent_state, cx))
            })
            .child(
                Icon::new(IconName::UserGroup)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(peer_name.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(shared, |row, shared| {
                row.child(Label::new("/").size(LabelSize::Small).color(Color::Muted))
                    .child(
                        Label::new(shared.name.clone())
                            .size(LabelSize::Small)
                            .single_line(),
                    )
                    .child(
                        Label::new(shared.branch.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .single_line()
                            .truncate(),
                    )
                    .when_some(shared.pr_state.as_deref(), |row, state| {
                        let label = match shared.pr_number {
                            Some(number) => {
                                format!("#{number} {}", mnemonic_bridge::pr_state_label(state))
                            }
                            None => mnemonic_bridge::pr_state_label(state).to_string(),
                        };
                        let color = mnemonic_bridge::pr_state_color(state, cx)
                            .map(Color::Custom)
                            .unwrap_or(Color::Muted);
                        row.child(Label::new(label).size(LabelSize::XSmall).color(color))
                    })
                    .when(shared.additions > 0 || shared.deletions > 0, |row| {
                        row.child(
                            Label::new(format!("+{}", shared.additions))
                                .size(LabelSize::XSmall)
                                .color(Color::Created),
                        )
                        .child(
                            Label::new(format!("-{}", shared.deletions))
                                .size(LabelSize::XSmall)
                                .color(Color::Deleted),
                        )
                    })
            })
    }
}

fn show_notice(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    if let Some(workspace) = workspace.upgrade() {
        show_step_in_notice(&workspace, message, cx);
    }
}

impl Focusable for TeammateWorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.pane.focus_handle(cx)
    }
}

impl Render for TeammateWorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let peer_name = self.peer_name(cx);
        let banner = self.banner(&peer_name, cx);
        let body = if self.chats.is_empty()
            && self.dismissed.is_empty()
            && self.diff_items.is_empty()
        {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .p_4()
                .child(
                    Label::new(format!(
                        "{peer_name} hasn't started a chat in this workspace"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        } else {
            self.pane.clone().into_any_element()
        };
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(&peer_name, cx))
            .when_some(banner, |column, banner| {
                column.child(
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_1()
                        .gap_1p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .bg(cx.theme().status().warning_background)
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(Label::new(banner).size(LabelSize::Small)),
                )
            })
            .child(div().flex_1().min_h_0().child(body))
    }
}
