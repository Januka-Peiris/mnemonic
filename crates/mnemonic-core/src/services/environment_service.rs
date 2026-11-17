use std::env;
use std::path::PathBuf;
use std::process::Command;

use crate::models::EnvironmentCheckItem;

struct Dependency {
    name: &'static str,
    binary: &'static str,
    fix: &'static str,
    optional: bool,
}

const DEPENDENCIES: &[Dependency] = &[
    Dependency {
        name: "git",
        binary: "git",
        fix: "brew install git",
        optional: false,
    },
    Dependency {
        name: "claude CLI",
        binary: "claude",
        fix: "curl -fsSL https://claude.ai/install.sh | bash (or: npm install -g @anthropic-ai/claude-code)",
        optional: false,
    },
    Dependency {
        name: "codex CLI",
        binary: "codex",
        fix: "npm install -g @openai/codex (or: brew install codex)",
        optional: true,
    },
    Dependency {
        name: "GitHub CLI",
        binary: "gh",
        fix: "brew install gh",
        optional: true,
    },
    Dependency {
        name: "Ollama",
        binary: "ollama",
        fix: "Install Ollama from https://ollama.com or use a custom local profile command",
        optional: true,
    },
];

pub fn check_environment() -> Vec<EnvironmentCheckItem> {
    DEPENDENCIES.iter().map(check_dependency).collect()
}

fn check_dependency(dependency: &Dependency) -> EnvironmentCheckItem {
    let (status, path) = match find_binary(dependency.binary) {
        Ok(Some(path)) => ("ok".to_string(), Some(path.display().to_string())),
        Ok(None) => ("missing".to_string(), None),
        Err(err) => {
            log::warn!(target: "mnemonic_lib", "environment check failed for {}: {err}", dependency.binary);
            ("unknown".to_string(), None)
        }
    };

    EnvironmentCheckItem {
        name: dependency.name.to_string(),
        binary: dependency.binary.to_string(),
        status,
        fix: dependency.fix.to_string(),
        optional: dependency.optional,
        path,
    }
}

pub fn find_binary(binary: &str) -> Result<Option<PathBuf>, String> {
    if let Some(path) = find_with_system_lookup(binary)? {
        return Ok(Some(path));
    }

    if let Some(path) = find_in_common_paths(binary) {
        return Ok(Some(path));
    }

    if let Some(path) = find_with_login_shell(binary) {
        return Ok(Some(path));
    }

    Ok(None)
}

fn find_with_system_lookup(binary: &str) -> Result<Option<PathBuf>, String> {
    let checker = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(checker)
        .arg(binary)
        .output()
        .map_err(|err| format!("failed to run {checker}: {err}"))?;

    if output.status.success() {
        Ok(first_existing_path(&String::from_utf8_lossy(
            &output.stdout,
        )))
    } else {
        Ok(None)
    }
}

fn find_in_common_paths(binary: &str) -> Option<PathBuf> {
    common_binary_dirs()
        .into_iter()
        .map(|dir| dir.join(binary))
        .find(|path| path.is_file())
}

fn common_binary_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/opt/homebrew/sbin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/local/sbin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ];

    if let Some(home) = home_dir() {
        dirs.extend([
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".npm-global/bin"),
            home.join(".bun/bin"),
            home.join("Library/pnpm"),
            home.join(".local/share/pnpm"),
        ]);
    }

    dirs
}

fn find_with_login_shell(binary: &str) -> Option<PathBuf> {
    let escaped_binary = shell_single_quote(binary);
    let script = format!("command -v {escaped_binary} 2>/dev/null");
    let mut shells = Vec::new();

    if let Ok(shell) = env::var("SHELL") {
        if !shell.trim().is_empty() {
            shells.push(shell);
        }
    }
    shells.extend([
        "/bin/zsh".to_string(),
        "/bin/bash".to_string(),
        "/bin/sh".to_string(),
    ]);
    shells.sort();
    shells.dedup();

    for shell in shells {
        let output = Command::new(&shell).args(["-lc", &script]).output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        if let Some(path) = first_existing_path(&String::from_utf8_lossy(&output.stdout)) {
            return Some(path);
        }
    }

    None
}

fn first_existing_path(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Returns the `-c credential.helper=...` args that route git authentication
/// through `gh auth git-credential`. Resolves the full path to `gh` so the
/// helper works even when the Tauri app inherits a minimal PATH (e.g. when
/// launched from Finder).
pub fn git_credential_args() -> Vec<String> {
    let gh_path = find_gh_path().unwrap_or_else(|| "gh".to_string());
    vec![
        "-c".to_string(),
        "credential.helper=".to_string(),
        "-c".to_string(),
        format!("credential.helper=!{gh_path} auth git-credential"),
    ]
}

pub fn find_gh_path() -> Option<String> {
    if let Ok(Some(path)) = find_binary("gh") {
        return Some(path.to_string_lossy().to_string());
    }
    for candidate in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"] {
        if std::path::Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_check_shape_is_stable() {
        let items = check_environment();
        assert_eq!(items.len(), 5);
        assert!(items.iter().any(|item| item.binary == "git"));
        assert!(items
            .iter()
            .any(|item| item.binary == "gh" && item.optional));
        assert!(items
            .iter()
            .any(|item| item.binary == "ollama" && item.optional));
    }

    #[test]
    fn first_existing_path_ignores_empty_lines() {
        let git_path = first_existing_path("\n/usr/bin/git\n");
        assert_eq!(git_path, Some(PathBuf::from("/usr/bin/git")));
    }

    #[test]
    fn shell_single_quote_escapes_quotes() {
        assert_eq!(shell_single_quote("abc'def"), "'abc'\\''def'");
    }
}
