//! A read-only view, shown as a tab in the Team window, following one of a teammate's workspaces: its agent chat
//! as it happens, and its diff on request.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement, ListAlignment,
    ListState, Render, SharedString, Subscription, Task, Window, list, px,
};
use language::{Buffer, LanguageRegistry};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use mnemonic_core::services::team_share_service::{
    MAX_CHAT_WAIT, TeamChatUpdate, TeamClient, TeamClientError, TeamEntry, TeamToolStatus,
    TeamWorkspace,
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;

use crate::team::{PeerState, PeerStatus, TeamShare, one_line_preview, render_agent_state};

/// How long to wait before asking again for a chat that isn't open or a
/// teammate that couldn't be reached.
const RETRY_CHAT_AFTER: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Chat,
    Changes,
}

enum ChatStatus {
    NoChats,
    Loading,
    Live,
    NotOpen,
    Failed(SharedString),
}

enum DiffStatus {
    NotLoaded,
    Loading,
    Loaded,
    Failed(SharedString),
}

enum TranscriptEntry {
    User(Entity<Markdown>),
    Assistant {
        markdown: Entity<Markdown>,
        thinking: Option<String>,
    },
    Tool {
        title: String,
        status: TeamToolStatus,
        description: Option<String>,
        output_preview: Option<String>,
    },
    Plan(Entity<Markdown>),
    Compaction,
    Unknown,
}

impl TranscriptEntry {
    fn new(entry: TeamEntry, languages: &Arc<LanguageRegistry>, cx: &mut App) -> Self {
        let markdown = |text: String, cx: &mut App| {
            cx.new(|cx| Markdown::new(text.into(), Some(languages.clone()), None, cx))
        };
        match entry {
            TeamEntry::User { markdown: text } => Self::User(markdown(text, cx)),
            TeamEntry::Assistant {
                markdown: text,
                thinking,
            } => Self::Assistant {
                markdown: markdown(text, cx),
                thinking,
            },
            TeamEntry::ToolCall {
                title,
                status,
                description,
                output_preview,
                ..
            } => Self::Tool {
                title,
                status,
                description,
                output_preview,
            },
            TeamEntry::Plan { markdown: text } => Self::Plan(markdown(text, cx)),
            TeamEntry::Compaction { .. } => Self::Compaction,
            TeamEntry::Unknown => Self::Unknown,
        }
    }

    /// Updates a streaming message's text in place, keeping its parsed
    /// markdown. Returns false when the entry has to be rebuilt.
    fn update(&mut self, entry: &TeamEntry, cx: &mut App) -> bool {
        match (self, entry) {
            (Self::User(markdown), TeamEntry::User { markdown: text }) => {
                markdown.update(cx, |markdown, cx| markdown.reset(text.clone().into(), cx));
                true
            }
            (
                Self::Assistant {
                    markdown,
                    thinking: current_thinking,
                },
                TeamEntry::Assistant {
                    markdown: text,
                    thinking,
                },
            ) => {
                markdown.update(cx, |markdown, cx| markdown.reset(text.clone().into(), cx));
                current_thinking.clone_from(thinking);
                true
            }
            (Self::Plan(markdown), TeamEntry::Plan { markdown: text }) => {
                markdown.update(cx, |markdown, cx| markdown.reset(text.clone().into(), cx));
                true
            }
            _ => false,
        }
    }
}

/// Stops the polling thread once the chat stops being followed.
struct ChatPoll {
    base_url: String,
    stop: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for ChatPoll {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub(crate) struct TeamWorkspaceView {
    host: String,
    workspace_id: String,
    team: Entity<TeamShare>,
    languages: Arc<LanguageRegistry>,
    focus_handle: FocusHandle,
    mode: Mode,
    selected_chat: Option<String>,
    transcript: Vec<TranscriptEntry>,
    list_state: ListState,
    chat_status: ChatStatus,
    chat_poll: Option<ChatPoll>,
    diff_editor: Option<Entity<Editor>>,
    diff_status: DiffStatus,
    /// The workspace as last seen, kept while the teammate is unreachable.
    last_known: Option<TeamWorkspace>,
    _team_observation: Subscription,
}

impl TeamWorkspaceView {
    pub(crate) fn new(
        team: Entity<TeamShare>,
        host: String,
        workspace_id: String,
        languages: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&team, |this, _, cx| {
            if let Some(shared) = this.live_workspace(cx).cloned() {
                this.last_known = Some(shared);
            }
            let address_changed = this.chat_poll.as_ref().is_some_and(|poll| {
                this.client(cx)
                    .is_none_or(|client| client.base_url() != poll.base_url)
            });
            if address_changed {
                this.chat_poll = None;
            }
            this.follow_selected_chat(cx);
            cx.notify();
        });
        let mut view = Self {
            host,
            workspace_id,
            team,
            languages,
            focus_handle: cx.focus_handle(),
            mode: Mode::Chat,
            selected_chat: None,
            transcript: Vec::new(),
            list_state: ListState::new(0, ListAlignment::Bottom, px(1000.)),
            chat_status: ChatStatus::Loading,
            chat_poll: None,
            diff_editor: None,
            diff_status: DiffStatus::NotLoaded,
            last_known: None,
            _team_observation: observation,
        };
        view.last_known = view.live_workspace(cx).cloned();
        view.follow_selected_chat(cx);
        view
    }

    fn peer<'a>(&self, cx: &'a App) -> Option<&'a PeerState> {
        self.team.read(cx).peer(&self.host)
    }

    fn live_workspace<'a>(&self, cx: &'a App) -> Option<&'a TeamWorkspace> {
        self.peer(cx)?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.workspace_id)
    }

    fn shared_workspace<'a>(&'a self, cx: &'a App) -> Option<&'a TeamWorkspace> {
        self.live_workspace(cx).or(self.last_known.as_ref())
    }

    /// Picks a chat when none is selected (preferring one that's open on the
    /// teammate's machine) and starts following it.
    fn follow_selected_chat(&mut self, cx: &mut Context<Self>) {
        let Some(shared) = self.live_workspace(cx) else {
            // Keeps the current state while the teammate is briefly
            // unreachable; the next successful poll brings the list back.
            return;
        };
        let still_listed = self
            .selected_chat
            .as_ref()
            .is_some_and(|id| shared.chats.iter().any(|chat| &chat.id == id));
        if still_listed {
            if self.chat_poll.is_none() {
                if let Some(chat_id) = self.selected_chat.clone() {
                    self.start_chat_poll(chat_id, cx);
                }
            }
            return;
        }
        let chosen = shared
            .chats
            .iter()
            .find(|chat| chat.open)
            .or_else(|| shared.chats.first())
            .map(|chat| chat.id.clone());
        match chosen {
            Some(chat_id) => self.select_chat(chat_id, cx),
            None => {
                self.selected_chat = None;
                self.chat_poll = None;
                self.clear_transcript();
                self.chat_status = ChatStatus::NoChats;
            }
        }
    }

    fn select_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.selected_chat.as_ref() == Some(&chat_id) && self.chat_poll.is_some() {
            return;
        }
        self.selected_chat = Some(chat_id.clone());
        self.start_chat_poll(chat_id, cx);
        cx.notify();
    }

    fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.list_state.reset(0);
    }

    fn client(&self, cx: &App) -> Option<TeamClient> {
        self.peer(cx)?.client.clone()
    }

    fn start_chat_poll(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_poll = None;
        self.clear_transcript();
        let Some(client) = self.client(cx) else {
            self.chat_status = ChatStatus::Failed("Can't reach this teammate".into());
            return;
        };
        self.chat_status = ChatStatus::Loading;
        let base_url = client.base_url().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        // Long-polls block for up to half a minute, so they get their own
        // thread rather than holding one of the shared background threads.
        let spawned = std::thread::Builder::new()
            .name("mnemonic-team-chat".into())
            .spawn({
                let stop = stop.clone();
                move || poll_chat(&client, &chat_id, &stop, &sender)
            });
        if let Err(err) = spawned {
            self.chat_status =
                ChatStatus::Failed(format!("Couldn't follow the chat: {err}").into());
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.apply_chat_message(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.chat_poll = Some(ChatPoll {
            base_url,
            stop,
            _task: task,
        });
    }

    fn apply_chat_message(
        &mut self,
        message: Result<Option<TeamChatUpdate>, TeamClientError>,
        cx: &mut Context<Self>,
    ) {
        match message {
            Ok(Some(update)) => {
                self.chat_status = ChatStatus::Live;
                self.apply_update(update, cx);
            }
            Ok(None) => {
                self.chat_status = ChatStatus::NotOpen;
                self.clear_transcript();
            }
            Err(err) => {
                self.chat_status = ChatStatus::Failed(err.to_string().into());
            }
        }
        cx.notify();
    }

    fn apply_update(&mut self, update: TeamChatUpdate, cx: &mut Context<Self>) {
        if update.reset {
            self.transcript.clear();
        }
        let old_len = self.transcript.len();
        let mut changed_in_place = Vec::new();
        for change in update.changes {
            if change.index < self.transcript.len() {
                if !self.transcript[change.index].update(&change.entry, cx) {
                    self.transcript[change.index] =
                        TranscriptEntry::new(change.entry, &self.languages, cx);
                }
                changed_in_place.push(change.index);
            } else if change.index == self.transcript.len() {
                self.transcript
                    .push(TranscriptEntry::new(change.entry, &self.languages, cx));
            } else {
                log::warn!(
                    "Teammate chat skipped from {} to entry {}",
                    self.transcript.len(),
                    change.index
                );
            }
        }
        self.transcript.truncate(update.len);

        if update.reset {
            self.list_state.reset(self.transcript.len());
            return;
        }
        let new_len = self.transcript.len();
        for index in changed_in_place {
            if index < old_len.min(new_len) {
                self.list_state.splice(index..index + 1, 1);
            }
        }
        if new_len > old_len {
            self.list_state.splice(old_len..old_len, new_len - old_len);
        } else if new_len < old_len {
            self.list_state.splice(new_len..old_len, 0);
        }
    }

    fn show_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Changes;
        if matches!(self.diff_status, DiffStatus::NotLoaded) {
            self.load_diff(window, cx);
        }
        cx.notify();
    }

    fn load_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            self.diff_status = DiffStatus::Failed("Can't reach this teammate".into());
            return;
        };
        self.diff_status = DiffStatus::Loading;
        let workspace_id = self.workspace_id.clone();
        let languages = self.languages.clone();
        cx.spawn_in(window, async move |this, cx| {
            let diff = cx
                .background_spawn(async move { client.diff(&workspace_id) })
                .await;
            let language = languages.language_for_name("Diff").await.log_err();
            this.update_in(cx, |this, window, cx| {
                match diff {
                    Ok(Some(text)) if text.trim().is_empty() => {
                        this.diff_editor = None;
                        this.diff_status = DiffStatus::Loaded;
                    }
                    Ok(Some(text)) => {
                        let buffer = cx.new(|cx| {
                            let mut buffer = Buffer::local(text, cx);
                            buffer.set_language(language, cx);
                            buffer
                        });
                        this.diff_editor = Some(cx.new(|cx| {
                            let mut editor = Editor::for_buffer(buffer, None, window, cx);
                            editor.set_read_only(true);
                            editor
                        }));
                        this.diff_status = DiffStatus::Loaded;
                    }
                    Ok(None) => {
                        this.diff_status =
                            DiffStatus::Failed("This workspace is no longer shared".into());
                    }
                    Err(err) => {
                        this.diff_status = DiffStatus::Failed(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let peer_name = self
            .peer(cx)
            .map(|peer| peer.peer.name.clone())
            .unwrap_or_else(|| self.host.clone());
        let shared = self.shared_workspace(cx).cloned();
        let offline = self.peer(cx).and_then(|peer| match &peer.status {
            PeerStatus::Failed(err) => Some(err.to_string()),
            PeerStatus::Connecting | PeerStatus::Online => None,
        });
        let colors = cx.theme().colors();
        let mode = self.mode;
        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .when_some(shared.as_ref(), |row, shared| {
                        row.child(render_agent_state(&shared.id, shared.agent_state, cx))
                    })
                    .child(
                        Label::new(peer_name)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when_some(offline, |row, offline| {
                        row.child(
                            Label::new(offline)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .child(Label::new("/").size(LabelSize::Small).color(Color::Muted))
                    .child(
                        Label::new(
                            shared
                                .as_ref()
                                .map(|shared| shared.name.clone())
                                .unwrap_or_else(|| "Not shared any more".to_string()),
                        )
                        .size(LabelSize::Small)
                        .single_line(),
                    )
                    .when_some(shared.as_ref(), |row, shared| {
                        row.child(
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
                        .when(
                            shared.additions > 0 || shared.deletions > 0,
                            |row| {
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
                            },
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("team-mode-chat", "Chat")
                            .label_size(LabelSize::Small)
                            .toggle_state(mode == Mode::Chat)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mode = Mode::Chat;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("team-mode-changes", "Changes")
                            .label_size(LabelSize::Small)
                            .toggle_state(mode == Mode::Changes)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_changes(window, cx);
                            })),
                    )
                    .when(mode == Mode::Changes, |row| {
                        row.child(
                            IconButton::new("team-refresh-diff", IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .disabled(matches!(self.diff_status, DiffStatus::Loading))
                                .tooltip(Tooltip::text("Refresh the diff"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.load_diff(window, cx);
                                })),
                        )
                    }),
            )
    }

    fn render_chat_picker(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let shared = self.shared_workspace(cx)?;
        if shared.chats.len() < 2 {
            return None;
        }
        let mut row = h_flex().px_3().py_1().gap_1().flex_wrap();
        for chat in &shared.chats {
            let chat_id = chat.id.clone();
            let label = if chat.open {
                format!("{} · {}", chat.title, chat.agent)
            } else {
                format!("{} · {} (not open)", chat.title, chat.agent)
            };
            row = row.child(
                Button::new(SharedString::from(format!("team-chat-{}", chat.id)), label)
                    .label_size(LabelSize::Small)
                    .color(if chat.open {
                        Color::Default
                    } else {
                        Color::Muted
                    })
                    .toggle_state(self.selected_chat.as_ref() == Some(&chat.id))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_chat(chat_id.clone(), cx);
                    })),
            );
        }
        Some(row)
    }

    fn render_chat(&self, cx: &mut Context<Self>) -> AnyElement {
        let peer_name = self
            .peer(cx)
            .map(|peer| peer.peer.name.clone())
            .unwrap_or_else(|| self.host.clone());
        let message = match &self.chat_status {
            ChatStatus::NoChats => Some(format!(
                "{peer_name} hasn't started a chat in this workspace"
            )),
            ChatStatus::Loading => Some("Loading the chat".to_string()),
            ChatStatus::NotOpen => Some(format!(
                "This chat isn't open on {peer_name}'s machine. It appears here once they open it."
            )),
            ChatStatus::Failed(err) if self.transcript.is_empty() => Some(err.to_string()),
            ChatStatus::Failed(_) | ChatStatus::Live => None,
        };
        if let Some(message) = message {
            return render_placeholder(message);
        }
        let reconnecting = match &self.chat_status {
            ChatStatus::Failed(err) => Some(format!("Reconnecting: {err}")),
            _ => None,
        };
        v_flex()
            .size_full()
            .when_some(reconnecting, |column, message| {
                column.child(
                    div().px_3().py_1().child(
                        Label::new(message)
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    ),
                )
            })
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, index: usize, window, cx| {
                        this.render_entry(index, window, cx)
                    }),
                )
                .flex_1()
                .size_full(),
            )
            .into_any_element()
    }

    fn render_entry(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.transcript.get(index) else {
            return div().into_any_element();
        };
        let markdown_style = || MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        let colors = cx.theme().colors();
        let body = match entry {
            TranscriptEntry::User(markdown) => div()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .bg(colors.editor_background)
                .child(MarkdownElement::new(markdown.clone(), markdown_style()))
                .into_any_element(),
            TranscriptEntry::Assistant { markdown, thinking } => v_flex()
                .gap_1()
                .when_some(thinking.as_deref(), |column, thinking| {
                    column.child(render_activity_row(
                        IconName::ToolThink,
                        Color::Muted,
                        "Thinking".to_string(),
                        Some(one_line_preview(thinking)),
                        cx,
                    ))
                })
                .when(!markdown.read(cx).source().is_empty(), |column| {
                    column.child(MarkdownElement::new(markdown.clone(), markdown_style()))
                })
                .into_any_element(),
            TranscriptEntry::Tool {
                title,
                status,
                description,
                output_preview,
            } => {
                let (icon, color) = match status {
                    TeamToolStatus::Pending => (IconName::Circle, Color::Muted),
                    TeamToolStatus::WaitingForApproval => (IconName::Info, Color::Accent),
                    TeamToolStatus::Running => (IconName::ArrowCircle, Color::Muted),
                    TeamToolStatus::Done => (IconName::Check, Color::Muted),
                    TeamToolStatus::Failed => (IconName::XCircle, Color::Error),
                    TeamToolStatus::Rejected | TeamToolStatus::Canceled => {
                        (IconName::Close, Color::Muted)
                    }
                };
                let (label, chip) = match description {
                    Some(description) => (description.clone(), Some(title.clone())),
                    None => (title.clone(), None),
                };
                let label = if *status == TeamToolStatus::WaitingForApproval {
                    format!("Waiting for permission: {label}")
                } else {
                    label
                };
                v_flex()
                    .child(render_activity_row(icon, color, label, chip, cx))
                    .when_some(output_preview.clone(), |column, preview| {
                        column.child(render_activity_row(
                            IconName::XCircle,
                            Color::Error,
                            "Error".to_string(),
                            Some(preview),
                            cx,
                        ))
                    })
                    .into_any_element()
            }
            TranscriptEntry::Plan(markdown) => v_flex()
                .gap_1()
                .child(
                    Label::new("Plan")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(MarkdownElement::new(markdown.clone(), markdown_style()))
                .into_any_element(),
            TranscriptEntry::Compaction => render_activity_row(
                IconName::ListTodo,
                Color::Muted,
                "Context compacted".to_string(),
                None,
                cx,
            )
            .into_any_element(),
            TranscriptEntry::Unknown => div().into_any_element(),
        };
        div().px_3().py_1().child(body).into_any_element()
    }

    fn render_changes(&self) -> AnyElement {
        match &self.diff_status {
            DiffStatus::NotLoaded | DiffStatus::Loading => {
                render_placeholder("Loading the diff".to_string())
            }
            DiffStatus::Failed(err) => render_placeholder(err.to_string()),
            DiffStatus::Loaded => match &self.diff_editor {
                Some(editor) => div().size_full().child(editor.clone()).into_any_element(),
                None => render_placeholder("No changes yet".to_string()),
            },
        }
    }
}

fn poll_chat(
    client: &TeamClient,
    chat_id: &str,
    stop: &AtomicBool,
    sender: &futures::channel::mpsc::UnboundedSender<
        Result<Option<TeamChatUpdate>, TeamClientError>,
    >,
) {
    let mut cursor: Option<String> = None;
    while !stop.load(Ordering::SeqCst) {
        let result = client.chat(chat_id, cursor.as_deref(), MAX_CHAT_WAIT);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let pause = match &result {
            Ok(Some(update)) => {
                cursor = Some(update.cursor.clone());
                false
            }
            Ok(None) | Err(_) => {
                cursor = None;
                true
            }
        };
        if sender.unbounded_send(result).is_err() {
            break;
        }
        if pause {
            let mut waited = Duration::ZERO;
            while waited < RETRY_CHAT_AFTER && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                waited += Duration::from_millis(250);
            }
        }
    }
}

fn render_placeholder(message: String) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .p_4()
        .child(
            Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .into_any_element()
}

/// A one-line row like the agent panel's: an icon, a label and an optional
/// excerpt in a chip.
fn render_activity_row(
    icon: IconName,
    color: Color,
    label: String,
    chip: Option<String>,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .overflow_hidden()
        .child(Icon::new(icon).size(IconSize::Small).color(color))
        .child(
            div()
                .flex_none()
                .max_w(relative(0.6))
                .overflow_hidden()
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line()
                        .truncate(),
                ),
        )
        .when_some(chip.filter(|chip| !chip.is_empty()), |row, chip| {
            row.child(
                div().min_w_0().flex_1().overflow_hidden().child(
                    div()
                        .min_w_0()
                        .max_w_full()
                        .px_1p5()
                        .rounded_sm()
                        .bg(cx.theme().colors().element_background)
                        .overflow_hidden()
                        .child(
                            Label::new(chip)
                                .buffer_font(cx)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        ),
                ),
            )
        })
}

impl Focusable for TeamWorkspaceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TeamWorkspaceView {
    pub(crate) fn matches(&self, host: &str, workspace_id: &str) -> bool {
        self.host == host && self.workspace_id == workspace_id
    }

    pub(crate) fn title(&self, cx: &App) -> SharedString {
        let peer_name = self
            .peer(cx)
            .map(|peer| peer.peer.name.clone())
            .unwrap_or_else(|| self.host.clone());
        match self.shared_workspace(cx) {
            Some(shared) => format!("{peer_name} / {}", shared.name).into(),
            None => peer_name.into(),
        }
    }

    pub(crate) fn tooltip(&self, cx: &App) -> Option<SharedString> {
        let shared = self.shared_workspace(cx)?;
        Some(format!("{} · {} (read-only)", shared.project, shared.branch).into())
    }
}

impl Render for TeamWorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.mode {
            Mode::Chat => self.render_chat(cx),
            Mode::Changes => self.render_changes(),
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .when(self.mode == Mode::Chat, |column| {
                column.children(self.render_chat_picker(cx))
            })
            .child(div().flex_1().min_h_0().child(body))
    }
}
