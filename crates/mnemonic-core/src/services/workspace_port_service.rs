use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::models::WorkspacePort;
use crate::repositories::{activity_repository, workspace_repository};
use crate::state::AppState;

const PORT_CACHE_TTL: Duration = Duration::from_secs(15);
static PORT_CACHE: OnceLock<Mutex<HashMap<String, CachedPorts>>> = OnceLock::new();

#[derive(Debug, Clone)]
struct CachedPorts {
    scanned_at: Instant,
    ports: Vec<WorkspacePort>,
}

#[derive(Debug, Clone, Default)]
struct RawListener {
    pid: Option<u32>,
    command: Option<String>,
    user: Option<String>,
    name: Option<String>,
}

pub fn list_workspace_ports(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<WorkspacePort>, String> {
    let cache = PORT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .map_err(|_| "Workspace port cache lock poisoned".to_string())?;
    if let Some(entry) = cache.get(workspace_id) {
        if entry.scanned_at.elapsed() < PORT_CACHE_TTL {
            return Ok(entry.ports.clone());
        }
    }

    let ports = scan_workspace_ports(state, workspace_id)?;
    cache.insert(
        workspace_id.to_string(),
        CachedPorts {
            scanned_at: Instant::now(),
            ports: ports.clone(),
        },
    );
    Ok(ports)
}

fn invalidate_workspace_port_cache(workspace_id: &str) {
    if let Some(cache) = PORT_CACHE.get() {
        if let Ok(mut cache) = cache.lock() {
            cache.remove(workspace_id);
        }
    }
}

fn scan_workspace_ports(
    state: &AppState,
    workspace_id: &str,
) -> Result<Vec<WorkspacePort>, String> {
    let started = Instant::now();
    let root = workspace_root_path(state, workspace_id)?;
    let output = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcLn"])
        .output()
        .map_err(|err| format!("Failed to scan listening ports with lsof: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "lsof failed while scanning listening ports".to_string()
        } else {
            format!("lsof failed while scanning listening ports: {stderr}")
        });
    }

    let cwd_by_pid = process_cwds().unwrap_or_default();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut ports = parse_lsof_listeners(&text)
        .into_iter()
        .filter_map(|listener| listener_to_workspace_port(listener, &root, &cwd_by_pid))
        .filter(|port| port.workspace_matched)
        .collect::<Vec<_>>();
    ports.sort_by_key(|port| port.port);
    ports.dedup_by_key(|port| (port.port, port.pid));
    log::debug!(
        target: "mnemonic_lib",
        "scan_workspace_ports workspace={} ports={} elapsed_ms={}",
        workspace_id,
        ports.len(),
        started.elapsed().as_millis()
    );
    Ok(ports)
}

pub fn open_workspace_port(state: &AppState, workspace_id: &str, port: u16) -> Result<(), String> {
    let _ = workspace_root_path(state, workspace_id)?;
    let url = format!("http://localhost:{port}");
    let output = Command::new("open")
        .arg(&url)
        .output()
        .map_err(|err| format!("Failed to open {url}: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("open returned a non-zero exit code for {url}")
        } else {
            format!("Failed to open {url}: {stderr}")
        })
    }
}

pub fn kill_workspace_port_process(
    state: &AppState,
    workspace_id: &str,
    port: u16,
    pid: u32,
) -> Result<Vec<WorkspacePort>, String> {
    let current = list_workspace_ports(state, workspace_id)?;
    let target = current
        .iter()
        .find(|item| item.port == port && item.pid == pid && item.workspace_matched)
        .cloned();
    let Some(target) = target else {
        return Err(format!(
            "Process {pid} is no longer a verified listener for workspace port {port}"
        ));
    };

    invalidate_workspace_port_cache(workspace_id);
    let output = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .map_err(|err| format!("Failed to kill process {pid}: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("kill returned a non-zero exit code for process {pid}")
        } else {
            format!("Failed to kill process {pid}: {stderr}")
        });
    }

    invalidate_workspace_port_cache(workspace_id);
    if let Ok(Some(workspace)) = workspace_repository::get_detail(&state.db, workspace_id) {
        let details = format_port_kill_activity_details(&target);
        let _ = activity_repository::record(
            &state.db,
            workspace_id,
            &workspace.summary.repo,
            Some(&workspace.summary.branch),
            "Workspace port process killed",
            "warning",
            Some(&details),
        );
    }
    Ok(scan_workspace_ports(state, workspace_id).unwrap_or_default())
}

fn format_port_kill_activity_details(port: &WorkspacePort) -> String {
    let cwd = port.cwd.as_deref().unwrap_or("unknown cwd");
    format!(
        "Sent SIGTERM to pid {} ({}) on localhost:{}; cwd: {}.",
        port.pid, port.command, port.port, cwd
    )
}

fn listener_to_workspace_port(
    listener: RawListener,
    workspace_root: &Path,
    cwd_by_pid: &HashMap<u32, String>,
) -> Option<WorkspacePort> {
    let pid = listener.pid?;
    let name = listener.name.unwrap_or_default();
    let port = parse_port_from_name(&name)?;
    let cwd = cwd_by_pid.get(&pid).cloned();
    let workspace_matched = cwd
        .as_deref()
        .map(|path| path_is_inside_workspace(Path::new(path), workspace_root))
        .unwrap_or(false);
    Some(WorkspacePort {
        port,
        pid,
        command: listener.command.unwrap_or_else(|| "unknown".to_string()),
        user: listener.user,
        protocol: "tcp".to_string(),
        address: name,
        cwd,
        workspace_matched,
    })
}

fn parse_lsof_listeners(text: &str) -> Vec<RawListener> {
    let mut listeners = Vec::new();
    let mut current = RawListener::default();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (field, value) = line.split_at(1);
        match field {
            "p" => {
                if current.pid.is_some() && current.name.is_some() {
                    listeners.push(std::mem::take(&mut current));
                }
                current.pid = value.parse::<u32>().ok();
            }
            "c" => current.command = Some(value.to_string()),
            "L" => current.user = Some(value.to_string()),
            "n" => {
                current.name = Some(value.to_string());
                if current.pid.is_some() {
                    listeners.push(std::mem::take(&mut current));
                }
            }
            _ => {}
        }
    }
    if current.pid.is_some() && current.name.is_some() {
        listeners.push(current);
    }
    listeners
}

fn parse_port_from_name(name: &str) -> Option<u16> {
    let before_space = name.split_whitespace().next().unwrap_or(name);
    let digits = before_space
        .rsplit_once(':')
        .map(|(_, right)| right)
        .unwrap_or(before_space)
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u16>().ok()
}

fn process_cwds() -> Result<HashMap<u32, String>, String> {
    let output = Command::new("lsof")
        .args(["-nP", "-d", "cwd", "-F", "pn"])
        .output()
        .map_err(|err| format!("Failed to inspect process working directories with lsof: {err}"))?;
    if !output.status.success() {
        return Err("Could not inspect process working directories".to_string());
    }
    Ok(parse_lsof_cwds(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_lsof_cwds(text: &str) -> HashMap<u32, String> {
    let mut cwd_by_pid = HashMap::new();
    let mut current_pid: Option<u32> = None;
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (field, value) = line.split_at(1);
        match field {
            "p" => current_pid = value.parse::<u32>().ok(),
            "n" => {
                if let Some(pid) = current_pid {
                    cwd_by_pid.insert(pid, value.to_string());
                }
            }
            _ => {}
        }
    }
    cwd_by_pid
}

fn workspace_root_path(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} was not found"))?;
    let path = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let path = PathBuf::from(path);
    if !path.exists() || !path.is_dir() {
        return Err(format!(
            "Workspace root path is unavailable: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn path_is_inside_workspace(path: &Path, workspace_root: &Path) -> bool {
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    canonical_path == canonical_root || canonical_path.starts_with(canonical_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_field_output() {
        let listeners =
            parse_lsof_listeners("p123\ncnode\nLjay\nn*:5173\np456\ncpython\nn127.0.0.1:8000\n");
        assert_eq!(listeners.len(), 2);
        assert_eq!(listeners[0].pid, Some(123));
        assert_eq!(listeners[0].command.as_deref(), Some("node"));
        assert_eq!(listeners[0].user.as_deref(), Some("jay"));
        assert_eq!(listeners[0].name.as_deref(), Some("*:5173"));
        assert_eq!(
            parse_port_from_name(listeners[1].name.as_deref().unwrap()),
            Some(8000)
        );
    }

    #[test]
    fn parses_lsof_cwd_output() {
        let cwds = parse_lsof_cwds("p123\nn/tmp/app\np456\nn/Users/example/project\n");
        assert_eq!(cwds.get(&123).map(String::as_str), Some("/tmp/app"));
        assert_eq!(
            cwds.get(&456).map(String::as_str),
            Some("/Users/example/project")
        );
    }

    #[test]
    fn parses_common_listener_names() {
        assert_eq!(parse_port_from_name("*:3000"), Some(3000));
        assert_eq!(parse_port_from_name("127.0.0.1:5173"), Some(5173));
        assert_eq!(parse_port_from_name("[::1]:8080"), Some(8080));
        assert_eq!(parse_port_from_name("localhost:notaport"), None);
    }

    #[test]
    fn matches_workspace_ancestry() {
        let root = Path::new("/tmp/forge-workspace");
        assert!(path_is_inside_workspace(
            Path::new("/tmp/forge-workspace"),
            root
        ));
        assert!(path_is_inside_workspace(
            Path::new("/tmp/forge-workspace/app"),
            root
        ));
        assert!(!path_is_inside_workspace(
            Path::new("/tmp/forge-other"),
            root
        ));
    }

    #[test]
    fn formats_port_kill_activity_details() {
        let details = format_port_kill_activity_details(&WorkspacePort {
            port: 5173,
            pid: 12345,
            command: "node".to_string(),
            user: Some("jay".to_string()),
            protocol: "tcp".to_string(),
            address: "*:5173".to_string(),
            cwd: Some("/tmp/forge-workspace".to_string()),
            workspace_matched: true,
        });

        assert!(details.contains("pid 12345"));
        assert!(details.contains("node"));
        assert!(details.contains("localhost:5173"));
        assert!(details.contains("/tmp/forge-workspace"));
    }
}
