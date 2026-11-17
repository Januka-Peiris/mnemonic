//! Phone alerts through ntfy, sent while the Mac has been left alone.

use std::time::Duration;

use agent_client_protocol::schema as acp;

const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    Permission,
    Finished,
    Error,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub kind: NoticeKind,
    pub workspace_name: String,
    pub chat_id: String,
    pub chat_title: String,
    pub tool_kind: Option<acp::ToolKind>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NtfyRequest {
    pub url: String,
    pub title: String,
    pub body: String,
    pub click: String,
    pub token: Option<String>,
}

/// The alert's words never include what the agent said or ran, only what
/// kind of thing happened, since ntfy topics may be on a public server.
pub fn ntfy_request(
    ntfy_url: &str,
    token: Option<&str>,
    page_url: &str,
    notice: &Notice,
) -> NtfyRequest {
    let body = match notice.kind {
        NoticeKind::Permission => {
            let action = match notice.tool_kind {
                Some(acp::ToolKind::Execute) => "run a command",
                Some(acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move) => {
                    "change files"
                }
                Some(acp::ToolKind::Read | acp::ToolKind::Search) => "read files",
                Some(acp::ToolKind::Fetch) => "fetch from the web",
                _ => "use a tool",
            };
            format!("needs permission to {action}")
        }
        NoticeKind::Finished => format!("finished: {}", notice.chat_title),
        NoticeKind::Error => format!("hit an error in {}", notice.chat_title),
    };
    NtfyRequest {
        url: ntfy_url.to_string(),
        title: notice.workspace_name.clone(),
        body,
        click: format!("{page_url}#chat={}", notice.chat_id),
        token: token.map(str::to_string),
    }
}

/// Published as JSON because header values can't carry non-ASCII workspace
/// names.
fn build_request(
    client: &reqwest::blocking::Client,
    request: &NtfyRequest,
) -> Result<reqwest::blocking::RequestBuilder, String> {
    let (server, topic) = request
        .url
        .trim_end_matches('/')
        .rsplit_once('/')
        .filter(|(server, topic)| !topic.is_empty() && !server.ends_with('/'))
        .ok_or_else(|| {
            "The ntfy address needs a topic, like https://ntfy.sh/my-topic".to_string()
        })?;
    let mut builder = client.post(server).json(&serde_json::json!({
        "topic": topic,
        "title": request.title,
        "message": request.body,
        "click": request.click,
    }));
    if let Some(token) = &request.token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    Ok(builder)
}

pub fn send(request: &NtfyRequest) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(SEND_TIMEOUT)
        .build()
        .map_err(|err| format!("Couldn't set up the ntfy request: {err}"))?;
    let response = build_request(&client, request)?
        .send()
        .map_err(|err| format!("Couldn't reach ntfy: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("ntfy answered {}", response.status()));
    }
    Ok(())
}

pub fn parse_hid_idle_seconds(ioreg_output: &str) -> Option<u64> {
    let line = ioreg_output
        .lines()
        .find(|line| line.contains("\"HIDIdleTime\""))?;
    let (_, value) = line.split_once('=')?;
    let nanoseconds: u64 = value.trim().parse().ok()?;
    Some(nanoseconds / 1_000_000_000)
}

// Only ever called from a background task, never the UI thread.
#[allow(clippy::disallowed_methods)]
pub fn mac_idle_seconds() -> Option<u64> {
    let output = std::process::Command::new("ioreg")
        .args(["-c", "IOHIDSystem"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_hid_idle_seconds(&String::from_utf8_lossy(&output.stdout))
}

pub fn should_notify(
    kind: &NoticeKind,
    idle_seconds: Option<u64>,
    idle_threshold_minutes: u32,
    stop_reason: Option<acp::StopReason>,
) -> bool {
    if *kind == NoticeKind::Finished && stop_reason == Some(acp::StopReason::Cancelled) {
        return false;
    }
    idle_seconds.is_some_and(|idle| idle >= u64::from(idle_threshold_minutes) * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(kind: NoticeKind) -> Notice {
        Notice {
            kind,
            workspace_name: "mnemonic".into(),
            chat_id: "chat-1".into(),
            chat_title: "Deploy".into(),
            tool_kind: Some(acp::ToolKind::Execute),
        }
    }

    #[test]
    fn permission_notice_has_no_tool_text() {
        let request = ntfy_request(
            "https://ntfy.sh/topic",
            None,
            "http://jay-mbp.ts.net:47232/",
            &notice(NoticeKind::Permission),
        );
        assert_eq!(request.body, "needs permission to run a command");
        assert_eq!(request.click, "http://jay-mbp.ts.net:47232/#chat=chat-1");
    }

    #[test]
    fn quiet_while_the_mac_is_in_use() {
        let finished = NoticeKind::Finished;
        let end_turn = Some(acp::StopReason::EndTurn);
        assert!(!should_notify(&finished, Some(60), 5, end_turn));
        assert!(should_notify(&finished, Some(301), 5, end_turn));
        assert!(!should_notify(&finished, None, 5, end_turn));
    }

    #[test]
    fn quiet_for_a_cancelled_turn() {
        assert!(!should_notify(
            &NoticeKind::Finished,
            Some(9999),
            5,
            Some(acp::StopReason::Cancelled)
        ));
    }

    #[test]
    fn reads_hid_idle_time() {
        assert_eq!(
            parse_hid_idle_seconds("    |   \"HIDIdleTime\" = 312000000000\n"),
            Some(312)
        );
    }

    #[test]
    fn non_ascii_workspace_name_can_be_published() {
        let request = NtfyRequest {
            url: "https://ntfy.sh/topic".into(),
            title: "caf\u{e9} \u{1f680}".into(),
            body: "finished: Deploy".into(),
            click: "http://mac.ts.net:47232/#chat=chat-1".into(),
            token: Some("tk".into()),
        };
        let client = reqwest::blocking::Client::new();
        let built = build_request(&client, &request).unwrap().build().unwrap();
        assert_eq!(built.url().as_str(), "https://ntfy.sh/");
        let body: serde_json::Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["topic"], "topic");
        assert_eq!(body["title"], "caf\u{e9} \u{1f680}");
        assert_eq!(body["message"], "finished: Deploy");
        assert_eq!(body["click"], "http://mac.ts.net:47232/#chat=chat-1");
    }

    #[test]
    fn send_reports_an_unreachable_server() {
        let request = NtfyRequest {
            url: "http://127.0.0.1:1/topic".into(),
            title: "t".into(),
            body: "b".into(),
            click: "c".into(),
            token: None,
        };
        assert!(send(&request).is_err());
    }
}
