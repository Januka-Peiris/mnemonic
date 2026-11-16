use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

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

impl SkipList {
    pub fn skips(&self, path: &str) -> bool {
        let components: Vec<&str> = path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect();
        let Some(final_component) = components.last() else {
            return false;
        };
        let normalized = components.join("/");
        let patterns = self
            .patterns
            .iter()
            .map(String::as_str)
            .chain(SECRET_FLOOR.iter().copied());
        patterns.into_iter().any(|pattern| {
            if pattern.contains('/') {
                pattern == normalized
            } else if pattern.contains('*') {
                glob_matches(pattern, final_component)
            } else {
                components.iter().any(|component| {
                    *component == pattern
                        || (pattern == ".git" && component.eq_ignore_ascii_case(".git"))
                })
            }
        })
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

pub fn needs_approval(path: &str) -> bool {
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

pub fn scan_workspace(root: &Path, skip: &SkipList) -> Result<TeamFileManifest, String> {
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

    let mut entries: BTreeMap<String, TeamFileEntry> = BTreeMap::new();
    for path in files {
        if let Some(entry) = build_entry(root, &canonical_root, &path)? {
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
    let (size, hash) =
        hash_file(&full_path).map_err(|error| format!("could not read {path}: {error}"))?;
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
            args[0],
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
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!("mnemonic-share-{}.bundle", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
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
        if !matches!(entry.kind, TeamFileKind::Deleted) {
            hashes.insert(entry.path.clone(), entry.hash);
        }
        dirty_vs_head.insert(entry.path);
    }
    for path in paths {
        if dirty_vs_head.contains(path) || skip.skips(path) {
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
    let local = read_local_copy(root, skip, state.synced.keys().map(String::as_str))?;
    let local_hash = |path: &str| local.hashes.get(path).map(String::as_str).unwrap_or("");
    let mut edits: Vec<String> = state
        .synced
        .iter()
        .filter(|(path, base)| !skip.skips(path) && local_hash(path) != base.as_str())
        .map(|(path, _)| path.clone())
        .chain(
            local
                .dirty_vs_head
                .iter()
                .filter(|path| !state.synced.contains_key(*path))
                .cloned(),
        )
        .collect();
    edits.sort_unstable();
    edits.dedup();
    Ok(edits)
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

/// Writes one of the sharer's entries into the copy, fetching its bytes
/// only when the copy doesn't already hold them. Returns the hash now in
/// the copy, or None when the entry may not be written here.
pub fn write_copy_entry(
    root: &Path,
    entry: &TeamFileEntry,
    fetch: &dyn Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<Option<String>, String> {
    if SkipList::default().skips(&entry.path) {
        return Ok(None);
    }
    let full_path = safe_relative_path(root, &entry.path)?;
    match &entry.kind {
        TeamFileKind::Deleted => {
            remove_if_present(&full_path)?;
            Ok(Some(String::new()))
        }
        TeamFileKind::File { executable } => {
            let is_file = std::fs::symlink_metadata(&full_path)
                .is_ok_and(|metadata| metadata.file_type().is_file());
            if is_file && current_hash(&full_path)?.as_deref() == Some(entry.hash.as_str()) {
                set_executable(&full_path, *executable)?;
                return Ok(Some(entry.hash.clone()));
            }
            let bytes = fetch(&entry.path)?;
            replace_atomically(&full_path, |temp_path| {
                std::fs::write(temp_path, &bytes)
                    .map_err(|error| format!("could not write {}: {error}", entry.path))?;
                set_executable(temp_path, *executable)
            })?;
            Ok(Some(file_hash(&bytes)))
        }
        TeamFileKind::Symlink { target } => {
            let inside = in_tree_link_target(&entry.path, target)
                .is_some_and(|resolved| safe_relative_path(root, &resolved).is_ok());
            if !inside {
                log::warn!(
                    "Not recreating {}: it points outside the workspace",
                    entry.path
                );
                return Ok(None);
            }
            replace_atomically(&full_path, |temp_path| make_symlink(target, temp_path))?;
            Ok(Some(file_hash(target.as_bytes())))
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
    let skip = SkipList::default();
    let incoming = TeamFileManifest {
        head_commit: incoming.head_commit.clone(),
        entries: incoming
            .entries
            .iter()
            .filter(|entry| !skip.skips(&entry.path))
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
                .filter(|path| !skip.skips(path)),
        );
    }
    let head_moves = incoming.head_commit != state.head_commit;
    let has_local_commits =
        head_moves && has_local_commits(root, &state.head_commit, &incoming.head_commit)?;
    let plan = plan_follow(
        state,
        &local.hashes,
        &local.dirty_vs_head,
        &incoming,
        has_local_commits,
    );

    if plan.move_head {
        move_head(root, state, &incoming.head_commit)?;
    }
    if !plan.restore.is_empty() {
        let tracked = tracked_paths(root, "HEAD")?;
        let (in_head, not_in_head): (Vec<&str>, Vec<&str>) = plan
            .restore
            .iter()
            .map(String::as_str)
            .partition(|path| tracked.contains(*path));
        run_git_on_paths(root, &["checkout", "HEAD"], &in_head)?;
        for path in not_in_head {
            if let Ok(full_path) = safe_relative_path(root, path) {
                remove_if_present(&full_path)?;
            }
        }
        for path in &plan.restore {
            state.synced.remove(path);
        }
    }

    let entries: HashMap<&str, &TeamFileEntry> = incoming
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut failures = Vec::new();
    let total = plan.write.len() + plan.delete.len();
    for (done, path) in plan.write.iter().chain(&plan.delete).enumerate() {
        on_progress(done, total);
        let Some(entry) = entries.get(path.as_str()) else {
            continue;
        };
        match write_copy_entry(root, entry, fetch) {
            Ok(Some(hash)) => {
                state.synced.insert(path.clone(), hash);
            }
            Ok(None) => {}
            Err(error) => failures.push(error),
        }
    }
    on_progress(total, total);
    match failures.first() {
        None => Ok(plan),
        Some(first) if failures.len() == 1 => Err(first.clone()),
        Some(first) => Err(format!("{first} (and {} more)", failures.len() - 1)),
    }
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

fn record_current_hashes(
    root: &Path,
    state: &mut CopyState,
    paths: &[String],
) -> Result<(), String> {
    for path in paths {
        let hash = match safe_relative_path(root, path) {
            Ok(full_path) => current_hash(&full_path)?.unwrap_or_default(),
            Err(_) => String::new(),
        };
        state.synced.insert(path.clone(), hash);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let bundle = bundle_for(sharer.path(), &[head.clone()]).expect("bundle");
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
                write_copy_entry(root, &link(path, target), &fetch),
                Ok(None)
            );
            assert!(
                std::fs::symlink_metadata(root.join(path)).is_err(),
                "{path}"
            );
        }
        write_copy_entry(root, &link("CLAUDE.md", ".rules"), &fetch).expect("in-tree link");
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
}
