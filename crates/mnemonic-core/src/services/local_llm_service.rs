use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

use crate::models::{
    AgentProfile, LocalLlmModel, LocalLlmProfileDiagnostic, LocalLlmProfileDiagnosticCheck,
};
use crate::services::{command_safety_service, environment_service};

pub fn list_local_llm_models(provider: Option<&str>) -> Result<Vec<LocalLlmModel>, String> {
    let provider = provider.unwrap_or("ollama").trim();
    match provider {
        "" | "ollama" => list_ollama_models(),
        other => Err(format!(
            "Local model discovery is not implemented for provider {other}"
        )),
    }
}

pub fn diagnose_local_llm_profile(profile: AgentProfile) -> LocalLlmProfileDiagnostic {
    let mut checks = Vec::new();
    checks.push(command_check(&profile));
    checks.push(command_safety_check(&profile));
    checks.push(endpoint_check(&profile));
    if let Some(check) = endpoint_reachability_check(&profile) {
        checks.push(check);
    }
    if profile
        .provider
        .as_deref()
        .map(|provider| provider.eq_ignore_ascii_case("ollama"))
        .unwrap_or(false)
    {
        checks.push(ollama_model_check(&profile));
    }

    let status = if checks.iter().any(|check| check.status == "error") {
        "error"
    } else if checks.iter().any(|check| check.status == "warning") {
        "warning"
    } else {
        "ok"
    }
    .to_string();
    let summary = match status.as_str() {
        "ok" => "Profile looks ready to launch.".to_string(),
        "warning" => "Profile is usable but has something to review.".to_string(),
        _ => "Profile needs attention before launch.".to_string(),
    };

    LocalLlmProfileDiagnostic {
        status,
        summary,
        command_preview: command_preview(&profile),
        checks,
    }
}

fn command_check(profile: &AgentProfile) -> LocalLlmProfileDiagnosticCheck {
    if profile.command.trim().is_empty() {
        return diagnostic_check("Command", "error", "No command configured.");
    }
    if profile.command.contains('/') {
        let path = std::path::Path::new(&profile.command);
        if path.is_file() {
            diagnostic_check("Command", "ok", &format!("Found {}", profile.command))
        } else {
            diagnostic_check(
                "Command",
                "error",
                &format!("{} was not found", profile.command),
            )
        }
    } else {
        match environment_service::find_binary(&profile.command) {
            Ok(Some(path)) => diagnostic_check(
                "Command",
                "ok",
                &format!("{} resolved to {}", profile.command, path.display()),
            ),
            Ok(None) => diagnostic_check(
                "Command",
                "error",
                &format!("{} was not found on PATH", profile.command),
            ),
            Err(err) => diagnostic_check("Command", "warning", &err),
        }
    }
}

fn command_safety_check(profile: &AgentProfile) -> LocalLlmProfileDiagnosticCheck {
    let preview = command_preview(profile);
    if command_safety_service::is_risky_command(&preview) {
        diagnostic_check(
            "Command safety",
            "warning",
            &format!("Launch command looks risky: {preview}"),
        )
    } else {
        diagnostic_check(
            "Command safety",
            "ok",
            "Launch command does not match Mnemonic's risky-command patterns.",
        )
    }
}

fn endpoint_check(profile: &AgentProfile) -> LocalLlmProfileDiagnosticCheck {
    let Some(endpoint) = profile
        .endpoint
        .as_deref()
        .filter(|value| !value.is_empty())
    else {
        return diagnostic_check("Endpoint", "warning", "No endpoint metadata configured.");
    };
    if is_local_endpoint(endpoint) {
        diagnostic_check(
            "Endpoint",
            "ok",
            &format!("{endpoint} is marked as local metadata."),
        )
    } else {
        diagnostic_check(
            "Endpoint",
            "warning",
            &format!("{endpoint} does not look like a localhost endpoint."),
        )
    }
}

fn endpoint_reachability_check(profile: &AgentProfile) -> Option<LocalLlmProfileDiagnosticCheck> {
    let endpoint = profile.endpoint.as_deref()?.trim();
    if !is_local_endpoint(endpoint) {
        return None;
    }
    let Some((host, port)) = parse_local_endpoint_host_port(endpoint) else {
        return Some(diagnostic_check(
            "Endpoint reachability",
            "warning",
            "Could not parse local endpoint host/port. This metadata is optional for CLI launch profiles.",
        ));
    };

    let addresses = endpoint_socket_addresses(&host, port);
    if addresses.is_empty() {
        return Some(diagnostic_check(
            "Endpoint reachability",
            "warning",
            &format!(
                "Could not resolve {host}:{port}. This HTTP endpoint is optional for CLI launch profiles."
            ),
        ));
    }

    let mut errors = Vec::new();
    for socket_addr in addresses {
        match TcpStream::connect_timeout(&socket_addr, Duration::from_millis(500)) {
            Ok(_) => {
                return Some(diagnostic_check(
                    "Endpoint reachability",
                    "ok",
                    &format!("{socket_addr} accepted a TCP connection."),
                ));
            }
            Err(err) => errors.push(format!("{socket_addr}: {err}")),
        }
    }

    Some(diagnostic_check(
        "Endpoint reachability",
        "warning",
        &format!(
            "Optional local HTTP endpoint {host}:{port} was not reachable over TCP. Mnemonic can still launch this CLI profile if the command and model checks pass. Last error: {}",
            errors
                .last()
                .cloned()
                .unwrap_or_else(|| "unknown connection failure".to_string())
        ),
    ))
}

fn endpoint_socket_addresses(host: &str, port: u16) -> Vec<SocketAddr> {
    let mut candidates = Vec::new();
    let mut push_addrs = |host: &str| {
        if let Ok(addrs) = (host, port).to_socket_addrs() {
            for addr in addrs {
                if !candidates.contains(&addr) {
                    candidates.push(addr);
                }
            }
        }
    };

    match host {
        "localhost" => {
            push_addrs("127.0.0.1");
            push_addrs("::1");
            push_addrs("localhost");
        }
        "127.0.0.1" => {
            push_addrs("127.0.0.1");
            push_addrs("localhost");
        }
        "::1" => {
            push_addrs("::1");
            push_addrs("localhost");
        }
        other => push_addrs(other),
    }

    candidates
}

fn ollama_model_check(profile: &AgentProfile) -> LocalLlmProfileDiagnosticCheck {
    let Some(model) = profile.model.as_deref().filter(|value| !value.is_empty()) else {
        return diagnostic_check("Ollama model", "warning", "No Ollama model configured.");
    };
    match list_ollama_models() {
        Ok(models) if models.iter().any(|item| item.name == model) => {
            diagnostic_check("Ollama model", "ok", &format!("{model} is installed."))
        }
        Ok(models) => diagnostic_check(
            "Ollama model",
            "warning",
            &format!(
                "{model} was not found in `ollama list` ({} model(s) installed).",
                models.len()
            ),
        ),
        Err(err) => diagnostic_check("Ollama model", "warning", &err),
    }
}

fn diagnostic_check(name: &str, status: &str, message: &str) -> LocalLlmProfileDiagnosticCheck {
    LocalLlmProfileDiagnosticCheck {
        name: name.to_string(),
        status: status.to_string(),
        message: message.to_string(),
    }
}

fn command_preview(profile: &AgentProfile) -> String {
    std::iter::once(profile.command.as_str())
        .chain(profile.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_local_endpoint(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("localhost") || lower.contains("127.0.0.1") || lower.contains("[::1]")
}

fn parse_local_endpoint_host_port(endpoint: &str) -> Option<(String, u16)> {
    let mut rest = endpoint.trim();
    let scheme = rest.split_once("://").map(|(scheme, after)| {
        rest = after;
        scheme.to_ascii_lowercase()
    });
    let default_port = match scheme.as_deref() {
        Some("https") => 443,
        _ => 80,
    };
    let authority = rest.split(['/', '?', '#']).next()?.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(after_bracket) = authority.strip_prefix("[::1]") {
        let port = after_bracket
            .strip_prefix(':')
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default_port);
        return Some(("::1".to_string(), port));
    }
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
        .unwrap_or((authority, default_port));
    let host = host.trim().to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "127.0.0.1") {
        Some((host, port))
    } else {
        None
    }
}

fn list_ollama_models() -> Result<Vec<LocalLlmModel>, String> {
    let binary = environment_service::find_binary("ollama")?
        .ok_or_else(|| "Ollama is not installed or was not found on PATH".to_string())?;
    let output = Command::new(binary)
        .arg("list")
        .output()
        .map_err(|err| format!("Failed to run ollama list: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "ollama list failed. Make sure Ollama is installed and available.".to_string()
        } else {
            format!("ollama list failed: {stderr}")
        });
    }
    Ok(parse_ollama_list(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_ollama_list(text: &str) -> Vec<LocalLlmModel> {
    text.lines()
        .skip_while(|line| line.trim_start().starts_with("NAME"))
        .filter_map(parse_ollama_model_line)
        .collect()
}

fn parse_ollama_model_line(line: &str) -> Option<LocalLlmModel> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    let name = parts[0].to_string();
    let size_index = parts
        .iter()
        .position(|part| matches!(*part, "KB" | "MB" | "GB" | "TB"))
        .and_then(|unit_index| unit_index.checked_sub(1));
    let size = size_index.and_then(|index| {
        parts
            .get(index)
            .zip(parts.get(index + 1))
            .map(|(value, unit)| format!("{value} {unit}"))
    });
    let modified = size_index.and_then(|index| {
        let start = index + 2;
        if start < parts.len() {
            Some(parts[start..].join(" "))
        } else {
            None
        }
    });
    Some(LocalLlmModel {
        provider: "ollama".to_string(),
        name,
        size,
        modified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ollama_list_output() {
        let models = parse_ollama_list(
            "NAME                    ID              SIZE      MODIFIED\nqwen2.5-coder:latest    abc123          4.7 GB    2 days ago\nllama3.2:latest         def456          2.0 GB    1 week ago\n",
        );
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen2.5-coder:latest");
        assert_eq!(models[0].size.as_deref(), Some("4.7 GB"));
        assert_eq!(models[0].modified.as_deref(), Some("2 days ago"));
    }

    #[test]
    fn skips_empty_ollama_lines() {
        assert!(parse_ollama_list("NAME ID SIZE MODIFIED\n\n").is_empty());
    }

    #[test]
    fn diagnoses_missing_command_as_error() {
        let diagnostic = diagnose_local_llm_profile(AgentProfile {
            id: "missing".to_string(),
            label: "Missing".to_string(),
            agent: "local_llm".to_string(),
            command: "definitely-not-a-real-forge-command".to_string(),
            args: vec![],
            model: Some("llama3.2".to_string()),
            reasoning: None,
            mode: Some("act".to_string()),
            provider: Some("ollama".to_string()),
            endpoint: Some("http://localhost:11434".to_string()),
            local: true,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        });

        assert_eq!(diagnostic.status, "error");
        assert!(diagnostic
            .checks
            .iter()
            .any(|check| check.name == "Command" && check.status == "error"));
    }

    #[test]
    fn endpoint_check_warns_for_remote_metadata() {
        let check = endpoint_check(&AgentProfile {
            id: "remote".to_string(),
            label: "Remote".to_string(),
            agent: "local_llm".to_string(),
            command: "ollama".to_string(),
            args: vec![],
            model: None,
            reasoning: None,
            mode: None,
            provider: Some("openai-compatible".to_string()),
            endpoint: Some("https://example.com/v1".to_string()),
            local: true,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        });

        assert_eq!(check.status, "warning");
    }

    #[test]
    fn command_safety_warns_for_risky_profile_launches() {
        let check = command_safety_check(&AgentProfile {
            id: "risky".to_string(),
            label: "Risky".to_string(),
            agent: "local_llm".to_string(),
            command: "rm".to_string(),
            args: vec!["-rf".to_string(), "/tmp/example".to_string()],
            model: None,
            reasoning: None,
            mode: None,
            provider: Some("custom".to_string()),
            endpoint: None,
            local: true,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        });

        assert_eq!(check.status, "warning");
    }

    #[test]
    fn parses_local_endpoint_host_ports() {
        assert_eq!(
            parse_local_endpoint_host_port("http://localhost:11434/v1"),
            Some(("localhost".to_string(), 11434))
        );
        assert_eq!(
            parse_local_endpoint_host_port("http://127.0.0.1:1234"),
            Some(("127.0.0.1".to_string(), 1234))
        );
        assert_eq!(
            parse_local_endpoint_host_port("http://[::1]:8080"),
            Some(("::1".to_string(), 8080))
        );
        assert_eq!(
            parse_local_endpoint_host_port("https://localhost/v1"),
            Some(("localhost".to_string(), 443))
        );
        assert_eq!(parse_local_endpoint_host_port("https://example.com"), None);
    }

    #[test]
    fn localhost_endpoint_addresses_include_ipv4_and_ipv6_loopbacks() {
        let addresses = endpoint_socket_addresses("localhost", 11434);
        assert!(addresses
            .iter()
            .any(|addr| addr.ip().is_loopback() && addr.port() == 11434));
        assert!(!addresses.is_empty());
    }

    #[test]
    fn unreachable_local_endpoint_is_warning_not_error() {
        let profile = AgentProfile {
            id: "endpoint-warning".to_string(),
            label: "Endpoint Warning".to_string(),
            agent: "local_llm".to_string(),
            command: "ollama".to_string(),
            args: vec![],
            model: None,
            reasoning: None,
            mode: None,
            provider: Some("ollama".to_string()),
            endpoint: Some("http://127.0.0.1:9".to_string()),
            local: true,
            description: None,
            skills: vec![],
            templates: vec![],
            role_preference: None,
            coordinator_eligible: None,
        };
        let check = endpoint_reachability_check(&profile).expect("endpoint check");
        assert_eq!(check.status, "warning");
        assert!(check.message.contains("optional") || check.message.contains("Optional"));
    }
}
