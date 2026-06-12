use std::collections::BTreeMap;

use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, SharedString, WeakEntity, Window,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::workspace_service;
use ui::{Tooltip, h_flex, v_flex, Icon, IconButton, IconName, Label, prelude::*};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

actions!(mnemonic_workspace_panel, [ToggleFocus]);

const WORKSPACE_PANEL_KEY: &str = "MnemonicWorkspacePanel";

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    _workspace: WeakEntity<Workspace>,
    workspaces: Vec<WorkspaceSummary>,
    collapsed_projects: Vec<String>,
    selected_workspace_id: Option<String>,
}

impl WorkspacePanel {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                _workspace: workspace.weak_handle(),
                workspaces: Vec::new(),
                collapsed_projects: Vec::new(),
                selected_workspace_id: None,
            };
            panel.refresh_workspaces(cx);
            panel
        })
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn refresh_workspaces(&mut self, cx: &App) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::list_workspaces(&state) {
            Ok(ws) => self.workspaces = ws,
            Err(e) => log::error!("Failed to list workspaces: {e}"),
        }
    }

    fn grouped_workspaces(&self) -> BTreeMap<String, Vec<&WorkspaceSummary>> {
        let mut groups: BTreeMap<String, Vec<&WorkspaceSummary>> = BTreeMap::new();
        for ws in &self.workspaces {
            let project = if ws.repo.is_empty() {
                "ungrouped".to_string()
            } else {
                ws.repo.clone()
            };
            groups.entry(project).or_default().push(ws);
        }
        groups
    }

    fn toggle_project(&mut self, project: &str) {
        if let Some(pos) = self.collapsed_projects.iter().position(|p| p == project) {
            self.collapsed_projects.remove(pos);
        } else {
            self.collapsed_projects.push(project.to_string());
        }
    }

    fn select_workspace(&mut self, ws_id: String, cx: &mut Context<Self>) {
        self.selected_workspace_id = Some(ws_id);
        cx.notify();
    }

    fn render_history_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("history-bar")
            .px_2()
            .h(px(30.))
            .gap_1p5()
            .items_center()
            .w_full()
            .cursor_pointer()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(|_this, _, _window, _cx| {
                log::info!("History clicked");
            }))
            .child(
                Icon::new(IconName::Clock)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("History")
                    .size(LabelSize::Default)
                    .color(Color::Default),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .h(px(28.))
            .justify_between()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Projects")
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("filter-projects", IconName::Filter)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Filter"))
                            .on_click(cx.listener(|_this, _, _window, _cx| {
                                log::info!("Filter clicked");
                            })),
                    )
                    .child(
                        IconButton::new("add-project", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Project"))
                            .on_click(cx.listener(|_this, _, _window, _cx| {
                                log::info!("New project clicked");
                            })),
                    ),
            )
    }

    fn render_project_group(
        &self,
        project: &str,
        workspaces: &[&WorkspaceSummary],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.collapsed_projects.contains(&project.to_string());
        let count = workspaces.len();
        let project_owned = project.to_string();
        let chevron_icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };

        let project_for_add = project.to_string();
        let mut group = v_flex().w_full();

        group = group.child(
            h_flex()
                .id(SharedString::from(format!("project-{project}")))
                .px_2()
                .h(px(28.))
                .gap_1()
                .items_center()
                .w_full()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.toggle_project(&project_owned);
                    cx.notify();
                }))
                .child(
                    Icon::new(chevron_icon)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(project.to_string())
                        .size(LabelSize::Default)
                        .color(Color::Default),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_grow(1.0))
                .child(
                    IconButton::new(
                        SharedString::from(format!("add-worktree-{project}")),
                        IconName::Plus,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("New Worktree"))
                    .on_click(cx.listener(move |_this, _, _window, _cx| {
                        log::info!("Add worktree to {}", project_for_add);
                    })),
                ),
        );

        if !collapsed {
            for ws in workspaces {
                group = group.child(self.render_workspace_row(ws, cx));
            }
        }

        group
    }

    fn render_workspace_row(
        &self,
        ws: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self
            .selected_workspace_id
            .as_ref()
            .is_some_and(|id| id == &ws.id);
        let ws_id = ws.id.clone();

        h_flex()
            .id(SharedString::from(ws.id.clone()))
            .pl(px(28.))
            .pr_2()
            .h(px(24.))
            .gap_1p5()
            .items_center()
            .w_full()
            .cursor_pointer()
            .rounded_md()
            .when(is_selected, |el| {
                el.bg(cx.theme().colors().ghost_element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.select_workspace(ws_id.clone(), cx);
            }))
            .child(
                Icon::new(IconName::FileGit)
                    .size(IconSize::XSmall)
                    .color(if is_selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .child(
                Label::new(ws.name.clone())
                    .size(LabelSize::Default)
                    .color(Color::Default)
                    .single_line(),
            )
    }

    fn render_empty(&self) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Label::new("No projects yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }
}

impl Focusable for WorkspacePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspacePanel {}

impl Panel for WorkspacePanel {
    fn persistent_name() -> &'static str {
        "MnemonicWorkspacePanel"
    }

    fn panel_key() -> &'static str {
        WORKSPACE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Projects")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let count = self.workspaces.len();
        if count > 0 {
            Some(count.to_string())
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        1
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_workspaces(cx);

        let groups = self.grouped_workspaces();
        let has_workspaces = !groups.is_empty();

        let mut content = v_flex()
            .id("workspace-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_history_bar(cx))
            .child(self.render_header(cx));

        if has_workspaces {
            let mut list = v_flex().py_0p5().overflow_y_hidden().flex_grow(1.0);
            for (project, workspaces) in &groups {
                list = list.child(self.render_project_group(project, workspaces, cx));
            }
            content = content.child(list);
        } else {
            content = content.child(self.render_empty());
        }

        content
    }
}
