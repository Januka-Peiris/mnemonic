//! The Team window: share workspaces one at a time and pick, from the machines
//! on your tailnet, whom to follow and who may follow you. Following and
//! allowing are written straight to the settings file; which workspaces are
//! shared is kept in the core's settings table.

use std::sync::Arc;

use fs::Fs;
use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    IntoElement, Render, SharedString, Stateful, Subscription, Task, TitlebarOptions, Window,
    WindowBounds, WindowOptions, px, size,
};
use language::LanguageRegistry;
use mnemonic_core::services::team_share_service::{TailnetDevice, tailnet_devices};
use mnemonic_core::services::workspace_service;
use settings::{Settings as _, SettingsStore, TeamPeerContent, update_settings_file};
use ui::{Checkbox, Headline, HeadlineSize, Switch, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;

use crate::team::{
    SharingStatus, TeamPeer, TeamSettings, is_workspace_shared, set_workspace_shared, team_share,
};
use crate::team_view::TeamWorkspaceView;

enum Devices {
    Loading,
    Loaded(Vec<TailnetDevice>),
    Failed(SharedString),
}

struct LocalWorkspace {
    id: String,
    project: String,
    name: String,
}

pub(crate) struct TeamSettingsView {
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    devices: Devices,
    workspaces: Vec<LocalWorkspace>,
    _load_devices: Task<()>,
    _load_workspaces: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl TeamSettingsView {
    fn new(fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![cx.observe_global::<SettingsStore>(|_, cx| cx.notify())];
        if let Some(team) = team_share(cx) {
            subscriptions.push(cx.observe(&team, |_, _, cx| cx.notify()));
        }
        Self {
            fs,
            focus_handle: cx.focus_handle(),
            devices: Devices::Loading,
            workspaces: Vec::new(),
            _load_devices: Self::load_devices(cx),
            _load_workspaces: Self::load_workspaces(cx),
            _subscriptions: subscriptions,
        }
    }

    fn load_devices(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let devices = cx.background_spawn(async move { tailnet_devices() }).await;
            this.update(cx, |this, cx| {
                this.devices = match devices {
                    Ok(devices) => Devices::Loaded(devices),
                    Err(err) => Devices::Failed(err.into()),
                };
                cx.notify();
            })
            .ok();
        })
    }

    fn load_workspaces(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                return;
            };
            let workspaces = cx
                .background_spawn(async move { workspace_service::list_workspaces(&state) })
                .await;
            this.update(cx, |this, cx| {
                match workspaces {
                    Ok(workspaces) => {
                        let mut workspaces: Vec<LocalWorkspace> = workspaces
                            .into_iter()
                            .map(|summary| LocalWorkspace {
                                id: summary.id,
                                project: summary.repo,
                                name: summary.name,
                            })
                            .collect();
                        workspaces.sort_by(|a, b| {
                            a.project.cmp(&b.project).then_with(|| a.name.cmp(&b.name))
                        });
                        this.workspaces = workspaces;
                    }
                    Err(err) => log::error!("Couldn't list workspaces for the Team tab: {err}"),
                }
                cx.notify();
            })
            .ok();
        })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.devices = Devices::Loading;
        self._load_devices = Self::load_devices(cx);
        self._load_workspaces = Self::load_workspaces(cx);
        cx.notify();
    }

    fn this_machine(&self) -> Option<&TailnetDevice> {
        match &self.devices {
            Devices::Loaded(devices) => devices.iter().find(|device| device.is_this_machine),
            _ => None,
        }
    }

    fn render_this_computer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shared_count = self
            .workspaces
            .iter()
            .filter(|workspace| is_workspace_shared(&workspace.id, cx))
            .count();
        let (status, color) = match team_share(cx).map(|team| &team.read(cx).sharing) {
            Some(SharingStatus::Starting) => ("Starting to share".to_string(), Color::Muted),
            Some(SharingStatus::On { .. }) => (
                match shared_count {
                    1 => "Sharing 1 workspace".to_string(),
                    count => format!("Sharing {count} workspaces"),
                },
                Color::Success,
            ),
            Some(SharingStatus::Failed(err)) => (
                format!("Not sharing: {err}. Mnemonic keeps retrying."),
                Color::Error,
            ),
            Some(SharingStatus::Off) | None => (
                "Not sharing anything. Share a workspace below to start.".to_string(),
                Color::Muted,
            ),
        };
        let address: AnyElement = match (&self.devices, self.this_machine()) {
            (_, Some(device)) => {
                let host = device.host.clone();
                h_flex()
                    .gap_1()
                    .child(
                        Label::new("Teammates follow you at")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(host.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .child(
                        IconButton::new("team-copy-host", IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Copy"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(host.clone()));
                            }),
                    )
                    .into_any_element()
            }
            (Devices::Failed(err), None) => Label::new(format!(
                "{err}. Turn Tailscale on to share workspaces or follow teammates."
            ))
            .size(LabelSize::Small)
            .color(Color::Warning)
            .into_any_element(),
            _ => Label::new("Looking up Tailscale")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };
        card("This computer", None, cx)
            .child(address)
            .child(Label::new(status).size(LabelSize::Small).color(color))
    }

    fn render_workspaces(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = card(
            "Your workspaces",
            Some(
                "Shared workspaces can be followed, read-only, by the people you allow below: \
                 their agent chats live, and their diffs. Chats can include anything your \
                 agents printed.",
            ),
            cx,
        );
        if self.workspaces.is_empty() {
            return section.child(
                Label::new("No workspaces yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        let mut current_project: Option<&str> = None;
        for workspace in &self.workspaces {
            if current_project != Some(workspace.project.as_str()) {
                current_project = Some(workspace.project.as_str());
                section = section.child(
                    div().pt_1().child(
                        Label::new(workspace.project.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
            }
            let shared = is_workspace_shared(&workspace.id, cx);
            let workspace_id = workspace.id.clone();
            section =
                section.child(
                    h_flex()
                        .w_full()
                        .py_0p5()
                        .gap_2()
                        .child(Icon::new(IconName::UserGroup).size(IconSize::Small).color(
                            if shared {
                                Color::Accent
                            } else {
                                Color::Disabled
                            },
                        ))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(workspace.name.clone())
                                    .size(LabelSize::Small)
                                    .single_line(),
                            ),
                        )
                        .child(
                            Switch::new(
                                SharedString::from(format!("team-share-{}", workspace.id)),
                                if shared {
                                    ToggleState::Selected
                                } else {
                                    ToggleState::Unselected
                                },
                            )
                            .on_click(move |state, _, cx| {
                                set_workspace_shared(
                                    &workspace_id,
                                    *state == ToggleState::Selected,
                                    cx,
                                );
                            }),
                        ),
                );
        }
        section
    }

    fn render_devices(&self, settings: &TeamSettings, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = card(
            "People on your tailnet",
            Some(
                "Follow puts their shared workspaces in your sidebar. Can see mine lets \
                 them follow the workspaces you share.",
            ),
            cx,
        );
        let devices: &[TailnetDevice] = match &self.devices {
            Devices::Loading => {
                return list.child(
                    Label::new("Looking up your tailnet")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            Devices::Failed(err) => {
                list = list.child(
                    Label::new(format!(
                        "{err}. Turn Tailscale on to pick teammates from your tailnet."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                );
                &[]
            }
            Devices::Loaded(devices) => devices,
        };
        let my_login = devices
            .iter()
            .find(|device| device.is_this_machine)
            .map(|device| device.login.to_lowercase());
        for device in devices {
            list =
                list.child(self.render_device(device, devices, settings, my_login.as_deref(), cx));
        }

        // Entries in the settings that match no machine on the tailnet, so
        // they can still be removed here.
        let stray_peers: Vec<&TeamPeer> = settings
            .peers
            .iter()
            .filter(|peer| {
                !devices
                    .iter()
                    .any(|device| peer_is_device(&peer.host, device))
            })
            .collect();
        let stray_logins: Vec<&String> = settings
            .allow
            .iter()
            .filter(|login| {
                !devices
                    .iter()
                    .any(|device| device.login.eq_ignore_ascii_case(login.trim()))
            })
            .collect();
        if !stray_peers.is_empty() || !stray_logins.is_empty() {
            list = list.child(
                div().pt_2().child(
                    Label::new("Not on your tailnet right now")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }
        for peer in stray_peers {
            let fs = self.fs.clone();
            let host = peer.host.clone();
            list = list.child(stray_row(
                SharedString::from(format!("team-stray-peer-{}", peer.host)),
                format!("Following {} ({})", peer.name, peer.host),
                move |cx| {
                    let host = host.clone();
                    update_settings_file(fs.clone(), cx, move |content, _| {
                        if let Some(peers) = content.team.get_or_insert_default().peers.as_mut() {
                            peers.retain(|peer| peer.host.as_deref() != Some(host.as_str()));
                        }
                    });
                },
            ));
        }
        for login in stray_logins {
            let fs = self.fs.clone();
            let login = login.clone();
            list = list.child(stray_row(
                SharedString::from(format!("team-stray-login-{login}")),
                format!("{login} can see your workspaces"),
                move |cx| set_allowed(fs.clone(), login.clone(), false, cx),
            ));
        }
        list
    }

    fn render_device(
        &self,
        device: &TailnetDevice,
        devices: &[TailnetDevice],
        settings: &TeamSettings,
        my_login: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_mine = my_login.is_some_and(|login| device.login.eq_ignore_ascii_case(login));
        let following = settings
            .peers
            .iter()
            .any(|peer| peer_is_device(&peer.host, device));
        let allowed = settings
            .allow
            .iter()
            .any(|login| login.trim().eq_ignore_ascii_case(&device.login));
        let owner_device_count = devices
            .iter()
            .filter(|other| other.login.eq_ignore_ascii_case(&device.login))
            .count();
        // A teammate's one machine is named after them; when they have
        // several (or it's one of yours) the machine name tells them apart.
        let peer_name = if is_mine {
            device.name.clone()
        } else if owner_device_count > 1 {
            format!("{} ({})", device.owner_name, device.name)
        } else {
            device.owner_name.clone()
        };
        let owner = if device.is_this_machine {
            "This computer".to_string()
        } else if is_mine {
            "You".to_string()
        } else {
            device.owner_name.clone()
        };

        let follow_toggle = {
            let fs = self.fs.clone();
            let host = device.host.clone();
            let ips = device.ips.clone();
            let name = device.name.clone();
            Checkbox::new(
                SharedString::from(format!("team-follow-{}", device.host)),
                if following {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                },
            )
            .label("Follow")
            .on_click(move |state, _, cx| {
                let follow = *state == ToggleState::Selected;
                let host = host.clone();
                let ips = ips.clone();
                let name = name.clone();
                let peer_name = peer_name.clone();
                update_settings_file(fs.clone(), cx, move |content, _| {
                    let peers = content
                        .team
                        .get_or_insert_default()
                        .peers
                        .get_or_insert_default();
                    peers.retain(|peer| {
                        !peer
                            .host
                            .as_deref()
                            .is_some_and(|peer_host| host_matches(peer_host, &host, &name, &ips))
                    });
                    if follow {
                        peers.push(TeamPeerContent {
                            name: Some(peer_name),
                            host: Some(host),
                        });
                    }
                });
            })
        };

        let can_see: AnyElement = if is_mine {
            div()
                .id(SharedString::from(format!("team-own-{}", device.host)))
                .child(
                    Label::new("Always sees yours")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .tooltip(Tooltip::text(
                    "Your own machines can always follow this one.",
                ))
                .into_any_element()
        } else {
            let fs = self.fs.clone();
            let login = device.login.clone();
            Checkbox::new(
                SharedString::from(format!("team-allow-{}", device.host)),
                if allowed {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                },
            )
            .label("Can see mine")
            .on_click(move |state, _, cx| {
                set_allowed(
                    fs.clone(),
                    login.clone(),
                    *state == ToggleState::Selected,
                    cx,
                );
            })
            .into_any_element()
        };

        h_flex()
            .w_full()
            .py_1()
            .gap_2()
            .child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(if device.online {
                        cx.theme().status().success
                    } else {
                        cx.theme().colors().text_muted
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(device.name.clone())
                            .size(LabelSize::Small)
                            .single_line(),
                    )
                    .child(
                        Label::new(if device.online {
                            owner
                        } else {
                            format!("{owner}, offline")
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line(),
                    ),
            )
            .when(!device.is_this_machine, |row| row.child(follow_toggle))
            .child(div().w(px(104.)).flex_none().child(can_see))
    }
}

/// Whether a peer's host from the settings names this machine, whether it
/// was written as the full MagicDNS name, the short name or an address,
/// with or without a port.
fn peer_is_device(peer_host: &str, device: &TailnetDevice) -> bool {
    host_matches(peer_host, &device.host, &device.name, &device.ips)
}

fn host_matches(peer_host: &str, host: &str, name: &str, ips: &[String]) -> bool {
    let peer_host = peer_host.trim().trim_end_matches('.');
    let peer_host = match peer_host.rsplit_once(':') {
        Some((without_port, port)) if port.parse::<u16>().is_ok() => without_port,
        _ => peer_host,
    };
    peer_host.eq_ignore_ascii_case(host)
        || peer_host.eq_ignore_ascii_case(name)
        || ips.iter().any(|ip| ip == peer_host)
}

fn set_allowed(fs: Arc<dyn Fs>, login: String, allowed: bool, cx: &App) {
    update_settings_file(fs, cx, move |content, _| {
        let allow = content
            .team
            .get_or_insert_default()
            .allow
            .get_or_insert_default();
        allow.retain(|existing| !existing.trim().eq_ignore_ascii_case(login.trim()));
        if allowed {
            allow.push(login);
        }
    });
}

fn stray_row(id: SharedString, label: String, remove: impl Fn(&App) + 'static) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .child(
            div().flex_1().min_w_0().child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line(),
            ),
        )
        .child(
            Button::new(id, "Remove")
                .label_size(LabelSize::Small)
                .on_click(move |_, _, cx| remove(cx)),
        )
}

/// A titled, bordered section of the Team tab.
fn card(title: &'static str, description: Option<&'static str>, cx: &App) -> Div {
    v_flex()
        .w_full()
        .p_4()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().editor_background)
        .child(Headline::new(title).size(HeadlineSize::XSmall))
        .when_some(description, |card, description| {
            card.child(
                Label::new(description)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        })
}

impl Focusable for TeamSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TeamSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = TeamSettings::get_global(cx).clone();
        div()
            .id("team-settings")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .overflow_y_scroll()
            .child(
                v_flex()
                    .mx_auto()
                    .w_full()
                    .max_w(rems(46.))
                    .px_6()
                    .py_6()
                    .gap_4()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(Headline::new("Team").size(HeadlineSize::Medium))
                                    .child(
                                        Label::new(
                                            "Follow your teammates' agents and share your own \
                                             workspaces, read-only, over Tailscale.",
                                        )
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Button::new("team-refresh", "Refresh")
                                    .start_icon(
                                        Icon::new(IconName::RotateCw)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .label_size(LabelSize::Small)
                                    .disabled(matches!(self.devices, Devices::Loading))
                                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                            ),
                    )
                    .child(self.render_this_computer(cx))
                    .child(self.render_workspaces(cx))
                    .child(self.render_devices(&settings, cx)),
            )
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum TeamWindowTab {
    Settings,
    Teammate { host: String, workspace_id: String },
}

/// The Team window: the Team settings, then one tab per teammate workspace
/// being followed. It lives outside every workspace window so none of this
/// sits among a workspace's own tabs.
pub(crate) struct TeamWindow {
    settings: Entity<TeamSettingsView>,
    teammates: Vec<Entity<TeamWorkspaceView>>,
    /// `None` shows the settings; otherwise the index into `teammates`.
    active_teammate: Option<usize>,
    languages: Arc<LanguageRegistry>,
    _subscriptions: Vec<Subscription>,
}

/// Opens the Team window on `tab`, or brings the open one forward.
pub(crate) fn open_team_window(
    tab: TeamWindowTab,
    fs: Arc<dyn Fs>,
    languages: Arc<LanguageRegistry>,
    cx: &mut App,
) {
    let existing = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<TeamWindow>());
    if let Some(existing) = existing {
        existing
            .update(cx, |team_window, window, cx| {
                window.activate_window();
                team_window.show(tab, cx);
            })
            .log_err();
        return;
    }
    // Deferred so the calling window's update finishes before a new window
    // opens.
    cx.defer(move |cx| {
        let bounds = Bounds::centered(None, size(px(900.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Mnemonic - Team".into()),
                    ..Default::default()
                }),
                focus: true,
                show: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(700.), px(400.))),
                window_background: cx.theme().window_background_appearance(),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|cx| {
                    let settings = cx.new(|cx| TeamSettingsView::new(fs, cx));
                    let mut team_window = TeamWindow {
                        settings,
                        teammates: Vec::new(),
                        active_teammate: None,
                        languages,
                        _subscriptions: Vec::new(),
                    };
                    team_window.show(tab, cx);
                    team_window
                })
            },
        )
        .log_err();
    });
}

impl TeamWindow {
    fn show(&mut self, tab: TeamWindowTab, cx: &mut Context<Self>) {
        match tab {
            TeamWindowTab::Settings => self.active_teammate = None,
            TeamWindowTab::Teammate { host, workspace_id } => {
                let existing = self
                    .teammates
                    .iter()
                    .position(|view| view.read(cx).matches(&host, &workspace_id));
                match existing {
                    Some(index) => self.active_teammate = Some(index),
                    None => {
                        let Some(team) = team_share(cx) else {
                            return;
                        };
                        let languages = self.languages.clone();
                        let view = cx.new(|cx| {
                            TeamWorkspaceView::new(team, host, workspace_id, languages, cx)
                        });
                        self._subscriptions
                            .push(cx.observe(&view, |_, _, cx| cx.notify()));
                        self.teammates.push(view);
                        self.active_teammate = Some(self.teammates.len() - 1);
                    }
                }
            }
        }
        cx.notify();
    }

    fn close_teammate(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.teammates.len() {
            return;
        }
        self.teammates.remove(index);
        self.active_teammate = match self.active_teammate {
            Some(active) if active == index => None,
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        cx.notify();
    }

    fn render_tab(
        &self,
        id: impl Into<ElementId>,
        label: SharedString,
        tooltip: Option<SharedString>,
        selected: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors();
        h_flex()
            .id(id.into())
            .h_full()
            .px_3()
            .gap_1p5()
            .items_center()
            .cursor_pointer()
            .border_b_1()
            .border_color(if selected {
                colors.text_accent
            } else {
                gpui::transparent_black()
            })
            .when_some(tooltip, |tab, tooltip| tab.tooltip(Tooltip::text(tooltip)))
            .child(
                Icon::new(IconName::UserGroup)
                    .size(IconSize::Small)
                    .color(if selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .child(Label::new(label).size(LabelSize::Small).color(if selected {
                Color::Default
            } else {
                Color::Muted
            }))
    }
}

impl Render for TeamWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let mut tabs = h_flex()
            .h(px(32.))
            .flex_none()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().tab_bar_background)
            .child(
                self.render_tab(
                    "team-window-settings",
                    "Team".into(),
                    None,
                    self.active_teammate.is_none(),
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.show(TeamWindowTab::Settings, cx))),
            );
        for (index, view) in self.teammates.iter().enumerate() {
            let view = view.read(cx);
            tabs = tabs.child(
                self.render_tab(
                    ("team-window-teammate", index),
                    view.title(cx),
                    view.tooltip(cx),
                    self.active_teammate == Some(index),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active_teammate = Some(index);
                    cx.notify();
                }))
                .child(
                    IconButton::new(("team-window-close", index), IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Close"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_teammate(index, cx);
                        })),
                ),
            );
        }
        let body = match self
            .active_teammate
            .and_then(|index| self.teammates.get(index))
        {
            Some(view) => view.clone().into_any_element(),
            None => self.settings.clone().into_any_element(),
        };
        v_flex()
            .size_full()
            .font(ui_font)
            .text_color(cx.theme().colors().text)
            .bg(cx.theme().colors().panel_background)
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_written_any_way_matches_its_machine() {
        let ips = vec!["100.64.0.2".to_string()];
        let matches =
            |peer: &str| host_matches(peer, "alice-mbp.tail1234.ts.net", "alice-mbp", &ips);
        assert!(matches("alice-mbp"));
        assert!(matches("Alice-MBP.tail1234.ts.net."));
        assert!(matches("alice-mbp:47231"));
        assert!(matches("100.64.0.2"));
        assert!(!matches("alice-pc"));
    }
}
