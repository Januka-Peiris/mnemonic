use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;

use super::team_files_service::safe_relative_path;

const DEFAULT_READ_LIMIT: usize = 2000;
const GLOB_RESULT_LIMIT: usize = 100;
const BASH_OUTPUT_LIMIT: usize = 30000;
const BASH_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const BASH_MAX_TIMEOUT: Duration = Duration::from_secs(600);
const BASH_DRAIN_GRACE: Duration = Duration::from_secs(2);
const READ_LINE_LIMIT: usize = 2000;
const BASH_STREAM_BYTE_LIMIT: usize = BASH_OUTPUT_LIMIT * 4;

#[derive(Clone, Debug)]
pub struct GitAuthor {
    pub name: String,
    pub email: String,
}

pub type WriteGuard = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct AgentToolContext {
    pub root: PathBuf,
    pub git_author: Option<GitAuthor>,
    pub can_write: WriteGuard,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GrepOutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[derive(Clone, Debug, Default)]
pub struct GrepRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub output_mode: GrepOutputMode,
    pub case_insensitive: bool,
    pub line_numbers: bool,
    pub head_limit: Option<usize>,
}

#[derive(Clone, Default)]
pub struct RunningCommands {
    process_groups: Arc<Mutex<HashMap<String, i32>>>,
}

impl RunningCommands {
    pub fn kill(&self, call_key: &str) -> bool {
        let process_group = self
            .process_groups
            .lock()
            .ok()
            .and_then(|groups| groups.get(call_key).copied());
        match process_group {
            Some(process_group) => {
                kill_process_group(process_group);
                true
            }
            None => false,
        }
    }

    fn register(&self, call_key: &str, process_group: i32) {
        if let Ok(mut groups) = self.process_groups.lock() {
            groups.insert(call_key.to_string(), process_group);
        }
    }

    fn unregister(&self, call_key: &str) {
        if let Ok(mut groups) = self.process_groups.lock() {
            groups.remove(call_key);
        }
    }
}

fn kill_process_group(process_group: i32) {
    // SAFETY: killpg only sends a signal; the id came from a child we spawned.
    unsafe {
        libc::killpg(process_group, libc::SIGKILL);
    }
}

pub fn resolve_agent_path(root: &Path, path: &str, for_write: bool) -> Result<PathBuf, String> {
    let relative = if Path::new(path).is_absolute() {
        let canonical_root = root.canonicalize().ok();
        Path::new(path)
            .strip_prefix(root)
            .ok()
            .or_else(|| {
                canonical_root
                    .as_deref()
                    .and_then(|canonical| Path::new(path).strip_prefix(canonical).ok())
            })
            .ok_or_else(|| format!("{path} is outside the workspace"))?
            .to_string_lossy()
            .into_owned()
    } else {
        path.to_string()
    };
    let relative = if relative.is_empty() {
        ".".to_string()
    } else {
        relative
    };

    if for_write {
        return safe_relative_path(root, &relative);
    }

    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("could not open the workspace: {error}"))?;
    let canonical = canonical_root
        .join(&relative)
        .canonicalize()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!("{path} doesn't exist")
            } else {
                format!("could not find {path}: {error}")
            }
        })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(format!("{path} is outside the workspace"));
    }
    Ok(canonical)
}

pub fn read(
    context: &AgentToolContext,
    file_path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, String> {
    let resolved = resolve_agent_path(&context.root, file_path, false)?;
    let bytes = std::fs::read(&resolved).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("{file_path} doesn't exist")
        } else {
            format!("could not read {file_path}: {error}")
        }
    })?;
    if bytes.contains(&0) {
        return Err(format!("{file_path} is a binary file"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
    let lines: Vec<String> = text
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(limit)
        .map(|(index, line)| {
            let line: String = line.chars().take(READ_LINE_LIMIT).collect();
            format!("{}\t{line}", index + 1)
        })
        .collect();
    Ok(lines.join("\n"))
}

pub fn write(context: &AgentToolContext, file_path: &str, content: &str) -> Result<String, String> {
    let resolved = resolve_agent_path(&context.root, file_path, true)?;
    (context.can_write)(&resolved)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create folders for {file_path}: {error}"))?;
    }
    std::fs::write(&resolved, content)
        .map_err(|error| format!("could not write {file_path}: {error}"))?;
    Ok(format!("Wrote {file_path}"))
}

pub fn edit(
    context: &AgentToolContext,
    file_path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string is empty".to_string());
    }
    let resolved = resolve_agent_path(&context.root, file_path, true)?;
    (context.can_write)(&resolved)?;
    let text = std::fs::read_to_string(&resolved)
        .map_err(|error| format!("could not read {file_path}: {error}"))?;
    let matches = text.matches(old_string).count();
    if matches == 0 {
        return Err(format!("old_string was not found in {file_path}"));
    }
    if matches > 1 && !replace_all {
        return Err(format!(
            "old_string has {matches} matches in {file_path}; add more context or set replace_all"
        ));
    }
    let updated = if replace_all {
        text.replace(old_string, new_string)
    } else {
        text.replacen(old_string, new_string, 1)
    };
    std::fs::write(&resolved, updated)
        .map_err(|error| format!("could not write {file_path}: {error}"))?;
    Ok(format!("Edited {file_path} ({matches} replaced)"))
}

fn walk_files(base: &Path) -> impl Iterator<Item = PathBuf> {
    WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
}

fn relative_to_root(context: &AgentToolContext, path: &Path) -> String {
    let canonical_root = context
        .root
        .canonicalize()
        .unwrap_or_else(|_| context.root.clone());
    path.strip_prefix(&canonical_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

pub fn glob(
    context: &AgentToolContext,
    pattern: &str,
    path: Option<&str>,
) -> Result<String, String> {
    let base = resolve_agent_path(&context.root, path.unwrap_or("."), false)?;
    let matcher = Glob::new(pattern)
        .map_err(|error| format!("invalid glob {pattern}: {error}"))?
        .compile_matcher();
    let mut found: Vec<(std::time::SystemTime, String)> = walk_files(&base)
        .filter(|file| {
            file.strip_prefix(&base)
                .is_ok_and(|relative| matcher.is_match(relative))
        })
        .map(|file| {
            let modified = std::fs::metadata(&file)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, relative_to_root(context, &file))
        })
        .collect();
    found.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    found.truncate(GLOB_RESULT_LIMIT);
    if found.is_empty() {
        return Ok("No files found".to_string());
    }
    Ok(found
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn grep(context: &AgentToolContext, request: &GrepRequest) -> Result<String, String> {
    let base = resolve_agent_path(&context.root, request.path.as_deref().unwrap_or("."), false)?;
    let regex = RegexBuilder::new(&request.pattern)
        .case_insensitive(request.case_insensitive)
        .build()
        .map_err(|error| format!("invalid pattern: {error}"))?;
    let file_filter = request
        .glob
        .as_deref()
        .map(|pattern| {
            Glob::new(pattern)
                .map(|glob| glob.compile_matcher())
                .map_err(|error| format!("invalid glob {pattern}: {error}"))
        })
        .transpose()?;

    let mut lines = Vec::new();
    for file in walk_files(&base) {
        if let Some(filter) = &file_filter {
            let relative = file.strip_prefix(&base).unwrap_or(&file);
            let name_matches = file
                .file_name()
                .is_some_and(|name| filter.is_match(Path::new(name)));
            if !filter.is_match(relative) && !name_matches {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let display = relative_to_root(context, &file);
        match request.output_mode {
            GrepOutputMode::FilesWithMatches => {
                if text.lines().any(|line| regex.is_match(line)) {
                    lines.push(display);
                }
            }
            GrepOutputMode::Count => {
                let count = text.lines().filter(|line| regex.is_match(line)).count();
                if count > 0 {
                    lines.push(format!("{display}:{count}"));
                }
            }
            GrepOutputMode::Content => {
                for (index, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        if request.line_numbers {
                            lines.push(format!("{display}:{}:{line}", index + 1));
                        } else {
                            lines.push(format!("{display}:{line}"));
                        }
                    }
                }
            }
        }
        if request.head_limit.is_some_and(|limit| lines.len() >= limit) {
            break;
        }
    }
    if let Some(limit) = request.head_limit {
        lines.truncate(limit);
    }
    if lines.is_empty() {
        return Ok("No matches found".to_string());
    }
    Ok(lines.join("\n"))
}

struct StreamOutput {
    index: usize,
    bytes: Vec<u8>,
    cut: bool,
}

fn drain<R: std::io::Read + Send + 'static>(
    mut stream: R,
    index: usize,
    sender: std::sync::mpsc::Sender<StreamOutput>,
) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut cut = false;
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let room = BASH_STREAM_BYTE_LIMIT.saturating_sub(bytes.len());
                    if count > room {
                        cut = true;
                    }
                    bytes.extend_from_slice(&chunk[..count.min(room)]);
                }
            }
        }
        sender.send(StreamOutput { index, bytes, cut }).ok();
    });
}

#[allow(clippy::disallowed_methods)]
pub fn bash(
    context: &AgentToolContext,
    command: &str,
    timeout: Option<Duration>,
    running: &RunningCommands,
    call_key: &str,
) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;

    let timeout = timeout
        .unwrap_or(BASH_DEFAULT_TIMEOUT)
        .min(BASH_MAX_TIMEOUT);
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let mut process = std::process::Command::new(shell);
    process
        .args(["-l", "-c", command])
        .current_dir(&context.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(author) = &context.git_author {
        process
            .env("GIT_AUTHOR_NAME", &author.name)
            .env("GIT_AUTHOR_EMAIL", &author.email);
    }
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not start the shell: {error}"))?;
    let process_group = child.id() as i32;
    running.register(call_key, process_group);

    let (sender, receiver) = std::sync::mpsc::channel();
    let mut expected_streams = 0;
    if let Some(stdout) = child.stdout.take() {
        drain(stdout, 0, sender.clone());
        expected_streams += 1;
    }
    if let Some(stderr) = child.stderr.take() {
        drain(stderr, 1, sender.clone());
        expected_streams += 1;
    }
    drop(sender);

    let started = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(format!("could not wait for the command: {error}")),
        }
        if started.elapsed() >= timeout {
            kill_process_group(process_group);
            child.wait().ok();
            running.unregister(call_key);
            return Err(format!(
                "command timed out after {:.1}s and was killed",
                timeout.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    running.unregister(call_key);
    // Leftover background children can hold the pipes open past the shell's exit.
    kill_process_group(process_group);

    let status = outcome?;
    // A daemonized grandchild outside the process group can keep a pipe open, so wait only briefly.
    let deadline = Instant::now() + BASH_DRAIN_GRACE;
    let mut streams = Vec::new();
    while streams.len() < expected_streams {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(stream) => streams.push(stream),
            Err(_) => break,
        }
    }
    streams.sort_by_key(|stream| stream.index);
    let mut output = Vec::new();
    let mut output_cut = streams.len() < expected_streams;
    for stream in streams {
        output_cut |= stream.cut;
        output.extend(stream.bytes);
    }
    let output = String::from_utf8_lossy(&output).into_owned();

    use std::os::unix::process::ExitStatusExt as _;
    if let Some(signal) = status.signal() {
        return Err(format!("command was killed by signal {signal}\n{output}"));
    }
    let code = status.code().unwrap_or(-1);
    let mut result = format!("exit {code}\n{output}");
    if output_cut || result.chars().count() > BASH_OUTPUT_LIMIT {
        result = result.chars().take(BASH_OUTPUT_LIMIT).collect();
        result.push_str("\n[output truncated]");
    }
    Ok(result)
}

pub const AGENT_MCP_SERVER_NAME: &str = "teammate_workspace";

const MCP_DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

pub enum McpReply {
    Json(String),
    Accepted,
}

fn mcp_tool_definitions() -> serde_json::Value {
    use serde_json::json;
    json!([
        {
            "name": "Read",
            "description": "Read a file in the shared workspace. file_path is an absolute path inside the shared workspace. Lines are numbered.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute path of the file in the shared workspace"},
                    "offset": {"type": "integer", "description": "Line number to start reading from"},
                    "limit": {"type": "integer", "description": "Number of lines to read"}
                },
                "required": ["file_path"]
            }
        },
        {
            "name": "Write",
            "description": "Write a file in the shared workspace, replacing it if it exists. file_path is an absolute path inside the shared workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute path of the file in the shared workspace"},
                    "content": {"type": "string", "description": "Full content to write"}
                },
                "required": ["file_path", "content"]
            }
        },
        {
            "name": "Edit",
            "description": "Replace text in a file in the shared workspace. file_path is an absolute path inside the shared workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Absolute path of the file in the shared workspace"},
                    "old_string": {"type": "string", "description": "Text to replace"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"}
                },
                "required": ["file_path", "old_string", "new_string"]
            }
        },
        {
            "name": "Glob",
            "description": "Find files by glob pattern in the shared workspace. path is an absolute directory path in the shared workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, for example **/*.rs"},
                    "path": {"type": "string", "description": "Absolute directory to search in"}
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "Grep",
            "description": "Search file contents with a regex in the shared workspace. path is an absolute path in the shared workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for"},
                    "path": {"type": "string", "description": "Absolute file or directory to search in"},
                    "glob": {"type": "string", "description": "Glob filter for files, for example *.rs"},
                    "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                    "-i": {"type": "boolean", "description": "Case insensitive search"},
                    "-n": {"type": "boolean", "description": "Show line numbers in content mode"},
                    "head_limit": {"type": "integer", "description": "Limit the number of results"}
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "Bash",
            "description": "Run a shell command in the root of the shared workspace on the sharer's Mac. The timeout is in milliseconds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout": {"type": "integer", "description": "Timeout in milliseconds"},
                    "description": {"type": "string", "description": "What the command does"}
                },
                "required": ["command"]
            }
        }
    ])
}

fn mcp_error_reply(id: serde_json::Value, code: i64, message: &str) -> McpReply {
    McpReply::Json(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        })
        .to_string(),
    )
}

fn mcp_result_reply(id: serde_json::Value, result: serde_json::Value) -> McpReply {
    McpReply::Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

fn mcp_request_id_text(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn mcp_required_str<'a>(arguments: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("missing or invalid argument: {key}"))
}

fn mcp_optional_usize(arguments: &serde_json::Value, key: &str) -> Option<usize> {
    arguments
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
}

fn mcp_optional_str<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(|value| value.as_str())
}

fn mcp_flag(arguments: &serde_json::Value, key: &str) -> bool {
    arguments
        .get(key)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn mcp_run_tool(
    context: &AgentToolContext,
    running: &RunningCommands,
    call_key: &str,
    name: &str,
    arguments: &serde_json::Value,
) -> Result<String, String> {
    match name {
        "Read" => read(
            context,
            mcp_required_str(arguments, "file_path")?,
            mcp_optional_usize(arguments, "offset"),
            mcp_optional_usize(arguments, "limit"),
        ),
        "Write" => write(
            context,
            mcp_required_str(arguments, "file_path")?,
            mcp_required_str(arguments, "content")?,
        ),
        "Edit" => edit(
            context,
            mcp_required_str(arguments, "file_path")?,
            mcp_required_str(arguments, "old_string")?,
            mcp_required_str(arguments, "new_string")?,
            mcp_flag(arguments, "replace_all"),
        ),
        "Glob" => glob(
            context,
            mcp_required_str(arguments, "pattern")?,
            mcp_optional_str(arguments, "path"),
        ),
        "Grep" => {
            let output_mode = match mcp_optional_str(arguments, "output_mode") {
                None | Some("files_with_matches") => GrepOutputMode::FilesWithMatches,
                Some("content") => GrepOutputMode::Content,
                Some("count") => GrepOutputMode::Count,
                Some(other) => return Err(format!("unknown output_mode: {other}")),
            };
            grep(
                context,
                &GrepRequest {
                    pattern: mcp_required_str(arguments, "pattern")?.to_string(),
                    path: mcp_optional_str(arguments, "path").map(str::to_string),
                    glob: mcp_optional_str(arguments, "glob").map(str::to_string),
                    output_mode,
                    case_insensitive: mcp_flag(arguments, "-i"),
                    line_numbers: mcp_flag(arguments, "-n"),
                    head_limit: mcp_optional_usize(arguments, "head_limit"),
                },
            )
        }
        "Bash" => {
            let timeout = arguments
                .get("timeout")
                .and_then(|value| value.as_u64())
                .map(Duration::from_millis);
            bash(
                context,
                mcp_required_str(arguments, "command")?,
                timeout,
                running,
                call_key,
            )
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// `caller_key` identifies one MCP session of one teammate: JSON-RPC ids
/// restart at 1 in each chat, so a command's cancel key is
/// `{caller_key}:{request id}`.
pub fn handle_mcp_message(
    context: &AgentToolContext,
    running: &RunningCommands,
    caller_key: &str,
    body: &[u8],
) -> McpReply {
    use serde_json::{Value, json};

    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(error) => {
            return mcp_error_reply(Value::Null, -32700, &format!("parse error: {error}"));
        }
    };
    let method = message
        .get("method")
        .and_then(|method| method.as_str())
        .unwrap_or("");
    let Some(id) = message.get("id").cloned() else {
        if method == "notifications/cancelled" {
            if let Some(request_id) = message.pointer("/params/requestId") {
                running.kill(&format!("{caller_key}:{}", mcp_request_id_text(request_id)));
            }
        }
        return McpReply::Accepted;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(|version| version.as_str())
                .unwrap_or(MCP_DEFAULT_PROTOCOL_VERSION);
            mcp_result_reply(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": AGENT_MCP_SERVER_NAME, "version": "1"}
                }),
            )
        }
        "ping" => mcp_result_reply(id, json!({})),
        "tools/list" => mcp_result_reply(id, json!({"tools": mcp_tool_definitions()})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(|name| name.as_str()) else {
                return mcp_error_reply(id, -32602, "tools/call requires a tool name");
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let call_key = format!("{caller_key}:{}", mcp_request_id_text(&id));
            let (text, is_error) = match mcp_run_tool(context, running, &call_key, name, &arguments)
            {
                Ok(text) => (text, false),
                Err(error) => (error, true),
            };
            mcp_result_reply(
                id,
                json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
            )
        }
        other => mcp_error_reply(id, -32601, &format!("method not found: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mnemonic-agent-tools-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        root.canonicalize().expect("canonical temp root")
    }

    fn context(root: &Path) -> AgentToolContext {
        AgentToolContext {
            root: root.to_path_buf(),
            git_author: None,
            can_write: Arc::new(|_| Ok(())),
        }
    }

    #[test]
    fn paths_resolve_inside_the_root_only() {
        let root = temp_root("paths");
        std::fs::write(root.join("a.txt"), "a").expect("write");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub/b.txt"), "b").expect("write");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::write(root.join(".git/config"), "").expect("write");
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("link")).expect("symlink");
        std::os::unix::fs::symlink("/etc", root.join("out")).expect("symlink");

        let absolute = root.join("a.txt");
        assert!(resolve_agent_path(&root, absolute.to_str().expect("utf8"), false).is_ok());
        assert!(resolve_agent_path(&root, "sub/b.txt", false).is_ok());
        assert!(resolve_agent_path(&root, "link", false).is_ok());
        assert!(resolve_agent_path(&root, "link", true).is_err());
        assert!(resolve_agent_path(&root, "out/passwd", false).is_err());
        assert!(resolve_agent_path(&root, "../x", false).is_err());
        assert!(resolve_agent_path(&root, "/etc/passwd", false).is_err());
        assert!(resolve_agent_path(&root, ".git/config", true).is_err());
    }

    #[test]
    fn read_numbers_lines_and_honours_offset_and_limit() {
        let root = temp_root("read");
        std::fs::write(root.join("f.txt"), "one\ntwo\nthree\n").expect("write");
        let context = context(&root);
        assert_eq!(
            read(&context, "f.txt", Some(2), Some(1)).expect("read"),
            "2\ttwo"
        );
        assert_eq!(
            read(&context, "f.txt", None, None).expect("read"),
            "1\tone\n2\ttwo\n3\tthree"
        );
    }

    #[test]
    fn read_cuts_long_lines() {
        let root = temp_root("long-line");
        std::fs::write(root.join("f.txt"), "x".repeat(5000)).expect("write");
        let output = read(&context(&root), "f.txt", None, None).expect("read");
        assert_eq!(output.chars().count(), "1\t".len() + READ_LINE_LIMIT);
    }

    #[test]
    fn bash_caps_output_and_does_not_hang_on_a_detached_holder() {
        let root = temp_root("big-output");
        let started = Instant::now();
        let output = bash(
            &context(&root),
            "head -c 3000000 /dev/zero | tr '\\0' a; perl -e 'use POSIX; fork and exit; POSIX::setsid(); sleep 30'; true",
            None,
            &RunningCommands::default(),
            "k",
        )
        .expect("bash");
        assert!(output.ends_with("[output truncated]"), "{}", output.len());
        assert!(output.chars().count() < BASH_OUTPUT_LIMIT + 100);
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn read_refuses_binary_files() {
        let root = temp_root("binary");
        std::fs::write(root.join("bin"), [1u8, 0, 2]).expect("write");
        assert!(read(&context(&root), "bin", None, None).is_err());
    }

    #[test]
    fn edit_requires_a_unique_match_unless_replace_all() {
        let root = temp_root("edit");
        std::fs::write(root.join("f.txt"), "a a").expect("write");
        let context = context(&root);
        let error = edit(&context, "f.txt", "a", "b", false).expect_err("ambiguous edit");
        assert!(error.contains("2 matches"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "a a"
        );
        edit(&context, "f.txt", "a", "b", true).expect("replace all");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "b b"
        );
    }

    #[test]
    fn write_and_edit_ask_can_write_first() {
        let root = temp_root("can-write");
        std::fs::write(root.join("f.txt"), "original").expect("write");
        let mut context = context(&root);
        context.can_write = Arc::new(|_| Err("has unsaved changes in Jay's editor".to_string()));
        let error = write(&context, "f.txt", "new").expect_err("write refused");
        assert!(error.contains("has unsaved changes in Jay's editor"));
        let error = edit(&context, "f.txt", "original", "new", false).expect_err("edit refused");
        assert!(error.contains("has unsaved changes in Jay's editor"));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).expect("read"),
            "original"
        );
    }

    #[test]
    fn grep_and_glob_skip_gitignored_files() {
        let root = temp_root("ignore");
        std::fs::write(root.join(".gitignore"), "target\n").expect("write");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(root.join("target/x.rs"), "needle").expect("write");
        std::fs::write(root.join("src/y.rs"), "needle").expect("write");
        let context = context(&root);

        let request = GrepRequest {
            pattern: "needle".to_string(),
            ..Default::default()
        };
        assert_eq!(grep(&context, &request).expect("grep"), "src/y.rs");
        assert_eq!(glob(&context, "**/*.rs", None).expect("glob"), "src/y.rs");
    }

    #[test]
    fn bash_runs_in_the_root_with_the_git_author() {
        let root = temp_root("bash");
        let mut context = context(&root);
        context.git_author = Some(GitAuthor {
            name: "Dan".to_string(),
            email: "dan@example.com".to_string(),
        });
        let output = bash(
            &context,
            "pwd && echo $GIT_AUTHOR_NAME",
            None,
            &RunningCommands::default(),
            "k",
        )
        .expect("bash");
        assert!(output.contains(root.to_str().expect("utf8")), "{output}");
        assert!(output.contains("Dan"), "{output}");
    }

    #[test]
    fn bash_reports_a_timeout() {
        let root = temp_root("timeout");
        let error = bash(
            &context(&root),
            "sleep 5",
            Some(Duration::from_millis(200)),
            &RunningCommands::default(),
            "k",
        )
        .expect_err("timeout");
        assert!(error.contains("timed out after 0.2s"), "{error}");
    }

    #[test]
    fn killing_a_running_command_stops_it() {
        let root = temp_root("kill");
        let context = context(&root);
        let running = RunningCommands::default();
        let handle = std::thread::spawn({
            let running = running.clone();
            move || bash(&context, "sleep 30", None, &running, "k")
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(running.kill("k"));
        let started = Instant::now();
        let result = handle.join().expect("thread");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(result.is_err());
    }

    fn mcp_json(
        context: &AgentToolContext,
        running: &RunningCommands,
        body: serde_json::Value,
    ) -> serde_json::Value {
        match handle_mcp_message(context, running, "alice", body.to_string().as_bytes()) {
            McpReply::Json(text) => serde_json::from_str(&text).expect("json reply"),
            McpReply::Accepted => panic!("expected a JSON reply"),
        }
    }

    #[test]
    fn mcp_initialize_echoes_the_protocol_version() {
        let root = temp_root("mcp-init");
        let reply = mcp_json(
            &context(&root),
            &RunningCommands::default(),
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        );
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
        assert_eq!(reply["result"]["serverInfo"]["name"], "teammate_workspace");
    }

    #[test]
    fn mcp_lists_the_six_tools_with_claude_shaped_inputs() {
        let root = temp_root("mcp-list");
        let reply = mcp_json(
            &context(&root),
            &RunningCommands::default(),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = reply["result"]["tools"].as_array().expect("tools");
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]);
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            serde_json::json!(["file_path"])
        );
        assert_eq!(
            tools[2]["inputSchema"]["required"],
            serde_json::json!(["file_path", "old_string", "new_string"])
        );
    }

    #[test]
    fn mcp_tool_call_returns_text_or_an_error_result() {
        let root = temp_root("mcp-call");
        std::fs::write(root.join("a.txt"), "hello\n").expect("write");
        let context = context(&root);
        let running = RunningCommands::default();
        let ok = mcp_json(
            &context,
            &running,
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"Read","arguments":{"file_path": root.join("a.txt").to_string_lossy()}}}),
        );
        assert_eq!(ok["result"]["isError"], false);
        assert!(
            ok["result"]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("1\t")
        );
        let denied = mcp_json(
            &context,
            &running,
            serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"Read","arguments":{"file_path":"/etc/passwd"}}}),
        );
        assert_eq!(denied["result"]["isError"], true);
        assert!(
            denied["result"]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("outside the workspace")
        );
    }

    #[test]
    fn mcp_protocol_problems_are_json_rpc_errors() {
        let root = temp_root("mcp-errors");
        let context = context(&root);
        let running = RunningCommands::default();
        let unknown = mcp_json(
            &context,
            &running,
            serde_json::json!({"jsonrpc":"2.0","id":5,"method":"nope"}),
        );
        assert_eq!(unknown["error"]["code"], -32601);
        match handle_mcp_message(&context, &running, "alice", b"{not json") {
            McpReply::Json(text) => {
                let reply: serde_json::Value = serde_json::from_str(&text).expect("json");
                assert_eq!(reply["error"]["code"], -32700);
            }
            McpReply::Accepted => panic!("expected a JSON reply"),
        }
    }

    #[test]
    fn mcp_notifications_are_accepted_without_a_body() {
        let root = temp_root("mcp-notify");
        let reply = handle_mcp_message(
            &context(&root),
            &RunningCommands::default(),
            "alice",
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert!(matches!(reply, McpReply::Accepted));
    }

    #[test]
    fn mcp_cancel_kills_the_callers_command() {
        let root = temp_root("mcp-cancel");
        let context = context(&root);
        let running = RunningCommands::default();
        let handle = std::thread::spawn({
            let context = context.clone();
            let running = running.clone();
            move || {
                mcp_json(
                    &context,
                    &running,
                    serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"Bash","arguments":{"command":"sleep 30"}}}),
                )
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let reply = handle_mcp_message(
            &context,
            &running,
            "alice",
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
        );
        assert!(matches!(reply, McpReply::Accepted));
        let result = handle.join().expect("thread");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(result["result"]["isError"], true);
    }

    #[test]
    fn mcp_cancel_only_kills_commands_of_the_same_caller_key() {
        let root = temp_root("mcp-cancel-key");
        let context = context(&root);
        let running = RunningCommands::default();
        let handle = std::thread::spawn({
            let context = context.clone();
            let running = running.clone();
            move || {
                let body = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"Bash","arguments":{"command":"sleep 2; echo done"}}});
                match handle_mcp_message(
                    &context,
                    &running,
                    "alice:session-1",
                    body.to_string().as_bytes(),
                ) {
                    McpReply::Json(text) => {
                        serde_json::from_str::<serde_json::Value>(&text).expect("json")
                    }
                    McpReply::Accepted => panic!("expected a JSON reply"),
                }
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        handle_mcp_message(
            &context,
            &running,
            "alice:session-2",
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
        );
        let result = handle.join().expect("thread");
        assert_eq!(result["result"]["isError"], false);
    }
}
