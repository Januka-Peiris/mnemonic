use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::terminal_service;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentModelProvider {
    Claude,
    Codex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentModel {
    pub id: String,
    pub name: String,
}

pub fn discover_models(
    provider: AgentModelProvider,
    directory: Option<&Path>,
) -> Result<Vec<AgentModel>, String> {
    let mut command = match provider {
        AgentModelProvider::Claude => {
            let mut command = Command::new("claude");
            // Only initialize the SDK control channel. No user message, hooks,
            // MCP servers or saved transcript are needed to list models.
            command.args([
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--settings",
                r#"{"disableAllHooks":true}"#,
            ]);
            command.env_remove("CLAUDECODE");
            command
        }
        AgentModelProvider::Codex => {
            let mut command = Command::new("codex");
            command.arg("app-server");
            command
        }
    };
    command.env("PATH", terminal_service::enriched_path());
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    discover_with_command(&mut command, provider, Duration::from_secs(20))
}

fn discover_with_command(
    command: &mut Command,
    provider: AgentModelProvider,
    timeout: Duration,
) -> Result<Vec<AgentModel>, String> {
    let mut process = MetadataProcess::spawn(command, timeout)?;
    let mut models = Vec::new();
    match provider {
        AgentModelProvider::Claude => {
            process.send(json!({
                "type": "control_request", "request_id": "models",
                "request": {"subtype": "initialize"}
            }))?;
            loop {
                let message = process.receive()?;
                if message["type"] != "control_response"
                    || message["response"]["request_id"] != "models"
                {
                    continue;
                }
                let response = &message["response"];
                if response["subtype"] != "success" {
                    return Err(format!(
                        "Claude model discovery failed: {}",
                        response["error"]
                    ));
                }
                let entries = response["response"]["models"]
                    .as_array()
                    .ok_or("Claude did not return a model list")?;
                for entry in entries {
                    add_model(&mut models, entry, "value")?;
                }
                break;
            }
        }
        AgentModelProvider::Codex => {
            process.send(json!({
                "id": 1, "method": "initialize",
                "params": {"clientInfo": {"name": "mnemonic", "version": env!("CARGO_PKG_VERSION")}}
            }))?;
            process.rpc_response(1)?;
            process.send(json!({"method": "initialized"}))?;
            let mut cursor = None;
            let mut seen_cursors = HashSet::new();
            let mut request_id = 2;
            loop {
                process.send(json!({
                    "id": request_id, "method": "model/list",
                    "params": {"includeHidden": false, "cursor": cursor}
                }))?;
                let response = process.rpc_response(request_id)?;
                let entries = response["data"]
                    .as_array()
                    .ok_or("Codex did not return a model list")?;
                for entry in entries {
                    if entry["hidden"].as_bool() != Some(true) {
                        add_model(&mut models, entry, "model")?;
                    }
                }
                cursor = response["nextCursor"].as_str().map(str::to_string);
                let Some(next_cursor) = cursor.as_ref() else {
                    break;
                };
                if !seen_cursors.insert(next_cursor.clone()) {
                    return Err("Codex repeated a model list cursor".to_string());
                }
                request_id += 1;
            }
        }
    }
    if models.is_empty() {
        return Err(
            "The agent returned no available models. Check its sign-in and configuration."
                .to_string(),
        );
    }
    Ok(models)
}

fn add_model(models: &mut Vec<AgentModel>, entry: &Value, id_field: &str) -> Result<(), String> {
    let id = entry[id_field]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or("The agent returned a model without an identifier")?;
    if !models.iter().any(|model| model.id == id) {
        models.push(AgentModel {
            id: id.to_string(),
            name: entry["displayName"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(id)
                .to_string(),
        });
    }
    Ok(())
}

struct MetadataProcess {
    child: Child,
    messages: Receiver<Result<Value, String>>,
    deadline: Instant,
}

impl MetadataProcess {
    fn spawn(command: &mut Command, timeout: Duration) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Isolate discovery so timeout cleanup also stops descendants
            // retaining the stdout pipe, without touching an agent session.
            command.process_group(0);
        }
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not start agent model discovery: {error}"))?;
        let (sender, messages) = mpsc::sync_channel(16);
        let mut process = Self {
            child,
            messages,
            deadline: Instant::now() + timeout,
        };
        let output = process
            .child
            .stdout
            .take()
            .ok_or("Model discovery stdout is unavailable")?;
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let message = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line)
                        .map_err(|error| format!("Invalid model discovery response: {error}"))
                });
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Ok(process)
    }

    fn send(&mut self, message: Value) -> Result<(), String> {
        let input = self
            .child
            .stdin
            .as_mut()
            .ok_or("Model discovery stdin is unavailable")?;
        writeln!(input, "{message}")
            .and_then(|()| input.flush())
            .map_err(|error| format!("Could not request agent models: {error}"))
    }

    fn receive(&self) -> Result<Value, String> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or("Agent model discovery timed out")?;
        self.messages.recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => "Agent model discovery timed out".to_string(),
                mpsc::RecvTimeoutError::Disconnected =>
                    "The agent exited before returning its models. Check its sign-in and configuration.".to_string(),
            })?
    }

    fn rpc_response(&self, request_id: u64) -> Result<Value, String> {
        loop {
            let message = self.receive()?;
            if message["id"].as_u64() != Some(request_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("Codex model discovery failed: {error}"));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| "Codex returned no model discovery result".to_string());
        }
    }
}

impl Drop for MetadataProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let result = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    log::warn!("Could not stop model discovery process group: {error}");
                }
            }
        }
        #[cfg(not(unix))]
        if let Err(error) = self.child.kill() {
            log::warn!("Could not stop model discovery: {error}");
        }
        if let Err(error) = self.child.wait() {
            log::warn!("Could not reap model discovery: {error}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn peer(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn codex_discovers_new_models_across_pages_without_starting_a_turn() {
        let mut command = peer(
            r#"
            read -r initialize
            case "$initialize" in *'"method":"initialize"'*) ;; *) exit 1;; esac
            printf '%s\n' '{"method":"startup/notification"}' '{"id":1,"result":{}}'
            read -r initialized
            case "$initialized" in *'"method":"initialized"'*) ;; *) exit 2;; esac
            read -r first_page
            case "$first_page" in *'"method":"model/list"'*) ;; *) exit 3;; esac
            printf '%s\n' '{"id":2,"result":{"data":[{"id":"picker-id","model":"future-sol","displayName":"Future Sol"},{"model":"hidden","displayName":"Hidden","hidden":true}],"nextCursor":"page-2"}}'
            read -r second_page
            case "$second_page" in *'"cursor":"page-2"'*) ;; *) exit 4;; esac
            printf '%s\n' '{"id":3,"result":{"data":[{"model":"future-astra","displayName":"Future Astra"}],"nextCursor":null}}'
            read -r unexpected
            exit 5
        "#,
        );
        let models = discover_with_command(
            &mut command,
            AgentModelProvider::Codex,
            Duration::from_secs(2),
        )
        .expect("metadata discovery failed");
        assert_eq!(
            models,
            vec![
                AgentModel {
                    id: "future-sol".into(),
                    name: "Future Sol".into()
                },
                AgentModel {
                    id: "future-astra".into(),
                    name: "Future Astra".into()
                },
            ]
        );
    }

    #[test]
    fn claude_discovers_new_cli_model_values_without_sending_a_prompt() {
        let mut command = peer(
            r#"
            read -r initialize
            case "$initialize" in *'"subtype":"initialize"'*) ;; *) exit 1;; esac
            case "$initialize" in *'"type":"control_request"'*) ;; *) exit 1;; esac
            printf '%s\n' '{"type":"system","subtype":"status"}' '{"type":"control_response","response":{"subtype":"success","request_id":"models","response":{"models":[{"value":"fable","displayName":"Future Fable"},{"value":"claude-future-sonnet","displayName":"Future Sonnet"}]}}}'
            read -r unexpected
            exit 2
        "#,
        );
        assert_eq!(
            discover_with_command(
                &mut command,
                AgentModelProvider::Claude,
                Duration::from_secs(2)
            )
            .expect("metadata discovery failed"),
            vec![
                AgentModel {
                    id: "fable".into(),
                    name: "Future Fable".into()
                },
                AgentModel {
                    id: "claude-future-sonnet".into(),
                    name: "Future Sonnet".into()
                },
            ]
        );
    }

    #[test]
    fn discovery_reports_agent_errors_and_reaps_a_stalled_peer() {
        let mut command = peer(
            r#"
            read -r initialize
            printf '%s\n' '{"type":"control_response","response":{"subtype":"error","request_id":"models","error":"Sign in required"}}'
            read -r unexpected
        "#,
        );
        let error = discover_with_command(
            &mut command,
            AgentModelProvider::Claude,
            Duration::from_secs(2),
        )
        .expect_err("authentication error must not become an empty model list");
        assert!(error.contains("Sign in required"), "{error}");

        let directory = tempfile::tempdir().expect("temporary directory");
        let pid_file = directory.path().join("pid");
        let mut command = peer(r#"echo $$ > "$PID_FILE"; read -r initialize; read -r never_sent"#);
        command.env("PID_FILE", &pid_file);
        let error = discover_with_command(
            &mut command,
            AgentModelProvider::Claude,
            Duration::from_millis(200),
        )
        .expect_err("stalled metadata discovery must time out");
        assert!(error.contains("timed out"), "{error}");
        let pid: i32 = std::fs::read_to_string(pid_file)
            .expect("peer pid")
            .trim()
            .parse()
            .expect("pid number");
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "metadata child is still running"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
