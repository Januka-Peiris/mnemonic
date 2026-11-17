use mnemonic_core::services::team_share_service::DEFAULT_TEAM_PORT;
use settings::{RegisterSetting, Settings};

const DEFAULT_NOTIFY_AFTER_IDLE_MINUTES: u32 = 5;

/// The `phone` section of the settings file.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub(crate) struct PhoneSettings {
    pub enabled: bool,
    pub port: u16,
    pub ntfy_url: String,
    pub ntfy_token: Option<String>,
    pub notify_after_idle_minutes: u32,
}

impl Settings for PhoneSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let phone = content.phone.clone().unwrap_or_default();
        Self {
            enabled: phone.enabled.unwrap_or(false),
            port: phone.port.unwrap_or(DEFAULT_TEAM_PORT + 1),
            ntfy_url: phone.ntfy_url.unwrap_or_default().trim().to_string(),
            ntfy_token: phone
                .ntfy_token
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty()),
            notify_after_idle_minutes: phone
                .notify_after_idle_minutes
                .unwrap_or(DEFAULT_NOTIFY_AFTER_IDLE_MINUTES),
        }
    }
}
