use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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
}
