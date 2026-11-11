use gpui::{App, Global};
use mnemonic_core::repositories::settings_repository;
use settings::{MergeMethod, RegisterSetting, ReviewAgent, Settings, SettingsStore};

/// The `workspaces` section of the settings file.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WorkspacesSettings {
    pub review_agent: ReviewAgent,
    pub keep_ci_green: bool,
    pub merge_method: MergeMethod,
    pub branch_prefix: String,
    pub default_base_branch: String,
    pub auto_run_setup: bool,
    pub allow_risky_scripts: bool,
    pub clone_location: String,
}

impl Settings for WorkspacesSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let workspaces = content.workspaces.clone().unwrap_or_default();
        Self {
            review_agent: workspaces.review_agent.unwrap_or_default(),
            keep_ci_green: workspaces.keep_ci_green.unwrap_or(false),
            merge_method: workspaces.merge_method.unwrap_or_default(),
            branch_prefix: workspaces.branch_prefix.unwrap_or_default(),
            default_base_branch: workspaces
                .default_base_branch
                .unwrap_or_else(|| "main".to_string()),
            auto_run_setup: workspaces.auto_run_setup.unwrap_or(false),
            allow_risky_scripts: workspaces.allow_risky_scripts.unwrap_or(false),
            clone_location: workspaces
                .clone_location
                .filter(|location| !location.trim().is_empty())
                .unwrap_or_else(|| "~/dev".to_string()),
        }
    }
}

struct MirroredWorkspacesSettings(WorkspacesSettings);

impl Global for MirroredWorkspacesSettings {}

/// Keeps the core's settings table in step with the settings file. The core
/// services read these values from the database, some of them off the main
/// thread where the settings store isn't reachable, so the settings file is
/// the source of truth and the table is a copy of it.
pub fn init(cx: &mut App) {
    mirror_to_database(cx);
    cx.observe_global::<SettingsStore>(mirror_to_database)
        .detach();
}

fn mirror_to_database(cx: &mut App) {
    let settings = WorkspacesSettings::get_global(cx).clone();
    if cx
        .try_global::<MirroredWorkspacesSettings>()
        .is_some_and(|mirrored| mirrored.0 == settings)
    {
        return;
    }

    let merge_method = match settings.merge_method {
        MergeMethod::Squash => "squash",
        MergeMethod::Merge => "merge",
        MergeMethod::Rebase => "rebase",
    };
    let flag = |enabled: bool| if enabled { "true" } else { "false" };
    let values = [
        ("merge_method", merge_method),
        ("branch_prefix", settings.branch_prefix.as_str()),
        ("default_base_branch", settings.default_base_branch.as_str()),
        ("auto_run_setup_enabled", flag(settings.auto_run_setup)),
        (
            "allow_risky_workspace_scripts",
            flag(settings.allow_risky_scripts),
        ),
    ];

    let state = mnemonic_bridge::mnemonic_state(cx);
    let mut mirrored = true;
    for (key, value) in values {
        if let Err(err) = settings_repository::set_value(&state.db, key, value) {
            log::error!("Failed to save workspaces setting {key}: {err}");
            mirrored = false;
        }
    }
    // Left unset after a failed write so the next settings change retries.
    if mirrored {
        cx.set_global(MirroredWorkspacesSettings(settings));
    }
}
