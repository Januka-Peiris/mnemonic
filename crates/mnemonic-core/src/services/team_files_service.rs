use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::services::team_share_service::TeamCaller;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TeamFileEntry {
    pub path: String,
    pub kind: TeamFileKind,
    pub size: u64,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamFileKind {
    File { executable: bool },
    Symlink { target: String },
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TeamFileManifest {
    pub head_commit: String,
    pub entries: Vec<TeamFileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SkipList {
    pub patterns: Vec<String>,
}

impl Default for SkipList {
    fn default() -> Self {
        let patterns = [
            ".git",
            "target",
            "node_modules",
            ".venv",
            "venv",
            "__pycache__",
            ".next",
            "dist",
            "build",
            ".env",
            ".env.*",
            "*.pem",
            "*.key",
            ".claude/settings.local.json",
            ".DS_Store",
        ];
        Self {
            patterns: patterns.iter().map(|pattern| pattern.to_string()).collect(),
        }
    }
}

/// Always skipped, whatever the editable patterns say, so an edit to the
/// list can never expose secrets.
const SECRET_FLOOR: [&str; 6] = [
    ".git",
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    ".claude/settings.local.json",
];

/// `text` folded the way a case-insensitive disk compares names, so that
/// `ſ` (long s) matches `s` and the Kelvin sign matches `k`.
pub fn fold_case(text: &str) -> String {
    text.to_uppercase().to_lowercase()
}

fn matches_pattern(pattern: &str, path: &str) -> bool {
    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let Some(final_component) = components.last() else {
        return false;
    };
    if pattern.contains('/') {
        pattern == components.join("/")
    } else if pattern.contains('*') {
        glob_matches(pattern, final_component)
    } else {
        components.contains(&pattern)
    }
}

impl SkipList {
    /// The secret floor is matched in any case spelling, as on a
    /// case-insensitive disk `.ENV` is `.env`; the editable patterns match
    /// exactly, so a `BUILD` file or `Dist/` folder is still shared.
    pub fn skips(&self, path: &str) -> bool {
        let folded = fold_case(path);
        SECRET_FLOOR
            .iter()
            .any(|pattern| matches_pattern(pattern, &folded))
            || self
                .patterns
                .iter()
                .any(|pattern| matches_pattern(pattern, path))
    }
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
        None => pattern == name,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TeamFileChange {
    pub path: String,
    pub base_hash: Option<String>,
    #[serde(default, with = "optional_base64")]
    pub content: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TeamChangeBatch {
    pub note: Option<String>,
    pub changes: Vec<TeamFileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TeamBatchOutcome {
    pub batch_id: String,
    pub applied: Vec<String>,
    pub held: Vec<HeldFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeldFile {
    pub path: String,
    pub reason: HeldReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldReason {
    ChangedSinceYourCopy,
    NeedsApproval,
    UnsavedInEditor,
}

/// A teammate's batch as the sharer's app recorded it, with each file's
/// contents before and after, so it can be shown and undone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BatchRecord {
    pub id: String,
    pub login: String,
    pub name: String,
    pub at: String,
    pub note: Option<String>,
    pub files: Vec<BatchFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BatchFile {
    pub path: String,
    /// The sharer's file when the batch arrived, None when there was none.
    #[serde(default, with = "optional_base64")]
    pub before: Option<Vec<u8>>,
    /// The teammate's file, None when they deleted it.
    #[serde(default, with = "optional_base64")]
    pub after: Option<Vec<u8>>,
    pub state: BatchFileState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchFileState {
    Applied,
    Held(HeldReason),
    /// The owner kept their version of a held file.
    Kept,
    /// The owner took the teammate's version of a held file.
    Taken,
    Undone,
}

impl BatchRecord {
    pub fn outcome(&self) -> TeamBatchOutcome {
        let mut outcome = TeamBatchOutcome {
            batch_id: self.id.clone(),
            applied: Vec::new(),
            held: Vec::new(),
        };
        for file in &self.files {
            match file.state {
                BatchFileState::Applied | BatchFileState::Taken => {
                    outcome.applied.push(file.path.clone())
                }
                BatchFileState::Held(reason) => outcome.held.push(HeldFile {
                    path: file.path.clone(),
                    reason,
                }),
                BatchFileState::Kept | BatchFileState::Undone => {}
            }
        }
        outcome
    }
}

mod optional_base64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(bytes) => {
                serializer.serialize_some(&base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

pub fn file_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Compared in any case spelling, as a case-insensitive disk would.
pub fn needs_approval(path: &str) -> bool {
    let path = fold_case(path);
    let mut components = path.split('/').filter(|component| !component.is_empty());
    match components.next() {
        Some(".mnemonic" | ".claude" | ".zed" | ".vscode") => true,
        _ => path.rsplit('/').next().is_some_and(|name| name == ".envrc"),
    }
}

pub fn safe_relative_path(root: &Path, path: &str) -> Result<PathBuf, String> {
    let mut resolved = root.to_path_buf();
    let components = Path::new(path).components();
    if path.is_empty() {
        return Err("path is empty".to_string());
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("{path} is not a plain relative path"));
    }
    for component in components {
        match component {
            Component::Normal(name) => {
                if name.eq_ignore_ascii_case(".git") {
                    return Err(format!("{path} is inside .git"));
                }
                resolved.push(name);
                if std::fs::symlink_metadata(&resolved)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return Err(format!("{path} passes through a symlink"));
                }
            }
            _ => return Err(format!("{path} is not a plain relative path")),
        }
    }
    Ok(resolved)
}

#[allow(clippy::disallowed_methods)]
fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// The hashes a scan last gave each file, so a rescan reads only files whose
/// size, modified time or inode moved since.
#[derive(Debug, Default)]
pub struct HashCache {
    hashes: HashMap<String, (FileStamp, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: std::time::SystemTime,
    inode: u64,
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        };
        #[cfg(not(unix))]
        let inode = 0;
        Some(Self {
            size: metadata.len(),
            modified: metadata.modified().ok()?,
            inode,
        })
    }
}

pub fn scan_workspace(root: &Path, skip: &SkipList) -> Result<TeamFileManifest, String> {
    scan_workspace_cached(root, skip, &mut HashCache::default())
}

/// `scan_workspace`, reusing and refreshing the hashes in `cache`.
pub fn scan_workspace_cached(
    root: &Path,
    skip: &SkipList,
    cache: &mut HashCache,
) -> Result<TeamFileManifest, String> {
    let head_commit = String::from_utf8_lossy(&run_git(root, &["rev-parse", "HEAD"])?)
        .trim()
        .to_string();
    let status = run_git(
        root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=traditional",
        ],
    )?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("could not resolve workspace root: {error}"))?;

    let mut paths: Vec<String> = Vec::new();
    let mut fields = status
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let line = String::from_utf8_lossy(field);
        let mut parts = line.splitn(2, ' ');
        let tag = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default();
        match tag {
            "1" => {
                if let Some(path) = rest.splitn(8, ' ').nth(7) {
                    paths.push(path.to_string());
                }
            }
            "2" => {
                if let Some(path) = rest.splitn(9, ' ').nth(8) {
                    paths.push(path.to_string());
                }
                if let Some(original) = fields.next() {
                    paths.push(String::from_utf8_lossy(original).into_owned());
                }
            }
            "u" => {
                if let Some(path) = rest.splitn(10, ' ').nth(9) {
                    paths.push(path.to_string());
                }
            }
            "?" | "!" => paths.push(rest.to_string()),
            _ => {}
        }
    }

    let mut files: Vec<String> = Vec::new();
    for path in paths {
        if let Some(directory) = path.strip_suffix('/') {
            collect_files(root, directory, skip, &mut files)?;
        } else if !skip.skips(&path) {
            files.push(path);
        }
    }

    let previous = std::mem::take(cache);
    let mut entries: BTreeMap<String, TeamFileEntry> = BTreeMap::new();
    for path in files {
        if let Some(entry) = build_entry(root, &canonical_root, &path, &previous, cache)? {
            entries.insert(path, entry);
        }
    }
    Ok(TeamFileManifest {
        head_commit,
        entries: entries.into_values().collect(),
    })
}

fn collect_files(
    root: &Path,
    directory: &str,
    skip: &SkipList,
    files: &mut Vec<String>,
) -> Result<(), String> {
    if skip.skips(directory) {
        return Ok(());
    }
    let read_dir = match std::fs::read_dir(root.join(directory)) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not read {directory}: {error}")),
    };
    for entry in read_dir {
        let entry = entry.map_err(|error| format!("could not read {directory}: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = format!("{directory}/{name}");
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not stat {relative}: {error}"))?;
        if skip.skips(&relative) {
            continue;
        }
        if file_type.is_dir() {
            collect_files(root, &relative, skip, files)?;
        } else {
            files.push(relative);
        }
    }
    Ok(())
}

fn build_entry(
    root: &Path,
    canonical_root: &Path,
    path: &str,
    previous: &HashCache,
    cache: &mut HashCache,
) -> Result<Option<TeamFileEntry>, String> {
    let full_path = root.join(path);
    let metadata = match std::fs::symlink_metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(TeamFileEntry {
                path: path.to_string(),
                kind: TeamFileKind::Deleted,
                size: 0,
                hash: String::new(),
            }));
        }
        Err(error) => return Err(format!("could not stat {path}: {error}")),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&full_path)
            .map_err(|error| format!("could not read link {path}: {error}"))?;
        if target.is_absolute() {
            return Ok(None);
        }
        let inside_root = full_path
            .canonicalize()
            .map(|resolved| resolved.starts_with(canonical_root))
            .unwrap_or(false);
        if !inside_root {
            return Ok(None);
        }
        let target = target.to_string_lossy().into_owned();
        return Ok(Some(TeamFileEntry {
            path: path.to_string(),
            size: target.len() as u64,
            hash: file_hash(target.as_bytes()),
            kind: TeamFileKind::Symlink { target },
        }));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let stamp = FileStamp::of(&metadata);
    let cached = previous
        .hashes
        .get(path)
        .filter(|(cached_stamp, _)| Some(cached_stamp) == stamp.as_ref());
    let (size, hash) = match cached {
        Some((cached_stamp, hash)) => (cached_stamp.size, hash.clone()),
        None => hash_file(&full_path).map_err(|error| format!("could not read {path}: {error}"))?,
    };
    if let Some(stamp) = stamp {
        cache.hashes.insert(path.to_string(), (stamp, hash.clone()));
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(Some(TeamFileEntry {
        path: path.to_string(),
        kind: TeamFileKind::File { executable },
        size,
        hash,
    }))
}

fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

/// A git remote reduced to `host/path` so teammates who cloned over ssh and
/// https compare equal.
pub fn normalize_remote_url(url: &str) -> String {
    let mut remote = url.trim();
    if let Some((_, rest)) = remote.split_once("://") {
        remote = rest;
        if let Some((authority, path)) = remote.split_once('/') {
            let host = authority.rsplit('@').next().unwrap_or(authority);
            return finish_remote(host, path);
        }
        return finish_remote(remote.rsplit('@').next().unwrap_or(remote), "");
    }
    let without_user = remote.rsplit_once('@').map_or(remote, |(_, rest)| rest);
    match without_user.split_once(':') {
        Some((host, path)) => finish_remote(host, path),
        None => match without_user.split_once('/') {
            Some((host, path)) => finish_remote(host, path),
            None => finish_remote(without_user, ""),
        },
    }
}

fn finish_remote(host: &str, path: &str) -> String {
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    let host = host.to_lowercase();
    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

#[allow(clippy::disallowed_methods)]
fn run_git_locale_free(root: &Path, args: &[&str], have: &[String]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .env("LC_ALL", "C")
        .args(args)
        .args(have)
        .arg("--remotes=origin")
        .current_dir(root)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

pub fn bundle_for(root: &Path, have: &[String]) -> Result<Vec<u8>, String> {
    // A teammate's tips can include commits this clone never had, which
    // git refuses to exclude by.
    let have: Vec<String> = have
        .iter()
        .filter(|commit| has_commit(root, commit))
        .cloned()
        .collect();
    let have = have.as_slice();
    let missing = run_git_locale_free(root, &["rev-list", "-n1", "HEAD", "--not"], have)?;
    if missing.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    run_git_locale_free(root, &["bundle", "create", "-", "HEAD", "--not"], have)
}

struct TempBundle(PathBuf);

impl Drop for TempBundle {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Couldn't remove {}: {error}", self.0.display());
            }
        }
    }
}

fn write_temp_bundle(bundle: &[u8]) -> Result<TempBundle, String> {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("mnemonic-share-{}.bundle", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("could not save the bundle: {error}"))?;
    let guard = TempBundle(path);
    file.write_all(bundle)
        .map_err(|error| format!("could not save the bundle: {error}"))?;
    Ok(guard)
}

pub fn fetch_bundle(repo: &Path, bundle: &[u8], head_commit: &str) -> Result<(), String> {
    let is_commit_id = (7..=64).contains(&head_commit.len())
        && head_commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !is_commit_id {
        return Err(format!("{head_commit} isn't a commit id"));
    }
    if !bundle.is_empty() {
        let bundle_file = write_temp_bundle(bundle)?;
        run_git(
            repo,
            &["fetch", "--quiet", &bundle_file.0.to_string_lossy(), "HEAD"],
        )?;
    }
    if run_git(repo, &["cat-file", "-e", head_commit]).is_ok() {
        Ok(())
    } else {
        Err(format!(
            "{head_commit} isn't in your clone or the sharer's bundle"
        ))
    }
}

/// Marks a workspace that is a teammate's copy, so Mnemonic doesn't run
/// the sharer's Setup in it.
pub const TEAM_COPY_SOURCE: &str = "team_copy";

/// A teammate's copy of a shared workspace, as this machine follows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CopyState {
    pub host: String,
    /// The sharer's name as configured for that teammate.
    #[serde(default)]
    pub sharer_name: String,
    pub workspace_id: String,
    pub local_workspace_id: String,
    /// The sharer's base branch, so a bundle can leave out what it holds.
    #[serde(default)]
    pub base_branch: Option<String>,
    pub head_commit: String,
    /// Each path last written from the sharer, with the hash written; an
    /// empty hash when the sharer had deleted it.
    pub synced: HashMap<String, String>,
    /// The sharer's skip list for the workspace, empty when not known.
    #[serde(default)]
    pub skip_patterns: Vec<String>,
}

impl CopyState {
    /// The skip list the sharer shares by, so the copy neither takes nor
    /// offers to send a file the sharer would skip.
    pub fn skip_list(&self) -> SkipList {
        if self.skip_patterns.is_empty() {
            SkipList::default()
        } else {
            SkipList {
                patterns: self.skip_patterns.clone(),
            }
        }
    }
}

/// Whether `path` is Mnemonic's own workspace config. Mnemonic ignores it in
/// git, so the sharer's would arrive as a shared file, and a copy holding it
/// would run the sharer's hooks and commands on the teammate's machine.
fn is_mnemonic_config(path: &str) -> bool {
    path.split('/')
        .find(|component| !component.is_empty())
        .is_some_and(|first| fold_case(first) == ".mnemonic")
}

/// What a copy never writes from the sharer nor reports as an edit.
fn copy_skips(skip: &SkipList, path: &str) -> bool {
    skip.skips(path) || is_mnemonic_config(path)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowPlan {
    pub write: Vec<String>,
    pub delete: Vec<String>,
    /// Paths the sharer no longer changes, returned to HEAD's version (or
    /// removed when HEAD lacks them).
    pub restore: Vec<String>,
    pub both_changed: Vec<String>,
    pub move_head: bool,
    pub waiting_reason: Option<String>,
}

fn follow_target(entry: &TeamFileEntry) -> &str {
    match entry.kind {
        TeamFileKind::Deleted => "",
        _ => entry.hash.as_str(),
    }
}

/// What following `incoming` means for the copy. `local` holds the current
/// hash of every relevant path that exists in the copy (paths in `synced`,
/// in `incoming`, and in `dirty_vs_head`); `dirty_vs_head` holds the paths
/// that differ from the copy's HEAD. A path is touched by the teammate when
/// its local hash differs from `synced`, or, for a path not in `synced`,
/// when it differs from HEAD at all, since git's sha1 blob ids can't be
/// compared with these sha256 hashes.
/// Up to three of `paths`, as "a, b and 2 more".
fn name_some(paths: &[&str]) -> String {
    match paths {
        [] => String::new(),
        [only] => only.to_string(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second} and {third}"),
        [first, second, third, rest @ ..] => {
            format!("{first}, {second}, {third} and {} more", rest.len())
        }
    }
}

pub fn plan_follow(
    state: &CopyState,
    local: &HashMap<String, String>,
    dirty_vs_head: &HashSet<String>,
    incoming: &TeamFileManifest,
    has_local_commits: bool,
) -> FollowPlan {
    let local_hash = |path: &str| local.get(path).map(String::as_str).unwrap_or("");
    let touched = |path: &str| match state.synced.get(path) {
        Some(base) => local_hash(path) != base,
        None => dirty_vs_head.contains(path),
    };
    let incoming_by_path: HashMap<&str, &TeamFileEntry> = incoming
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut dropped: Vec<&str> = state
        .synced
        .keys()
        .map(String::as_str)
        .filter(|path| !incoming_by_path.contains_key(path))
        .collect();
    dropped.sort_unstable();

    let mut unsent_edits: Vec<&str> = state
        .synced
        .keys()
        .chain(dirty_vs_head.iter())
        .map(String::as_str)
        .filter(|&path| {
            let agrees = match incoming_by_path.get(path) {
                Some(entry) => local_hash(path) == follow_target(entry),
                None => !dirty_vs_head.contains(path),
            };
            touched(path) && !agrees
        })
        .collect();
    unsent_edits.sort_unstable();
    unsent_edits.dedup();

    let head_moves = incoming.head_commit != state.head_commit;
    let waiting_reason = if !head_moves {
        None
    } else if has_local_commits {
        Some(
            "This copy has commits of its own, so it stays on the commit it started from"
                .to_string(),
        )
    } else if !unsent_edits.is_empty() {
        Some(format!(
            "Waiting to follow the new commit: you have unsent changes to {}",
            name_some(&unsent_edits)
        ))
    } else {
        None
    };
    let mut plan = FollowPlan {
        move_head: head_moves && waiting_reason.is_none(),
        waiting_reason,
        ..FollowPlan::default()
    };

    if plan.move_head {
        for entry in &incoming.entries {
            match entry.kind {
                TeamFileKind::Deleted => plan.delete.push(entry.path.clone()),
                _ => plan.write.push(entry.path.clone()),
            }
        }
        plan.restore = dropped.iter().map(|path| path.to_string()).collect();
        return plan;
    }

    for entry in &incoming.entries {
        let path = entry.path.as_str();
        let target = follow_target(entry);
        let base = state.synced.get(path).map(String::as_str);
        let follow = if local_hash(path) == target {
            base != Some(target)
        } else if !touched(path) {
            true
        } else {
            if base != Some(target) {
                plan.both_changed.push(entry.path.clone());
            }
            false
        };
        if follow {
            match entry.kind {
                TeamFileKind::Deleted => plan.delete.push(entry.path.clone()),
                _ => plan.write.push(entry.path.clone()),
            }
        }
    }
    // A path dropped from the manifest may be in a commit this copy hasn't
    // moved to yet, so it is only restored while both sides share HEAD.
    if !head_moves {
        for path in dropped {
            if touched(path) && dirty_vs_head.contains(path) {
                plan.both_changed.push(path.to_string());
            } else {
                plan.restore.push(path.to_string());
            }
        }
    }
    plan
}

/// The copy as it is on disk now: hashes of every path that differs from
/// HEAD or is named in `paths`, and which of those differ from HEAD.
pub struct LocalCopy {
    pub hashes: HashMap<String, String>,
    pub dirty_vs_head: HashSet<String>,
}

pub fn read_local_copy<'a>(
    root: &Path,
    skip: &SkipList,
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<LocalCopy, String> {
    let scan = scan_workspace(root, skip)?;
    let mut hashes = HashMap::new();
    let mut dirty_vs_head = HashSet::new();
    for entry in scan.entries {
        if is_mnemonic_config(&entry.path) {
            continue;
        }
        if !matches!(entry.kind, TeamFileKind::Deleted) {
            hashes.insert(entry.path.clone(), entry.hash);
        }
        dirty_vs_head.insert(entry.path);
    }
    for path in paths {
        if dirty_vs_head.contains(path) || copy_skips(skip, path) {
            continue;
        }
        let Ok(full_path) = safe_relative_path(root, path) else {
            continue;
        };
        if let Some(hash) = current_hash(&full_path)? {
            hashes.insert(path.to_string(), hash);
        }
    }
    Ok(LocalCopy {
        hashes,
        dirty_vs_head,
    })
}

/// The hash a scan would give the path, None when nothing is there.
fn current_hash(full_path: &Path) -> Result<Option<String>, String> {
    let metadata = match std::fs::symlink_metadata(full_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not stat {}: {error}", full_path.display())),
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(full_path)
            .map_err(|error| format!("could not read link {}: {error}", full_path.display()))?;
        return Ok(Some(file_hash(target.to_string_lossy().as_bytes())));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    hash_file(full_path)
        .map(|(_, hash)| Some(hash))
        .map_err(|error| format!("could not read {}: {error}", full_path.display()))
}

/// The paths the teammate changed in their copy and hasn't sent.
pub fn local_edits(state: &CopyState, root: &Path, skip: &SkipList) -> Result<Vec<String>, String> {
    Ok(local_edit_hashes(state, root, skip)?.into_keys().collect())
}

/// Each path the teammate changed and hasn't sent, with its hash in the
/// copy now; an empty hash when they deleted it.
pub fn local_edit_hashes(
    state: &CopyState,
    root: &Path,
    skip: &SkipList,
) -> Result<BTreeMap<String, String>, String> {
    let local = read_local_copy(root, skip, state.synced.keys().map(String::as_str))?;
    let local_hash = |path: &str| local.hashes.get(path).cloned().unwrap_or_default();
    let synced_edits = state
        .synced
        .iter()
        .filter(|(path, base)| !copy_skips(skip, path) && local_hash(path) != base.as_str())
        .map(|(path, _)| path);
    let head_edits = local
        .dirty_vs_head
        .iter()
        .filter(|path| !state.synced.contains_key(*path));
    Ok(synced_edits
        .chain(head_edits)
        .map(|path| (path.clone(), local_hash(path)))
        .collect())
}

pub fn has_commit(repo: &Path, commit: &str) -> bool {
    run_git(repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok()
}

pub fn fetch_origin(repo: &Path) -> Result<(), String> {
    run_git(repo, &["fetch", "--quiet", "origin"]).map(|_| ())
}

/// The normalized `origin` of the repository, None when it has none.
pub fn origin_url(repo: &Path) -> Option<String> {
    let output = run_git(repo, &["remote", "get-url", "origin"]).ok()?;
    let url = normalize_remote_url(&String::from_utf8_lossy(&output));
    (!url.is_empty()).then_some(url)
}

/// `wanted`, or it with the first free numeric suffix when a branch of that
/// name exists, so a new copy never lands on an older copy's branch.
pub fn unused_branch_name(repo: &Path, wanted: &str) -> Result<String, String> {
    let exists = |name: &str| {
        run_git(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .is_ok()
    };
    if !exists(wanted) {
        return Ok(wanted.to_string());
    }
    (2..1000)
        .map(|suffix| format!("{wanted}-{suffix}"))
        .find(|name| !exists(name))
        .ok_or_else(|| format!("Every branch name like {wanted} is taken"))
}

/// Commits this clone has that a sharer's bundle can leave out: the tip of
/// `origin/<base_branch>` first, then other `origin` branches and local
/// branches, newest first, at most `limit`.
pub fn have_commits(repo: &Path, base_branch: Option<&str>, limit: usize) -> Vec<String> {
    let tips = |refs: &[&str]| -> Vec<String> {
        let mut args = vec![
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(objectname)",
        ];
        args.extend_from_slice(refs);
        match run_git(repo, &args) {
            Ok(output) => String::from_utf8_lossy(&output)
                .lines()
                .map(str::to_string)
                .collect(),
            Err(error) => {
                log::warn!("Couldn't list branches in {}: {error}", repo.display());
                Vec::new()
            }
        }
    };
    let base_ref = base_branch.map(|base| format!("refs/remotes/origin/{base}"));
    let mut commits: Vec<String> = Vec::new();
    let preferred = base_ref
        .as_deref()
        .map(|base| tips(&[base]))
        .unwrap_or_default();
    for commit in preferred
        .into_iter()
        .chain(tips(&["refs/remotes/origin", "refs/heads"]))
    {
        if commits.len() >= limit {
            break;
        }
        if !commits.contains(&commit) {
            commits.push(commit);
        }
    }
    commits
}

/// Whether HEAD has commits the sharer never had, judged against both the
/// commit the copy started from and the sharer's new one, so a sharer who
/// amended or rebased isn't mistaken for the teammate committing.
pub fn has_local_commits(
    root: &Path,
    started_from: &str,
    sharer_head: &str,
) -> Result<bool, String> {
    let output = run_git(
        root,
        &[
            "rev-list",
            "-n1",
            "HEAD",
            "--not",
            started_from,
            sharer_head,
        ],
    )?;
    Ok(!output.iter().all(u8::is_ascii_whitespace))
}

#[allow(clippy::disallowed_methods)]
fn run_git_on_paths(root: &Path, args: &[&str], paths: &[&str]) -> Result<(), String> {
    for chunk in paths.chunks(200) {
        let output = Command::new("git")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .args(args)
            .arg("--")
            .args(chunk)
            .current_dir(root)
            .output()
            .map_err(|error| format!("could not run git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

fn tracked_paths(root: &Path, commit: &str) -> Result<HashSet<String>, String> {
    let output = run_git(root, &["ls-tree", "-r", "-z", "--name-only", commit])?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn remove_if_present(full_path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(full_path) {
        Ok(metadata) if metadata.is_dir() => Err(format!("{} is a folder", full_path.display())),
        Ok(_) => std::fs::remove_file(full_path)
            .map_err(|error| format!("could not remove {}: {error}", full_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not stat {}: {error}", full_path.display())),
    }
}

/// Where an in-tree link at `path` points, relative to the root, or None
/// when its target leaves the root or goes through `.git`.
fn in_tree_link_target(path: &str, target: &str) -> Option<String> {
    if target.is_empty() || Path::new(target).is_absolute() {
        return None;
    }
    let mut resolved: Vec<&str> = path.split('/').collect();
    resolved.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                resolved.pop()?;
            }
            name if name.eq_ignore_ascii_case(".git") => return None,
            name => resolved.push(name),
        }
    }
    (!resolved.is_empty()).then(|| resolved.join("/"))
}

fn temp_path_beside(full_path: &Path) -> Result<PathBuf, String> {
    let name = full_path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", full_path.display()))?;
    Ok(full_path.with_file_name(format!(
        ".{}.{}.mnemonic-tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    )))
}

fn create_parent(full_path: &Path) -> Result<(), String> {
    match full_path.parent() {
        Some(parent) => std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display())),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn set_executable(full_path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(full_path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        format!(
            "could not set permissions on {}: {error}",
            full_path.display()
        )
    })
}

#[cfg(not(unix))]
fn set_executable(_full_path: &Path, _executable: bool) -> Result<(), String> {
    Ok(())
}

fn replace_atomically(
    full_path: &Path,
    write_temp: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    create_parent(full_path)?;
    let temp_path = temp_path_beside(full_path)?;
    let written = write_temp(&temp_path).and_then(|()| {
        std::fs::rename(&temp_path, full_path)
            .map_err(|error| format!("could not replace {}: {error}", full_path.display()))
    });
    if written.is_err() {
        if let Err(error) = remove_if_present(&temp_path) {
            log::warn!("Couldn't clean up {}: {error}", temp_path.display());
        }
    }
    written
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyWrite {
    /// Written, with the hash now in the copy.
    Written(String),
    /// The entry may not be written in a copy.
    Refused,
    /// The copy's file changed since the round read it, so it was kept.
    ChangedLocally,
}

/// Whether the file at `full_path` still has `expected`, the hash a follow
/// round planned from (empty for no file); always when `expected` is None.
fn unchanged_since_planned(full_path: &Path, expected: Option<&str>) -> Result<bool, String> {
    match expected {
        Some(expected) => Ok(current_hash(full_path)?.unwrap_or_default() == expected),
        None => Ok(true),
    }
}

/// Writes one of the sharer's entries into the copy, fetching its bytes
/// only when the copy doesn't already hold them. With `expected_local`,
/// the copy's file is only replaced while it still has that hash, so an
/// edit made since the round read the copy is kept.
pub fn write_copy_entry(
    root: &Path,
    entry: &TeamFileEntry,
    expected_local: Option<&str>,
    fetch: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<CopyWrite, String> {
    if copy_skips(&SkipList { patterns: vec![] }, &entry.path) {
        return Ok(CopyWrite::Refused);
    }
    let full_path = safe_relative_path(root, &entry.path)?;
    match &entry.kind {
        TeamFileKind::Deleted => {
            if !unchanged_since_planned(&full_path, expected_local)? {
                return Ok(CopyWrite::ChangedLocally);
            }
            remove_if_present(&full_path)?;
            Ok(CopyWrite::Written(String::new()))
        }
        TeamFileKind::File { executable } => {
            let is_file = std::fs::symlink_metadata(&full_path)
                .is_ok_and(|metadata| metadata.file_type().is_file());
            if is_file && current_hash(&full_path)?.as_deref() == Some(entry.hash.as_str()) {
                set_executable(&full_path, *executable)?;
                return Ok(CopyWrite::Written(entry.hash.clone()));
            }
            let bytes = fetch(&entry.path)?;
            if !unchanged_since_planned(&full_path, expected_local)? {
                return Ok(CopyWrite::ChangedLocally);
            }
            replace_atomically(&full_path, |temp_path| {
                std::fs::write(temp_path, &bytes)
                    .map_err(|error| format!("could not write {}: {error}", entry.path))?;
                set_executable(temp_path, *executable)
            })?;
            Ok(CopyWrite::Written(file_hash(&bytes)))
        }
        TeamFileKind::Symlink { target } => {
            let inside = in_tree_link_target(&entry.path, target)
                .is_some_and(|resolved| safe_relative_path(root, &resolved).is_ok());
            if !inside {
                log::warn!(
                    "Not recreating {}: it points outside the workspace",
                    entry.path
                );
                return Ok(CopyWrite::Refused);
            }
            if !unchanged_since_planned(&full_path, expected_local)? {
                return Ok(CopyWrite::ChangedLocally);
            }
            replace_atomically(&full_path, |temp_path| make_symlink(target, temp_path))?;
            Ok(CopyWrite::Written(file_hash(target.as_bytes())))
        }
    }
}

#[cfg(unix)]
fn make_symlink(target: &str, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|error| format!("could not link {}: {error}", link.display()))
}

#[cfg(not(unix))]
fn make_symlink(_target: &str, link: &Path) -> Result<(), String> {
    Err(format!(
        "could not link {}: links aren't supported here",
        link.display()
    ))
}

/// Brings the copy at `root` up to `incoming`, recording in `state` what was
/// written as it goes, so it stays accurate when a step fails. The sharer's
/// HEAD must already be in the copy's repository.
pub fn follow_manifest(
    root: &Path,
    state: &mut CopyState,
    incoming: &TeamFileManifest,
    fetch: &dyn Fn(&str) -> Result<Vec<u8>, String>,
    on_progress: &dyn Fn(usize, usize),
) -> Result<FollowPlan, String> {
    let skip = state.skip_list();
    let incoming = TeamFileManifest {
        head_commit: incoming.head_commit.clone(),
        entries: incoming
            .entries
            .iter()
            .filter(|entry| !copy_skips(&skip, &entry.path))
            .cloned()
            .collect(),
    };
    let mut local = read_local_copy(
        root,
        &skip,
        state
            .synced
            .keys()
            .map(String::as_str)
            .chain(incoming.entries.iter().map(|entry| entry.path.as_str())),
    )?;
    // Changes the teammate committed on the copy branch are clean against
    // HEAD but are still theirs, so they count against the sharer's commit.
    let copy_head = String::from_utf8_lossy(&run_git(root, &["rev-parse", "HEAD"])?)
        .trim()
        .to_string();
    if copy_head != state.head_commit {
        let committed = run_git(
            root,
            &["diff", "--name-only", "-z", &state.head_commit, "HEAD"],
        )?;
        local.dirty_vs_head.extend(
            committed
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .filter(|path| !copy_skips(&skip, path)),
        );
    }
    let head_moves = incoming.head_commit != state.head_commit;
    let has_local_commits =
        head_moves && has_local_commits(root, &state.head_commit, &incoming.head_commit)?;
    let mut plan = plan_follow(
        state,
        &local.hashes,
        &local.dirty_vs_head,
        &incoming,
        has_local_commits,
    );
    // The teammate can edit while the round runs; each file the plan saw is
    // checked again just before it is replaced, and kept if it moved.
    let planned_hash = |path: &str| local.hashes.get(path).cloned().unwrap_or_default();
    let changed_since_planned = |path: &str| -> Result<bool, String> {
        Ok(!copy_skips(&skip, path) && hash_now(root, path)? != planned_hash(path))
    };

    if plan.move_head {
        let mut changed = Vec::new();
        for path in state.synced.keys() {
            if changed_since_planned(path)? {
                changed.push(path.as_str());
            }
        }
        if !changed.is_empty() {
            changed.sort_unstable();
            return Ok(FollowPlan {
                waiting_reason: Some(format!(
                    "Waiting to follow the new commit: you have unsent changes to {}",
                    name_some(&changed)
                )),
                ..FollowPlan::default()
            });
        }
        move_head(root, state, &incoming.head_commit)?;
    }
    if !plan.restore.is_empty() {
        let mut restore = Vec::new();
        // A moving round already checked these, as they are all synced.
        for path in std::mem::take(&mut plan.restore) {
            if !plan.move_head && changed_since_planned(&path)? {
                plan.both_changed.push(path);
            } else {
                restore.push(path);
            }
        }
        restore_to_head(root, &restore)?;
        for path in &restore {
            state.synced.remove(path);
        }
        plan.restore = restore;
    }
    // Moving HEAD rewrote the copy, so what the plan saw no longer holds;
    // edits from here on are still kept.
    let mut expected = HashMap::new();
    for path in plan.write.iter().chain(&plan.delete) {
        let hash = if plan.move_head {
            hash_now(root, path)?
        } else {
            planned_hash(path)
        };
        expected.insert(path.as_str(), hash);
    }

    let entries: HashMap<&str, &TeamFileEntry> = incoming
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut failures = Vec::new();
    let mut kept = Vec::new();
    let total = plan.write.len() + plan.delete.len();
    for (done, path) in plan.write.iter().chain(&plan.delete).enumerate() {
        on_progress(done, total);
        let Some(entry) = entries.get(path.as_str()) else {
            continue;
        };
        let expected_local = expected.get(path.as_str()).map(String::as_str);
        match write_copy_entry(root, entry, expected_local, fetch) {
            Ok(CopyWrite::Written(hash)) => {
                state.synced.insert(path.clone(), hash);
            }
            Ok(CopyWrite::Refused) => {}
            Ok(CopyWrite::ChangedLocally) => kept.push(path.clone()),
            Err(error) => failures.push(error),
        }
    }
    plan.both_changed.extend(kept);
    on_progress(total, total);
    match failures.first() {
        None => Ok(plan),
        Some(first) if failures.len() == 1 => Err(first.clone()),
        Some(first) => Err(format!("{first} (and {} more)", failures.len() - 1)),
    }
}

/// Returns each path to HEAD's version, removing it when HEAD lacks it.
fn restore_to_head(root: &Path, paths: &[String]) -> Result<(), String> {
    let tracked = tracked_paths(root, "HEAD")?;
    let (in_head, not_in_head): (Vec<&str>, Vec<&str>) = paths
        .iter()
        .map(String::as_str)
        .partition(|path| tracked.contains(*path));
    run_git_on_paths(root, &["checkout", "HEAD"], &in_head)?;
    for path in not_in_head {
        if let Ok(full_path) = safe_relative_path(root, path) {
            remove_if_present(&full_path)?;
        }
    }
    Ok(())
}

/// Takes the sharer's files off the copy, so `git reset --keep` sees no
/// local changes in its way, then moves the copy's branch to `new_head`.
/// The planner has already checked none of those files was the teammate's.
fn move_head(root: &Path, state: &mut CopyState, new_head: &str) -> Result<(), String> {
    let tracked_now = tracked_paths(root, "HEAD")?;
    let tracked_next = tracked_paths(root, new_head)?;
    let mut synced: Vec<String> = state.synced.keys().cloned().collect();
    synced.sort_unstable();
    let (in_head, not_in_head): (Vec<&str>, Vec<&str>) = synced
        .iter()
        .map(String::as_str)
        .partition(|path| tracked_now.contains(*path));
    run_git_on_paths(root, &["checkout", "HEAD"], &in_head)?;
    for path in not_in_head
        .into_iter()
        .filter(|path| tracked_next.contains(*path))
    {
        if let Ok(full_path) = safe_relative_path(root, path) {
            remove_if_present(&full_path)?;
        }
    }
    // Recorded before the reset, so if it fails the next round sees these
    // files as the sharer's rather than as the teammate's edits, and again
    // after it, so a file that then fails to arrive isn't either.
    record_current_hashes(root, state, &synced)?;
    run_git(root, &["reset", "--quiet", "--keep", new_head])?;
    state.head_commit = new_head.to_string();
    record_current_hashes(root, state, &synced)
}

/// The hash `path` has in the copy now, empty when nothing is there or it
/// can't be reached safely.
fn hash_now(root: &Path, path: &str) -> Result<String, String> {
    match safe_relative_path(root, path) {
        Ok(full_path) => Ok(current_hash(&full_path)?.unwrap_or_default()),
        Err(_) => Ok(String::new()),
    }
}

fn record_current_hashes(
    root: &Path,
    state: &mut CopyState,
    paths: &[String],
) -> Result<(), String> {
    for path in paths {
        state.synced.insert(path.clone(), hash_now(root, path)?);
    }
    Ok(())
}

fn copy_states_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("team-copies")
}

pub fn save_copy_state(data_dir: &Path, state: &CopyState) -> Result<(), String> {
    if state.local_workspace_id.is_empty()
        || !state
            .local_workspace_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!("{} isn't a workspace id", state.local_workspace_id));
    }
    let json = serde_json::to_vec_pretty(state).map_err(|error| error.to_string())?;
    let full_path = copy_states_dir(data_dir).join(format!("{}.json", state.local_workspace_id));
    replace_atomically(&full_path, |temp_path| {
        std::fs::write(temp_path, &json)
            .map_err(|error| format!("could not save {}: {error}", full_path.display()))
    })
}

pub fn load_copy_states(data_dir: &Path) -> Result<Vec<CopyState>, String> {
    let directory = copy_states_dir(data_dir);
    let read_dir = match std::fs::read_dir(&directory) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", directory.display())),
    };
    let mut states = Vec::new();
    for entry in read_dir {
        let path = entry
            .map_err(|error| format!("could not read {}: {error}", directory.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let parsed = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<CopyState>(&bytes).map_err(|error| error.to_string())
            });
        match parsed {
            Ok(state) => states.push(state),
            Err(error) => log::error!("Couldn't read {}: {error}", path.display()),
        }
    }
    Ok(states)
}

pub fn remove_copy_state(data_dir: &Path, local_workspace_id: &str) -> Result<(), String> {
    let full_path = copy_states_dir(data_dir).join(format!("{local_workspace_id}.json"));
    remove_if_present(&full_path)
}

/// Merges a follow round's result into the copy's live state: the round's
/// paths, except those a send or discard changed while the round ran, as
/// the round started from an older state and would otherwise undo them.
pub fn merge_follow_round(
    started_from: &CopyState,
    round: CopyState,
    live: &CopyState,
) -> CopyState {
    let mut merged = round;
    for path in started_from.synced.keys().chain(live.synced.keys()) {
        let live_hash = live.synced.get(path);
        if live_hash == started_from.synced.get(path) {
            continue;
        }
        match live_hash {
            Some(hash) => {
                merged.synced.insert(path.clone(), hash.clone());
            }
            None => {
                merged.synced.remove(path);
            }
        }
    }
    merged
}

/// A batch ready to send, and the edits that can't be sent in one.
pub struct OutgoingBatch {
    pub batch: TeamChangeBatch,
    /// Why each edit that can't be sent was left out, one line per path.
    pub left_out: Vec<String>,
}

/// Builds the batch for `paths` in the copy at `root`. Each change's base is
/// the sharer's version the copy last had: the hash last written from the
/// sharer, or for a path never written from them, its hash at the sharer's
/// commit, since the sharer's file then matched that commit.
pub fn build_batch(
    state: &CopyState,
    root: &Path,
    paths: &[String],
    note: Option<String>,
) -> Result<OutgoingBatch, String> {
    let mut in_head: Option<HashSet<String>> = None;
    let mut changes = Vec::new();
    let mut left_out = Vec::new();
    for path in paths {
        let full_path = match safe_relative_path(root, path) {
            Ok(full_path) => full_path,
            Err(reason) => {
                log::warn!("Not sending {path}: {reason}");
                left_out.push(reason);
                continue;
            }
        };
        let content = match std::fs::symlink_metadata(&full_path) {
            Ok(metadata) if metadata.is_file() => Some(
                std::fs::read(&full_path)
                    .map_err(|error| format!("could not read {path}: {error}"))?,
            ),
            Ok(_) => {
                left_out.push(format!(
                    "{path} is a link or folder, and only files can be sent"
                ));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("could not stat {path}: {error}")),
        };
        let base_hash = match state.synced.get(path) {
            Some(hash) => (!hash.is_empty()).then(|| hash.clone()),
            None => {
                let tracked = match &mut in_head {
                    Some(tracked) => tracked,
                    None => in_head.insert(tracked_paths(root, &state.head_commit)?),
                };
                if tracked.contains(path) {
                    let blob = run_git(
                        root,
                        &["cat-file", "blob", &format!("{}:{path}", state.head_commit)],
                    )?;
                    Some(file_hash(&blob))
                } else {
                    None
                }
            }
        };
        changes.push(TeamFileChange {
            path: path.clone(),
            base_hash,
            content,
        });
    }
    Ok(OutgoingBatch {
        batch: TeamChangeBatch { note, changes },
        left_out,
    })
}

/// Records the files the sharer applied from `batch` as the sharer's, so
/// they stop counting as unsent edits. Held files stay the teammate's.
pub fn record_sent(state: &mut CopyState, batch: &TeamChangeBatch, outcome: &TeamBatchOutcome) {
    let applied: HashSet<&str> = outcome.applied.iter().map(String::as_str).collect();
    for change in &batch.changes {
        if applied.contains(change.path.as_str()) {
            let hash = change.content.as_deref().map(file_hash).unwrap_or_default();
            state.synced.insert(change.path.clone(), hash);
        }
    }
}

/// Returns the copy's `path` to the sharer's version: `entry` when the
/// sharer's file list names it, else the copy's HEAD, which the sharer's
/// file then matches. The new hash to record as synced, or None when the
/// path is no longer the sharer's change.
pub fn discard_edit(
    root: &Path,
    path: &str,
    entry: Option<&TeamFileEntry>,
    fetch: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<Option<String>, String> {
    match entry {
        Some(entry) => match write_copy_entry(root, entry, None, fetch)? {
            CopyWrite::Written(hash) => Ok(Some(hash)),
            CopyWrite::Refused | CopyWrite::ChangedLocally => {
                Err(format!("{path} can't be written in this copy"))
            }
        },
        None => {
            restore_to_head(root, &[path.to_string()])?;
            Ok(None)
        }
    }
}

fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

/// A batch id that sorts by when it arrived, given `now` in one fixed
/// format.
fn batch_id(now: &str) -> String {
    let stamp: String = now.chars().filter(char::is_ascii_alphanumeric).collect();
    let suffix: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect();
    format!("{stamp}-{suffix}")
}

/// What is at `full_path` now: its bytes, or None when nothing is there.
fn read_existing(path: &str, full_path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(full_path) {
        Ok(metadata) if metadata.is_file() => std::fs::read(full_path)
            .map(Some)
            .map_err(|error| format!("could not read {path}: {error}")),
        Ok(_) => Err(format!("{path} isn't a file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not stat {path}: {error}")),
    }
}

/// Writes `content` at `full_path` with `permissions`, or removes it when
/// None.
fn write_batch_file(
    full_path: &Path,
    content: Option<&[u8]>,
    permissions: Option<&std::fs::Permissions>,
) -> Result<(), String> {
    let Some(bytes) = content else {
        return remove_if_present(full_path);
    };
    replace_atomically(full_path, |temp_path| {
        std::fs::write(temp_path, bytes)
            .map_err(|error| format!("could not write {}: {error}", full_path.display()))?;
        match permissions {
            Some(permissions) => {
                std::fs::set_permissions(temp_path, permissions.clone()).map_err(|error| {
                    format!(
                        "could not set permissions on {}: {error}",
                        full_path.display()
                    )
                })
            }
            None => Ok(()),
        }
    })
}

/// Checks that a batch path names what is on disk byte for byte. A
/// case-insensitive disk opens `.Vscode` or a differently normalized name
/// as the existing entry, which would slip past the checks on the name.
#[derive(Default)]
struct SpellingCheck {
    /// Names in each folder seen so far.
    folders: HashMap<PathBuf, HashSet<std::ffi::OsString>>,
}

impl SpellingCheck {
    fn check(&mut self, root: &Path, path: &str) -> Result<(), String> {
        let mut folder = root.to_path_buf();
        for component in path.split('/') {
            let next = folder.join(component);
            match std::fs::symlink_metadata(&next) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(format!("could not stat {path}: {error}")),
            }
            let names = match self.folders.entry(folder.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let mut names = HashSet::new();
                    let read_dir = std::fs::read_dir(&folder)
                        .map_err(|error| format!("could not read {}: {error}", folder.display()))?;
                    for name in read_dir {
                        let name = name.map_err(|error| {
                            format!("could not read {}: {error}", folder.display())
                        })?;
                        names.insert(name.file_name());
                    }
                    entry.insert(names)
                }
            };
            if !names.contains(std::ffi::OsStr::new(component)) {
                return Err(format!("{path} is spelled differently on disk"));
            }
            folder = next;
        }
        Ok(())
    }
}

/// What `replace_if_unchanged` found.
enum Replaced {
    /// Written, with the permissions the file had, so it can be put back.
    Written(Option<std::fs::Permissions>),
    /// Left alone, as the file no longer held what was expected; this is
    /// what it holds instead.
    Changed(Option<Vec<u8>>),
}

/// Writes `content` at `path` with the file's permissions, or removes it
/// when None, only while the file holds `expected` (None: nothing there).
fn replace_if_unchanged(
    root: &Path,
    path: &str,
    full_path: &Path,
    expected: Option<&[u8]>,
    content: Option<&[u8]>,
) -> Result<Replaced, String> {
    // Checked again just before writing, in case a link appeared on the way
    // or the sharer's side wrote the file since it was last read.
    safe_relative_path(root, path)?;
    let permissions = std::fs::symlink_metadata(full_path)
        .ok()
        .map(|metadata| metadata.permissions());
    let now = read_existing(path, full_path)?;
    if now.as_deref() != expected {
        return Ok(Replaced::Changed(now));
    }
    write_batch_file(full_path, content, permissions.as_ref())?;
    Ok(Replaced::Written(permissions))
}

/// Writes one file of a batch, unless the sharer's file changed since the
/// batch was checked, in which case it is held with what is there now.
/// Returns the permissions the file had when it was written, so it can be
/// put back.
fn write_checked_file(
    root: &Path,
    file: &mut BatchFile,
    full_path: &Path,
) -> Result<Option<Option<std::fs::Permissions>>, String> {
    let replaced = replace_if_unchanged(
        root,
        &file.path,
        full_path,
        file.before.as_deref(),
        file.after.as_deref(),
    )?;
    match replaced {
        Replaced::Written(permissions) => Ok(Some(permissions)),
        Replaced::Changed(now) => {
            let already_there =
                now.as_deref().map(file_hash) == file.after.as_deref().map(file_hash);
            file.state = if already_there {
                BatchFileState::Applied
            } else {
                BatchFileState::Held(HeldReason::ChangedSinceYourCopy)
            };
            file.before = now;
            Ok(None)
        }
    }
}

/// Applies a teammate's batch to the workspace at `root`. Every path is
/// checked before anything is written, and one that may not be written
/// rejects the whole batch. A change applies when the file still matches
/// the base the teammate started from; otherwise, or when the file has
/// unsaved edits in an editor (`unsaved`, relative to `root`) or makes
/// something run, it is held and left alone. When a write fails, the
/// files already written are put back.
pub fn apply_batch(
    root: &Path,
    skip: &SkipList,
    batch: &TeamChangeBatch,
    unsaved: &HashSet<String>,
    caller: &TeamCaller,
    now: &str,
) -> Result<BatchRecord, String> {
    // Paths are compared without regard to case, as on a case-insensitive
    // disk two spellings name one file.
    let unsaved: HashSet<String> = unsaved.iter().map(|path| fold_case(path)).collect();
    let mut spelling = SpellingCheck::default();
    let mut seen = HashSet::new();
    let mut checked = Vec::with_capacity(batch.changes.len());
    for change in &batch.changes {
        let path = change.path.as_str();
        if !seen.insert(fold_case(path)) {
            return Err(format!("{path} is in the batch twice"));
        }
        // The same rule the teammate's copy follows, so a file it shows as
        // sendable isn't refused here; a respelled existing folder is caught
        // by the spelling check below.
        if skip.skips(path) {
            return Err(format!("{path} can't be changed by teammates"));
        }
        let full_path = safe_relative_path(root, path)?;
        spelling.check(root, path)?;
        let before = read_existing(path, &full_path)?;
        checked.push((change, full_path, before));
    }

    let mut files = Vec::with_capacity(checked.len());
    let mut to_write = Vec::new();
    for (change, full_path, before) in checked {
        let path = change.path.as_str();
        let current_hash = before.as_deref().map(file_hash);
        let already_there = current_hash == change.content.as_deref().map(file_hash);
        let state = if already_there {
            BatchFileState::Applied
        } else if unsaved.contains(&fold_case(path)) {
            BatchFileState::Held(HeldReason::UnsavedInEditor)
        } else if current_hash != change.base_hash {
            BatchFileState::Held(HeldReason::ChangedSinceYourCopy)
        } else if needs_approval(path) {
            BatchFileState::Held(HeldReason::NeedsApproval)
        } else {
            to_write.push((full_path, files.len()));
            BatchFileState::Applied
        };
        files.push(BatchFile {
            path: change.path.clone(),
            before,
            after: change.content.clone(),
            state,
        });
    }

    let mut written: Vec<(&Path, usize, Option<std::fs::Permissions>)> = Vec::new();
    for (full_path, index) in &to_write {
        let Some(file) = files.get_mut(*index) else {
            continue;
        };
        match write_checked_file(root, file, full_path) {
            Ok(Some(permissions)) => written.push((full_path.as_path(), *index, permissions)),
            Ok(None) => {}
            Err(error) => {
                for (written_path, index, permissions) in written.into_iter().rev() {
                    let before = files.get(index).and_then(|file| file.before.as_deref());
                    if let Err(undo_error) =
                        write_batch_file(written_path, before, permissions.as_ref())
                    {
                        log::error!(
                            "Couldn't put back {} after a failed batch: {undo_error}",
                            written_path.display()
                        );
                    }
                }
                return Err(error);
            }
        }
    }

    Ok(BatchRecord {
        id: batch_id(now),
        login: caller.login.clone(),
        name: caller.name.clone(),
        at: now.to_string(),
        note: batch.note.clone(),
        files,
    })
}

/// Puts each file of `record` back to what it held before the batch, where
/// it still holds what the batch wrote and has no unsaved edits in an
/// editor (`unsaved`, relative to `root`), and returns the paths left
/// alone. Files still held are kept as they are. `record` is updated file
/// by file, so it stays true to the disk when a write fails part way.
pub fn undo_batch(
    root: &Path,
    record: &mut BatchRecord,
    unsaved: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let unsaved: HashSet<String> = unsaved.iter().map(|path| fold_case(path)).collect();
    let mut left_alone = Vec::new();
    for file in &mut record.files {
        match file.state {
            BatchFileState::Applied | BatchFileState::Taken => {}
            BatchFileState::Held(_) => {
                file.state = BatchFileState::Kept;
                continue;
            }
            BatchFileState::Kept | BatchFileState::Undone => continue,
        }
        if unsaved.contains(&fold_case(&file.path)) {
            left_alone.push(file.path.clone());
            continue;
        }
        // A fresh spelling check per file, as putting back an earlier file
        // can add a name to a folder already listed.
        let checked = safe_relative_path(root, &file.path).and_then(|full_path| {
            SpellingCheck::default().check(root, &file.path)?;
            Ok(full_path)
        });
        let full_path = match checked {
            Ok(full_path) => full_path,
            Err(error) => {
                log::warn!("Not undoing {}: {error}", file.path);
                left_alone.push(file.path.clone());
                continue;
            }
        };
        let replaced = replace_if_unchanged(
            root,
            &file.path,
            &full_path,
            file.after.as_deref(),
            file.before.as_deref(),
        )?;
        match replaced {
            Replaced::Written(_) => file.state = BatchFileState::Undone,
            Replaced::Changed(_) => left_alone.push(file.path.clone()),
        }
    }
    Ok(left_alone)
}

/// Settles a held file of `record`: keeps the owner's version, or writes
/// the teammate's when the file still holds what it did when it was held.
/// A file changed since stays held, now showing what it holds, and is left
/// alone.
pub fn resolve_held(
    root: &Path,
    record: &mut BatchRecord,
    path: &str,
    take_teammates: bool,
) -> Result<(), String> {
    let file = record
        .files
        .iter_mut()
        .find(|file| file.path == path)
        .ok_or_else(|| format!("{path} isn't in this batch"))?;
    if !matches!(file.state, BatchFileState::Held(_)) {
        return Err(format!("{path} isn't held"));
    }
    if !take_teammates {
        file.state = BatchFileState::Kept;
        return Ok(());
    }
    let full_path = safe_relative_path(root, path)?;
    SpellingCheck::default().check(root, path)?;
    let replaced = replace_if_unchanged(
        root,
        path,
        &full_path,
        file.before.as_deref(),
        file.after.as_deref(),
    )?;
    match replaced {
        Replaced::Written(_) => {
            file.state = BatchFileState::Taken;
            Ok(())
        }
        Replaced::Changed(now) => {
            file.before = now;
            Err(format!(
                "{path} changed again since it was held, so it was left alone"
            ))
        }
    }
}

/// The lock held while a batch is applied, undone or resolved in a
/// workspace, so two of them never interleave on its files.
pub fn workspace_files_lock(workspace_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(workspace_id.to_string())
        .or_default()
        .clone()
}

fn batches_dir(data_dir: &Path, workspace_id: &str) -> Result<PathBuf, String> {
    if !is_plain_id(workspace_id) {
        return Err(format!("{workspace_id} isn't a workspace id"));
    }
    Ok(data_dir.join("team-batches").join(workspace_id))
}

pub fn save_batch_record(
    data_dir: &Path,
    workspace_id: &str,
    record: &BatchRecord,
) -> Result<(), String> {
    if !is_plain_id(&record.id) {
        return Err(format!("{} isn't a batch id", record.id));
    }
    let json = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    let full_path = batches_dir(data_dir, workspace_id)?.join(format!("{}.json", record.id));
    replace_atomically(&full_path, |temp_path| {
        std::fs::write(temp_path, &json)
            .map_err(|error| format!("could not save {}: {error}", full_path.display()))
    })
}

/// The workspace's batches, oldest first.
pub fn load_batch_records(data_dir: &Path, workspace_id: &str) -> Result<Vec<BatchRecord>, String> {
    let directory = batches_dir(data_dir, workspace_id)?;
    let read_dir = match std::fs::read_dir(&directory) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not read {}: {error}", directory.display())),
    };
    let mut records = Vec::new();
    for entry in read_dir {
        let path = entry
            .map_err(|error| format!("could not read {}: {error}", directory.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let parsed = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<BatchRecord>(&bytes).map_err(|error| error.to_string())
            });
        match parsed {
            Ok(record) => records.push(record),
            Err(error) => log::error!("Couldn't read {}: {error}", path.display()),
        }
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::team_share_service::TeamCaller;

    #[test]
    fn the_secret_floor_holds_with_no_patterns() {
        let empty = SkipList { patterns: vec![] };
        for path in [
            ".env.local",
            "key.pem",
            ".claude/settings.local.json",
            "a/.GIT/config",
        ] {
            assert!(empty.skips(path), "{path}");
        }
        assert!(!empty.skips("src/main.rs"));
    }

    #[test]
    fn remotes_compare_equal_whatever_their_spelling() {
        for spelling in [
            "git@GitHub.com:Acme/App.git",
            "https://github.com/Acme/App.git",
            "https://github.com/Acme/App/",
            "ssh://git@github.com/Acme/App.git",
            "  https://user@GITHUB.com/Acme/App  ",
        ] {
            assert_eq!(
                normalize_remote_url(spelling),
                "github.com/Acme/App",
                "{spelling}"
            );
        }
    }

    #[allow(clippy::disallowed_methods)]
    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write(root: &Path, path: &str, content: &str) {
        let full_path = root.join(path);
        std::fs::create_dir_all(full_path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(full_path, content).expect("write");
    }

    fn repo() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "t"]);
        write(
            root,
            ".gitignore",
            ".context/\ntarget/\nnode_modules/\n.env*\n*.pem\n.claude/settings.local.json\n",
        );
        write(root, "kept.txt", "kept");
        write(root, "changed.txt", "one");
        write(root, "gone.txt", "gone");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        directory
    }

    fn paths(manifest: &TeamFileManifest) -> Vec<&str> {
        manifest
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect()
    }

    #[test]
    fn scan_lists_only_what_differs_from_head() {
        let directory = repo();
        let root = directory.path();
        write(root, "changed.txt", "two");
        write(root, "new.txt", "new");
        write(root, ".context/notes.md", "notes");
        std::fs::remove_file(root.join("gone.txt")).expect("remove");

        let manifest = scan_workspace(root, &SkipList::default()).expect("scan");
        let head = String::from_utf8(run_git(root, &["rev-parse", "HEAD"]).expect("rev-parse"))
            .expect("utf8");
        assert_eq!(manifest.head_commit, head.trim());
        assert_eq!(
            paths(&manifest),
            vec![".context/notes.md", "changed.txt", "gone.txt", "new.txt"]
        );
        let gone = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "gone.txt")
            .expect("gone entry");
        assert_eq!(gone.kind, TeamFileKind::Deleted);
        let changed = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "changed.txt")
            .expect("changed entry");
        assert_eq!(changed.hash, file_hash(b"two"));
    }

    #[test]
    fn scan_skips_build_dirs_and_secrets() {
        let directory = repo();
        let root = directory.path();
        write(root, "target/x", "x");
        write(root, "node_modules/y", "y");
        write(root, ".env", "s");
        write(root, ".env.local", "s");
        write(root, "key.pem", "s");
        write(root, ".claude/settings.local.json", "{}");
        write(root, ".context/plan.md", "plan");
        write(root, ".context/node_modules/x", "x");

        let manifest = scan_workspace(root, &SkipList::default()).expect("scan");
        assert_eq!(paths(&manifest), vec![".context/plan.md"]);

        let skip = SkipList::default();
        assert!(skip.skips(".Git/config"));
        assert!(skip.skips(".GIT/hooks/x"));
    }

    #[test]
    fn scan_skips_secrets_in_any_spelling_but_keeps_capitalised_build_files() {
        let directory = repo();
        let root = directory.path();
        write(root, "BUILD", "bazel");
        write(root, "Dist/x", "x");
        write(root, ".ENV", "s");
        write(root, "x.\u{212A}ey", "s");

        let manifest = scan_workspace(root, &SkipList::default()).expect("scan");
        let mut scanned = paths(&manifest);
        scanned.sort_unstable();
        assert_eq!(scanned, vec!["BUILD", "Dist/x"]);
    }

    #[cfg(unix)]
    #[test]
    fn scan_keeps_in_tree_symlinks_and_drops_outside_ones() {
        let directory = repo();
        let root = directory.path();
        write(root, ".rules", "rules");
        std::os::unix::fs::symlink(".rules", root.join("CLAUDE.md")).expect("link");
        std::os::unix::fs::symlink("/tmp", root.join("outside")).expect("link");

        let manifest = scan_workspace(root, &SkipList::default()).expect("scan");
        let claude = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "CLAUDE.md")
            .expect("CLAUDE.md entry");
        assert_eq!(
            claude.kind,
            TeamFileKind::Symlink {
                target: ".rules".to_string()
            }
        );
        assert!(!paths(&manifest).contains(&"outside"));
    }

    #[cfg(unix)]
    #[test]
    fn safe_relative_path_refuses_escapes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        std::fs::create_dir(root.join("real")).expect("mkdir");
        std::os::unix::fs::symlink("real", root.join("link")).expect("link");
        std::os::unix::fs::symlink("/etc/hosts", root.join("linkfile")).expect("link");

        for bad in [
            "../x",
            "/etc/passwd",
            ".git/config",
            "a/../../x",
            "link/file.txt",
            ".Git/config",
            ".GIT/hooks/x",
            "linkfile",
            "a/./b",
            "a//b",
            "a/",
        ] {
            assert!(
                safe_relative_path(root, bad).is_err(),
                "{bad} should be refused"
            );
        }
        assert_eq!(
            safe_relative_path(root, ".context/plan.md"),
            Ok(root.join(".context/plan.md"))
        );
    }

    #[test]
    fn needs_approval_matches_command_running_files() {
        for path in [
            ".mnemonic/config.json",
            ".claude/settings.json",
            ".zed/settings.json",
            ".vscode/tasks.json",
            ".envrc",
            "sub/.envrc",
        ] {
            assert!(needs_approval(path), "{path}");
        }
        assert!(!needs_approval(".context/plan.md"));
    }

    #[test]
    fn team_file_change_content_round_trips_as_base64() {
        let change = TeamFileChange {
            path: "a.bin".to_string(),
            base_hash: None,
            content: Some(vec![0, 255, 7]),
        };
        let json = serde_json::to_string(&change).expect("serialize");
        assert!(json.contains("\"AP8H\""));
        assert_eq!(
            serde_json::from_str::<TeamFileChange>(&json).expect("parse"),
            change
        );
    }

    fn head_of(root: &Path) -> String {
        String::from_utf8_lossy(&run_git(root, &["rev-parse", "HEAD"]).expect("rev-parse"))
            .trim()
            .to_string()
    }

    #[test]
    fn an_unpushed_commit_reaches_the_teammate() {
        let sharer = repo();
        let origin = tempfile::tempdir().expect("tempdir");
        git(origin.path(), &["init", "-q", "--bare"]);
        let origin_path = origin.path().to_string_lossy().to_string();
        git(sharer.path(), &["remote", "add", "origin", &origin_path]);
        git(
            sharer.path(),
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
        );
        git(sharer.path(), &["fetch", "-q", "origin"]);
        let teammate_parent = tempfile::tempdir().expect("tempdir");
        let teammate = teammate_parent.path().join("clone");
        git(
            teammate_parent.path(),
            &["clone", "-q", &origin_path, "clone"],
        );

        write(sharer.path(), "local.txt", "unpushed");
        git(sharer.path(), &["add", "-A"]);
        git(sharer.path(), &["commit", "-q", "-m", "unpushed"]);
        let head = head_of(sharer.path());
        assert!(run_git(&teammate, &["cat-file", "-e", &head]).is_err());

        let bundle = bundle_for(sharer.path(), &[]).expect("bundle");
        assert!(!bundle.is_empty());
        fetch_bundle(&teammate, &bundle, &head).expect("fetch");
        run_git(&teammate, &["cat-file", "-e", &head]).expect("head present");
    }

    #[test]
    fn a_sharer_with_no_origin_still_bundles_head() {
        let sharer = repo();
        let head = head_of(sharer.path());
        let teammate = tempfile::tempdir().expect("tempdir");
        git(teammate.path(), &["init", "-q"]);

        let bundle = bundle_for(sharer.path(), &[]).expect("bundle");
        fetch_bundle(teammate.path(), &bundle, &head).expect("fetch");
        run_git(teammate.path(), &["cat-file", "-e", &head]).expect("head present");
    }

    #[test]
    fn a_teammate_who_has_the_head_gets_an_empty_bundle() {
        let sharer = repo();
        let head = head_of(sharer.path());
        let bundle = bundle_for(sharer.path(), std::slice::from_ref(&head)).expect("bundle");
        assert!(bundle.is_empty());
        fetch_bundle(sharer.path(), &bundle, &head).expect("already present");
    }

    fn copy_state(head: &str, synced: &[(&str, &str)]) -> CopyState {
        CopyState {
            host: "jay-mac".to_string(),
            sharer_name: "Jay".to_string(),
            workspace_id: "ws-1".to_string(),
            local_workspace_id: "ws-local".to_string(),
            base_branch: None,
            head_commit: head.to_string(),
            synced: synced
                .iter()
                .map(|(path, hash)| (path.to_string(), hash.to_string()))
                .collect(),
            skip_patterns: Vec::new(),
        }
    }

    fn manifest(head: &str, files: &[(&str, &str)]) -> TeamFileManifest {
        TeamFileManifest {
            head_commit: head.to_string(),
            entries: files
                .iter()
                .map(|(path, hash)| TeamFileEntry {
                    path: path.to_string(),
                    kind: TeamFileKind::File { executable: false },
                    size: 1,
                    hash: hash.to_string(),
                })
                .collect(),
        }
    }

    fn hashes(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(path, hash)| (path.to_string(), hash.to_string()))
            .collect()
    }

    #[test]
    fn an_untouched_file_follows_the_sharer() {
        let state = copy_state("aaaaaaa", &[(".context/plan.md", "one")]);
        let local = hashes(&[(".context/plan.md", "one")]);
        let incoming = manifest("aaaaaaa", &[(".context/plan.md", "two")]);

        let plan = plan_follow(&state, &local, &HashSet::new(), &incoming, false);
        assert_eq!(plan.write, vec![".context/plan.md".to_string()]);
        assert!(plan.both_changed.is_empty());
        assert_eq!(plan.waiting_reason, None);
    }

    #[test]
    fn a_touched_file_is_kept_and_marked_both_changed() {
        let state = copy_state("aaaaaaa", &[(".context/plan.md", "one")]);
        let local = hashes(&[(".context/plan.md", "mine")]);
        let dirty = HashSet::from([".context/plan.md".to_string()]);
        let incoming = manifest("aaaaaaa", &[(".context/plan.md", "two")]);

        let plan = plan_follow(&state, &local, &dirty, &incoming, false);
        assert!(plan.write.is_empty());
        assert_eq!(plan.both_changed, vec![".context/plan.md".to_string()]);
    }

    #[test]
    fn a_new_head_waits_while_there_are_unsent_edits() {
        let state = copy_state("aaaaaaa", &[("src/lib.rs", "one")]);
        let local = hashes(&[("src/lib.rs", "mine")]);
        let dirty = HashSet::from(["src/lib.rs".to_string()]);
        let incoming = manifest("bbbbbbb", &[]);

        let plan = plan_follow(&state, &local, &dirty, &incoming, false);
        assert!(!plan.move_head);
        assert!(
            plan.waiting_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("src/lib.rs"))
        );
        assert!(plan.restore.is_empty());
    }

    #[test]
    fn a_new_head_waits_while_the_teammate_has_local_commits() {
        let state = copy_state("aaaaaaa", &[("src/lib.rs", "one")]);
        let local = hashes(&[("src/lib.rs", "one")]);
        let dirty = HashSet::from(["src/lib.rs".to_string()]);
        let incoming = manifest("bbbbbbb", &[]);

        let waiting = plan_follow(&state, &local, &dirty, &incoming, true);
        assert!(!waiting.move_head);
        assert!(waiting.waiting_reason.is_some());

        let following = plan_follow(&state, &local, &dirty, &incoming, false);
        assert!(following.move_head);
        assert_eq!(following.restore, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn a_copy_follows_a_new_commit_over_the_sharers_files() {
        let sharer = repo();
        let teammate = tempfile::tempdir().expect("tempdir");
        let sharer_path = sharer.path().to_string_lossy().to_string();
        git(teammate.path(), &["clone", "-q", &sharer_path, "copy"]);
        let copy = teammate.path().join("copy");
        write(sharer.path(), "changed.txt", "two");
        write(sharer.path(), ".context/notes.md", "notes");
        let fetch =
            |path: &str| std::fs::read(sharer.path().join(path)).map_err(|error| error.to_string());
        let mut state = copy_state(&head_of(sharer.path()), &[]);

        let first = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        follow_manifest(&copy, &mut state, &first, &fetch, &|_, _| {}).expect("first follow");
        assert_eq!(
            std::fs::read_to_string(copy.join("changed.txt")).expect("read"),
            "two"
        );

        git(sharer.path(), &["commit", "-q", "-am", "two"]);
        git(&copy, &["fetch", "-q", "origin"]);
        let second = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        let plan =
            follow_manifest(&copy, &mut state, &second, &fetch, &|_, _| {}).expect("second follow");

        assert!(plan.move_head);
        assert_eq!(head_of(&copy), head_of(sharer.path()));
        assert_eq!(
            std::fs::read_to_string(copy.join("changed.txt")).expect("read"),
            "two"
        );
        assert_eq!(
            std::fs::read_to_string(copy.join(".context/notes.md")).expect("read"),
            "notes"
        );
        assert_eq!(
            local_edits(&state, &copy, &SkipList::default()).expect("edits"),
            Vec::<String>::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_copy_is_not_recreated() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let link = |path: &str, target: &str| TeamFileEntry {
            path: path.to_string(),
            kind: TeamFileKind::Symlink {
                target: target.to_string(),
            },
            size: 0,
            hash: String::new(),
        };
        let fetch = |_: &str| Err("no fetching".to_string());

        for (path, target) in [
            ("a/out", "../../etc"),
            ("abs", "/etc/hosts"),
            ("g", ".git/config"),
        ] {
            assert_eq!(
                write_copy_entry(root, &link(path, target), None, &fetch),
                Ok(CopyWrite::Refused)
            );
            assert!(
                std::fs::symlink_metadata(root.join(path)).is_err(),
                "{path}"
            );
        }
        write_copy_entry(root, &link("CLAUDE.md", ".rules"), None, &fetch).expect("in-tree link");
        assert_eq!(
            std::fs::read_link(root.join("CLAUDE.md")).expect("link"),
            PathBuf::from(".rules")
        );
    }

    fn clone_of(sharer: &Path, parent: &Path) -> PathBuf {
        let sharer_path = sharer.to_string_lossy().to_string();
        git(parent, &["clone", "-q", &sharer_path, "copy"]);
        let copy = parent.join("copy");
        git(&copy, &["config", "user.email", "t@example.com"]);
        git(&copy, &["config", "user.name", "t"]);
        copy
    }

    #[test]
    fn a_committed_edit_is_kept_and_marked_both_changed() {
        let sharer = repo();
        let teammate = tempfile::tempdir().expect("tempdir");
        let copy = clone_of(sharer.path(), teammate.path());
        let fetch =
            |path: &str| std::fs::read(sharer.path().join(path)).map_err(|error| error.to_string());
        let mut state = copy_state(&head_of(sharer.path()), &[]);
        write(&copy, "kept.txt", "teammate");
        git(&copy, &["commit", "-q", "-am", "teammate"]);
        write(sharer.path(), "kept.txt", "sharer");

        let manifest = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        let plan =
            follow_manifest(&copy, &mut state, &manifest, &fetch, &|_, _| {}).expect("follow");

        assert_eq!(plan.both_changed, vec!["kept.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(copy.join("kept.txt")).expect("read"),
            "teammate"
        );
    }

    #[test]
    fn a_file_that_fails_to_arrive_after_a_new_commit_still_follows() {
        let sharer = repo();
        let teammate = tempfile::tempdir().expect("tempdir");
        let copy = clone_of(sharer.path(), teammate.path());
        let fetch =
            |path: &str| std::fs::read(sharer.path().join(path)).map_err(|error| error.to_string());
        let mut state = copy_state(&head_of(sharer.path()), &[]);
        write(sharer.path(), "changed.txt", "two");
        let first = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        follow_manifest(&copy, &mut state, &first, &fetch, &|_, _| {}).expect("first follow");

        git(sharer.path(), &["commit", "-q", "-am", "two"]);
        write(sharer.path(), "changed.txt", "three");
        git(&copy, &["fetch", "-q", "origin"]);
        let second = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        let offline = |_: &str| Err("offline".to_string());
        assert!(follow_manifest(&copy, &mut state, &second, &offline, &|_, _| {}).is_err());
        assert_eq!(head_of(&copy), head_of(sharer.path()));

        let plan = follow_manifest(&copy, &mut state, &second, &fetch, &|_, _| {}).expect("retry");
        assert!(plan.both_changed.is_empty());
        assert_eq!(
            std::fs::read_to_string(copy.join("changed.txt")).expect("read"),
            "three"
        );
    }

    #[test]
    fn a_tip_the_sharer_lacks_doesnt_stop_the_bundle() {
        let sharer = repo();
        let head = head_of(sharer.path());
        let unknown = "1".repeat(40);
        let bundle = bundle_for(sharer.path(), &[unknown]).expect("bundle");
        let teammate = tempfile::tempdir().expect("tempdir");
        git(teammate.path(), &["init", "-q"]);
        fetch_bundle(teammate.path(), &bundle, &head).expect("fetch");
    }

    fn read(root: &Path, path: &str) -> Option<String> {
        std::fs::read_to_string(root.join(path)).ok()
    }

    fn change(path: &str, base: Option<&str>, content: Option<&str>) -> TeamFileChange {
        TeamFileChange {
            path: path.to_string(),
            base_hash: base.map(|base| file_hash(base.as_bytes())),
            content: content.map(|content| content.as_bytes().to_vec()),
        }
    }

    fn batch(changes: Vec<TeamFileChange>) -> TeamChangeBatch {
        TeamChangeBatch {
            note: Some("tidied the plan".to_string()),
            changes,
        }
    }

    fn dan() -> TeamCaller {
        TeamCaller {
            login: "dan@example.com".to_string(),
            name: "Dan".to_string(),
        }
    }

    const NOW: &str = "2026-11-16T14:14:05.123Z";

    fn apply(
        root: &Path,
        changes: Vec<TeamFileChange>,
        unsaved: &[&str],
    ) -> Result<BatchRecord, String> {
        let unsaved = unsaved.iter().map(|path| path.to_string()).collect();
        apply_batch(
            root,
            &SkipList::default(),
            &batch(changes),
            &unsaved,
            &dan(),
            NOW,
        )
    }

    fn state_of<'a>(record: &'a BatchRecord, path: &str) -> &'a BatchFile {
        record
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} is in the record"))
    }

    #[test]
    fn a_matching_base_applies_and_records_before_and_after() {
        let directory = repo();
        let root = directory.path();
        write(root, ".context/plan.md", "one");
        #[cfg(unix)]
        set_executable(&root.join(".context/plan.md"), true).expect("chmod");

        let record = apply(
            root,
            vec![
                change(".context/plan.md", Some("one"), Some("two")),
                change("notes/new.md", None, Some("hello")),
                change("gone.txt", Some("gone"), None),
            ],
            &[],
        )
        .expect("applies");

        assert_eq!(read(root, ".context/plan.md").as_deref(), Some("two"));
        assert_eq!(read(root, "notes/new.md").as_deref(), Some("hello"));
        assert_eq!(read(root, "gone.txt"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join(".context/plan.md"))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "kept executable");
        }

        assert_eq!(record.login, "dan@example.com");
        assert_eq!(record.name, "Dan");
        assert_eq!(record.at, NOW);
        assert_eq!(record.note.as_deref(), Some("tidied the plan"));
        let plan = state_of(&record, ".context/plan.md");
        assert_eq!(plan.state, BatchFileState::Applied);
        assert_eq!(plan.before.as_deref(), Some(b"one".as_slice()));
        assert_eq!(plan.after.as_deref(), Some(b"two".as_slice()));
        let new = state_of(&record, "notes/new.md");
        assert_eq!(new.before, None);
        assert_eq!(new.after.as_deref(), Some(b"hello".as_slice()));
        let gone = state_of(&record, "gone.txt");
        assert_eq!(gone.before.as_deref(), Some(b"gone".as_slice()));
        assert_eq!(gone.after, None);

        let outcome = record.outcome();
        assert_eq!(outcome.batch_id, record.id);
        assert_eq!(outcome.applied.len(), 3);
        assert!(outcome.held.is_empty());
    }

    #[test]
    fn a_changed_base_is_held_and_untouched() {
        let directory = repo();
        let root = directory.path();
        write(root, "changed.txt", "jay's");

        let record = apply(
            root,
            vec![
                change("changed.txt", Some("one"), Some("dan's")),
                change("kept.txt", Some("kept"), Some("kept by dan")),
            ],
            &[],
        )
        .expect("applies the rest");

        assert_eq!(read(root, "changed.txt").as_deref(), Some("jay's"));
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept by dan"));
        let held = state_of(&record, "changed.txt");
        assert_eq!(
            held.state,
            BatchFileState::Held(HeldReason::ChangedSinceYourCopy)
        );
        assert_eq!(held.before.as_deref(), Some(b"jay's".as_slice()));
        assert_eq!(held.after.as_deref(), Some(b"dan's".as_slice()));
        assert_eq!(
            record.outcome().held,
            vec![HeldFile {
                path: "changed.txt".to_string(),
                reason: HeldReason::ChangedSinceYourCopy,
            }]
        );
    }

    #[test]
    fn a_command_running_file_is_held_for_approval() {
        let directory = repo();
        let root = directory.path();

        let record = apply(
            root,
            vec![
                change(".claude/settings.json", None, Some("{\"hooks\":{}}")),
                change(".envrc", None, Some("export X=1")),
                change(".Claude/settings.local2.json", None, Some("{}")),
                change("sub/.ENVRC", None, Some("export X=1")),
            ],
            &[],
        )
        .expect("holds");

        assert_eq!(read(root, ".claude/settings.json"), None);
        assert_eq!(read(root, ".envrc"), None);
        assert_eq!(read(root, ".Claude/settings.local2.json"), None);
        assert_eq!(read(root, "sub/.ENVRC"), None);
        for file in &record.files {
            assert_eq!(
                file.state,
                BatchFileState::Held(HeldReason::NeedsApproval),
                "{}",
                file.path
            );
        }
    }

    #[test]
    fn a_file_with_unsaved_edits_is_held() {
        let directory = repo();
        let root = directory.path();

        let record = apply(
            root,
            vec![change("kept.txt", Some("kept"), Some("dan's"))],
            &["kept.txt"],
        )
        .expect("holds");

        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept"));
        assert_eq!(
            state_of(&record, "kept.txt").state,
            BatchFileState::Held(HeldReason::UnsavedInEditor)
        );
    }

    #[test]
    fn a_skipped_or_escaping_path_rejects_the_whole_batch() {
        let directory = repo();
        let root = directory.path();
        let outside = tempfile::tempdir().expect("tempdir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.join("link")).expect("link");

        let mut bad_paths = vec![
            ".env",
            ".ENV",
            "config/.env.local",
            "server.pem",
            "X.PEM",
            "node_modules/x.js",
            "x.\u{212A}ey",
            ".claude/\u{17F}ettings.local.json",
            ".git/config",
            "../outside.txt",
            "/tmp/outside.txt",
            "a/../kept.txt",
            "",
        ];
        if cfg!(unix) {
            bad_paths.push("link/x.txt");
        }
        for bad_path in bad_paths {
            let result = apply(
                root,
                vec![
                    change("kept.txt", Some("kept"), Some("dan's")),
                    change(bad_path, None, Some("bad")),
                ],
                &[],
            );
            assert!(result.is_err(), "{bad_path:?} rejects the batch");
            assert_eq!(
                read(root, "kept.txt").as_deref(),
                Some("kept"),
                "{bad_path:?}"
            );
        }
        assert!(
            std::fs::read_dir(outside.path())
                .expect("read")
                .next()
                .is_none()
        );

        let twice = apply(
            root,
            vec![
                change("kept.txt", Some("kept"), Some("first")),
                change("kept.txt", Some("kept"), Some("second")),
            ],
            &[],
        );
        assert!(twice.is_err(), "a path named twice rejects the batch");
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept"));

        let two_spellings = apply(
            root,
            vec![
                change("a.txt", None, Some("first")),
                change("A.txt", None, Some("second")),
            ],
            &[],
        );
        assert!(
            two_spellings.is_err(),
            "two spellings of one path reject the batch"
        );
        assert_eq!(read(root, "a.txt"), None);
    }

    #[test]
    fn a_differently_spelled_existing_path_is_refused_or_held() {
        let directory = repo();
        let root = directory.path();
        write(root, ".vscode/tasks.json", "{}");

        let case_insensitive_disk = root.join("KEPT.TXT").exists();

        let result = apply(
            root,
            vec![change("Kept.txt", Some("kept"), Some("evil"))],
            &[],
        );
        assert_eq!(result.is_err(), case_insensitive_disk);
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept"));

        for spelling in [".v\u{17F}code/tasks.json", ".VSCode/tasks.json"] {
            let result = apply(root, vec![change(spelling, Some("{}"), Some("evil"))], &[]);
            // A case-insensitive disk opens the real file under this
            // spelling, so it is refused; elsewhere it is a new file that
            // still needs approval.
            if case_insensitive_disk {
                assert!(result.is_err(), "{spelling}");
            } else if let Ok(record) = result {
                assert_eq!(
                    state_of(&record, spelling).state,
                    BatchFileState::Held(HeldReason::NeedsApproval),
                    "{spelling}"
                );
            }
            assert_eq!(
                read(root, ".vscode/tasks.json").as_deref(),
                Some("{}"),
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_failed_write_puts_back_what_was_written() {
        let directory = repo();
        let root = directory.path();
        write(root, "tool.sh", "#!/bin/sh");
        #[cfg(unix)]
        set_executable(&root.join("tool.sh"), true).expect("chmod");

        // "blocker" is written as a file first, so its folder can't be made.
        let result = apply(
            root,
            vec![
                change("tool.sh", Some("#!/bin/sh"), None),
                change("kept.txt", Some("kept"), Some("dan's")),
                change("blocker", None, Some("a file")),
                change("blocker/inside.txt", None, Some("inside")),
            ],
            &[],
        );

        assert!(result.is_err());
        assert_eq!(read(root, "tool.sh").as_deref(), Some("#!/bin/sh"));
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept"));
        assert!(!root.join("blocker").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("tool.sh"))
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "kept its mode");
        }
    }

    #[test]
    fn a_new_file_that_now_exists_is_held() {
        let directory = repo();
        let root = directory.path();
        write(root, ".context/new.md", "jay's");

        let record = apply(
            root,
            vec![change(".context/new.md", None, Some("dan's"))],
            &[],
        )
        .expect("holds");

        assert_eq!(read(root, ".context/new.md").as_deref(), Some("jay's"));
        assert_eq!(
            state_of(&record, ".context/new.md").state,
            BatchFileState::Held(HeldReason::ChangedSinceYourCopy)
        );
    }

    #[test]
    fn a_file_from_the_sharers_head_is_sent_with_its_head_hash() {
        let directory = repo();
        let root = directory.path();
        let head = String::from_utf8_lossy(&run_git(root, &["rev-parse", "HEAD"]).expect("head"))
            .trim()
            .to_string();
        write(root, ".context/plan.md", "dan's plan");
        write(root, "changed.txt", "dan's");
        write(root, "new.txt", "new");
        std::fs::remove_file(root.join("gone.txt")).expect("rm");
        let state = copy_state(&head, &[(".context/plan.md", "jay-plan-hash")]);

        let outgoing = build_batch(
            &state,
            root,
            &[
                ".context/plan.md".to_string(),
                "changed.txt".to_string(),
                "gone.txt".to_string(),
                "new.txt".to_string(),
            ],
            None,
        )
        .expect("builds");

        let base_of = |path: &str| {
            outgoing
                .batch
                .changes
                .iter()
                .find(|change| change.path == path)
                .map(|change| change.base_hash.clone())
                .unwrap_or_else(|| panic!("{path} is sent"))
        };
        assert_eq!(
            base_of(".context/plan.md").as_deref(),
            Some("jay-plan-hash")
        );
        assert_eq!(base_of("changed.txt"), Some(file_hash(b"one")));
        assert_eq!(base_of("gone.txt"), Some(file_hash(b"gone")));
        assert_eq!(base_of("new.txt"), None);
        let gone = outgoing
            .batch
            .changes
            .iter()
            .find(|change| change.path == "gone.txt")
            .expect("sent");
        assert_eq!(gone.content, None);
    }

    #[test]
    fn a_send_during_a_follow_round_is_not_overwritten() {
        let started_from = copy_state("aaaaaaa", &[("a.md", "a1"), ("b.md", "b1"), ("c.md", "c1")]);
        let mut round = started_from.clone();
        round.head_commit = "bbbbbbb".to_string();
        round.synced.insert("a.md".to_string(), "a2".to_string());
        round.synced.insert("b.md".to_string(), "b2".to_string());
        round.synced.remove("c.md");
        let mut live = started_from.clone();
        live.synced.insert("b.md".to_string(), "b-sent".to_string());
        live.synced.insert("d.md".to_string(), "d-sent".to_string());

        let merged = merge_follow_round(&started_from, round, &live);

        assert_eq!(merged.head_commit, "bbbbbbb");
        assert_eq!(merged.synced.get("a.md").map(String::as_str), Some("a2"));
        assert_eq!(
            merged.synced.get("b.md").map(String::as_str),
            Some("b-sent")
        );
        assert_eq!(merged.synced.get("c.md"), None);
        assert_eq!(
            merged.synced.get("d.md").map(String::as_str),
            Some("d-sent")
        );
    }

    #[test]
    fn undo_restores_files_that_still_hold_the_batch() {
        let directory = repo();
        let root = directory.path();
        let mut record = apply(
            root,
            vec![
                change("changed.txt", Some("one"), Some("two")),
                change("notes/new.md", None, Some("hello")),
                change("gone.txt", Some("gone"), None),
            ],
            &[],
        )
        .expect("applies");

        let left_alone = undo_batch(root, &mut record, &HashSet::new()).expect("undoes");

        assert!(left_alone.is_empty(), "{left_alone:?}");
        assert_eq!(read(root, "changed.txt").as_deref(), Some("one"));
        assert_eq!(read(root, "notes/new.md"), None);
        assert_eq!(read(root, "gone.txt").as_deref(), Some("gone"));
        for file in &record.files {
            assert_eq!(file.state, BatchFileState::Undone, "{}", file.path);
        }
    }

    #[test]
    fn undo_leaves_a_file_changed_since() {
        let directory = repo();
        let root = directory.path();
        let mut record = apply(
            root,
            vec![
                change("changed.txt", Some("one"), Some("two")),
                change("kept.txt", Some("kept"), Some("dan's")),
                change("notes/new.md", None, Some("hello")),
            ],
            &[],
        )
        .expect("applies");
        write(root, "changed.txt", "jay's since");
        std::fs::remove_file(root.join("notes/new.md")).expect("remove");

        let left_alone = undo_batch(root, &mut record, &HashSet::new()).expect("undoes the rest");

        assert_eq!(left_alone, vec!["changed.txt", "notes/new.md"]);
        assert_eq!(read(root, "changed.txt").as_deref(), Some("jay's since"));
        assert_eq!(read(root, "notes/new.md"), None);
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept"));
        assert_eq!(
            state_of(&record, "changed.txt").state,
            BatchFileState::Applied
        );
        assert_eq!(state_of(&record, "kept.txt").state, BatchFileState::Undone);
    }

    #[test]
    fn taking_a_held_file_writes_the_teammates_version() {
        let directory = repo();
        let root = directory.path();
        write(root, "changed.txt", "jay's");
        write(root, "kept.txt", "jay's too");
        let mut record = apply(
            root,
            vec![
                change("changed.txt", Some("one"), Some("dan's")),
                change("kept.txt", Some("kept"), Some("dan's too")),
            ],
            &[],
        )
        .expect("holds");

        resolve_held(root, &mut record, "changed.txt", true).expect("takes");

        assert_eq!(read(root, "changed.txt").as_deref(), Some("dan's"));
        assert_eq!(
            state_of(&record, "changed.txt").state,
            BatchFileState::Taken
        );

        // Jay's side wrote it again after it was held: taking it now would
        // overwrite that, so it is left alone and still held.
        write(root, "kept.txt", "jay's again");
        assert!(resolve_held(root, &mut record, "kept.txt", true).is_err());
        assert_eq!(read(root, "kept.txt").as_deref(), Some("jay's again"));
        assert_eq!(
            state_of(&record, "kept.txt").state,
            BatchFileState::Held(HeldReason::ChangedSinceYourCopy)
        );
    }

    #[test]
    fn keeping_a_held_file_changes_nothing() {
        let directory = repo();
        let root = directory.path();
        write(root, "changed.txt", "jay's");
        let mut record = apply(
            root,
            vec![change("changed.txt", Some("one"), Some("dan's"))],
            &[],
        )
        .expect("holds");

        resolve_held(root, &mut record, "changed.txt", false).expect("keeps");

        assert_eq!(read(root, "changed.txt").as_deref(), Some("jay's"));
        assert_eq!(state_of(&record, "changed.txt").state, BatchFileState::Kept);
    }

    #[test]
    fn undo_leaves_a_file_with_unsaved_edits() {
        let directory = repo();
        let root = directory.path();
        let mut record = apply(
            root,
            vec![
                change("changed.txt", Some("one"), Some("two")),
                change("kept.txt", Some("kept"), Some("dan's")),
            ],
            &[],
        )
        .expect("applies");

        let unsaved = HashSet::from(["Changed.TXT".to_string()]);
        let left_alone = undo_batch(root, &mut record, &unsaved).expect("undoes the rest");

        assert_eq!(left_alone, vec!["changed.txt"]);
        assert_eq!(read(root, "changed.txt").as_deref(), Some("two"));
        assert_eq!(read(root, "kept.txt").as_deref(), Some("kept"));
        assert_eq!(
            state_of(&record, "changed.txt").state,
            BatchFileState::Applied
        );
    }

    #[test]
    fn undo_keeps_yours_for_held_files_and_undoes_taken_ones() {
        let directory = repo();
        let root = directory.path();
        write(root, "changed.txt", "jay's");
        write(root, "kept.txt", "jay's too");
        let mut record = apply(
            root,
            vec![
                change("changed.txt", Some("one"), Some("dan's")),
                change("kept.txt", Some("kept"), Some("dan's too")),
            ],
            &[],
        )
        .expect("holds");
        resolve_held(root, &mut record, "changed.txt", true).expect("takes");

        let left_alone = undo_batch(root, &mut record, &HashSet::new()).expect("undoes");

        assert!(left_alone.is_empty(), "{left_alone:?}");
        assert_eq!(read(root, "changed.txt").as_deref(), Some("jay's"));
        assert_eq!(
            state_of(&record, "changed.txt").state,
            BatchFileState::Undone
        );
        assert_eq!(read(root, "kept.txt").as_deref(), Some("jay's too"));
        assert_eq!(state_of(&record, "kept.txt").state, BatchFileState::Kept);
    }

    #[test]
    fn a_copy_never_takes_the_sharers_mnemonic_config() {
        let sharer = repo();
        let teammate = tempfile::tempdir().expect("tempdir");
        let copy = clone_of(sharer.path(), teammate.path());
        write(
            sharer.path(),
            ".mnemonic/config.json",
            "{\"teardown\":[\"rm -rf ~\"]}",
        );
        write(sharer.path(), "changed.txt", "two");
        let fetch =
            |path: &str| std::fs::read(sharer.path().join(path)).map_err(|error| error.to_string());
        let mut state = copy_state(&head_of(sharer.path()), &[]);
        let mut manifest = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        assert!(paths(&manifest).contains(&".mnemonic/config.json"));
        manifest.entries.push(TeamFileEntry {
            path: ".MNEMONIC/hooks.json".to_string(),
            kind: TeamFileKind::File { executable: false },
            size: 2,
            hash: file_hash(b"{}"),
        });

        follow_manifest(&copy, &mut state, &manifest, &fetch, &|_, _| {}).expect("follow");

        assert_eq!(read(&copy, "changed.txt").as_deref(), Some("two"));
        assert!(std::fs::symlink_metadata(copy.join(".mnemonic")).is_err());
        assert!(std::fs::symlink_metadata(copy.join(".MNEMONIC")).is_err());
        write(&copy, ".Mnemonic/config.json", "{}");
        assert_eq!(
            local_edits(&state, &copy, &state.skip_list()).expect("edits"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn only_the_secret_floor_refuses_other_spellings() {
        let directory = repo();
        let root = directory.path();

        let record = apply(
            root,
            vec![
                change("BUILD", None, Some("bazel")),
                change("Dist/x", None, Some("x")),
            ],
            &[],
        )
        .expect("applied");

        assert_eq!(record.outcome().applied.len(), 2);
        assert_eq!(read(root, "BUILD").as_deref(), Some("bazel"));
        assert_eq!(read(root, "Dist/x").as_deref(), Some("x"));
        assert!(apply(root, vec![change(".ENV", None, Some("SECRET=1"))], &[]).is_err());
        assert_eq!(read(root, ".ENV"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_rescan_reads_only_files_that_changed() {
        use std::os::unix::fs::PermissionsExt;
        let directory = repo();
        let root = directory.path();
        let notes = root.join("notes.md");
        write(root, "notes.md", "one");
        let mut cache = HashCache::default();
        let first = scan_workspace_cached(root, &SkipList::default(), &mut cache).expect("scan");

        std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let second = scan_workspace_cached(root, &SkipList::default(), &mut cache);
        std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(second.expect("an unchanged file isn't read again"), first);

        write(root, "notes.md", "two!");
        let third = scan_workspace_cached(root, &SkipList::default(), &mut cache).expect("scan");
        let notes_hash = third
            .entries
            .iter()
            .find(|entry| entry.path == "notes.md")
            .map(|entry| entry.hash.clone());
        assert_eq!(notes_hash, Some(file_hash(b"two!")));
    }

    #[test]
    fn an_edit_made_while_a_round_runs_is_kept() {
        let sharer = repo();
        let teammate = tempfile::tempdir().expect("tempdir");
        let copy = clone_of(sharer.path(), teammate.path());
        write(sharer.path(), "changed.txt", "two");
        let mut state = copy_state(&head_of(sharer.path()), &[]);
        let manifest = scan_workspace(sharer.path(), &SkipList::default()).expect("scan");
        // The teammate saves the file while the sharer's version downloads.
        let fetch = |path: &str| {
            write(&copy, path, "teammate");
            std::fs::read(sharer.path().join(path)).map_err(|error| error.to_string())
        };

        let plan =
            follow_manifest(&copy, &mut state, &manifest, &fetch, &|_, _| {}).expect("follow");

        assert_eq!(plan.both_changed, vec!["changed.txt".to_string()]);
        assert_eq!(read(&copy, "changed.txt").as_deref(), Some("teammate"));
        assert!(!state.synced.contains_key("changed.txt"));
    }
}
