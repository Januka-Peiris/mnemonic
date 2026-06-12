use serde::{Deserialize, Serialize};

/// Level of risk associated with a command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SafetyLevel {
    Safe,          // Read-only or common low-risk operations
    Informational, // Operations that might take time or have side effects but are generally intended
    Risky,         // Destructive operations or significant changes
    Blocked,       // Strictly forbidden operations (if any)
}

/// A detailed report on a command's safety.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSafetyResult {
    pub command: String,
    pub safety_level: SafetyLevel,
    pub category: String,
    pub explanation: String,
    pub risks: Vec<String>,
}

/// Patterns for categorization and risk detection.
const DESTRUCTIVE_PATTERNS: &[(&str, &str)] = &[
    ("rm -rf", "Recursive deletion of files and directories"),
    ("rm -fr", "Recursive deletion of files and directories"),
    ("rm -r ", "Recursive deletion of files"),
    ("sudo ", "Execution with elevated privileges"),
    ("dd if=", "Low-level data copying (can overwrite disks)"),
    ("mkfs", "Filesystem creation (erases disk data)"),
    (": >", "File truncation"),
    ("DROP TABLE", "SQL table deletion"),
    ("DROP DATABASE", "SQL database deletion"),
];

const GIT_RISKY_PATTERNS: &[(&str, &str)] = &[
    ("push --force", "Force-pushing can overwrite remote history"),
    ("push -f", "Force-pushing can overwrite remote history"),
    (
        "reset --hard",
        "Hard reset discards all uncommitted changes",
    ),
    ("clean -f", "Force-cleaning removes untracked files"),
    (
        "clean -fd",
        "Force-cleaning removes untracked files and directories",
    ),
];

const SAFE_PATTERNS: &[&str] = &[
    "ls",
    "git status",
    "git log",
    "git diff",
    "cat",
    "grep",
    "pwd",
    "whoami",
    "npm list",
    "cargo list",
    "echo",
];

pub fn check_command_safety(command: &str) -> CommandSafetyResult {
    let lower = command.to_lowercase();
    let mut risks = Vec::new();
    let mut category = "general".to_string();
    let mut explanation = "This command appears to be a standard operation.".to_string();
    let mut safety_level = SafetyLevel::Safe;

    // 1. Check for blocked/extremely risky patterns
    for (pattern, desc) in DESTRUCTIVE_PATTERNS {
        if lower.contains(pattern) {
            safety_level = SafetyLevel::Risky;
            category = "destructive".to_string();
            explanation = format!("This command involves potentially destructive actions: {desc}.");
            risks.push(desc.to_string());
        }
    }

    // 2. Check for risky Git operations
    for (pattern, desc) in GIT_RISKY_PATTERNS {
        if lower.contains(pattern) {
            safety_level = SafetyLevel::Risky;
            category = "git_destructive".to_string();
            explanation = format!("This Git command is considered risky: {desc}.");
            risks.push(desc.to_string());
        }
    }

    // 3. Detect category if not set
    if category == "general" {
        if lower.starts_with("git ") {
            category = "git".to_string();
        } else if lower.starts_with("npm ")
            || lower.starts_with("pnpm ")
            || lower.starts_with("yarn ")
        {
            category = "node".to_string();
            safety_level = SafetyLevel::Informational;
            explanation =
                "This command involves Node.js package management or scripts.".to_string();
        } else if lower.starts_with("cargo ") {
            category = "rust".to_string();
            safety_level = SafetyLevel::Informational;
            explanation = "This command involves Rust/Cargo operations.".to_string();
        }
    }

    // 4. Override to Safe if it matches known safe patterns (and no risks found)
    if risks.is_empty() {
        let cmd_start = lower.split_whitespace().next().unwrap_or("");
        if SAFE_PATTERNS.contains(&cmd_start)
            || (lower.starts_with("git ")
                && SAFE_PATTERNS.contains(&lower.split_whitespace().nth(1).unwrap_or("")))
        {
            safety_level = SafetyLevel::Safe;
            explanation =
                "This is a recognized safe command for inspection or reporting.".to_string();
        }
    }

    CommandSafetyResult {
        command: command.to_string(),
        safety_level,
        category,
        explanation,
        risks,
    }
}

pub fn is_risky_command(command: &str) -> bool {
    let result = check_command_safety(command);
    result.safety_level == SafetyLevel::Risky || result.safety_level == SafetyLevel::Blocked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_risky_shell_commands() {
        assert!(is_risky_command("rm -rf /tmp/example"));
        assert!(is_risky_command("git reset --hard HEAD"));
        assert!(is_risky_command("git push --force origin branch"));
    }

    #[test]
    fn detail_report_is_accurate() {
        let result = check_command_safety("rm -rf /");
        assert_eq!(result.safety_level, SafetyLevel::Risky);
        assert!(result.explanation.contains("destructive"));

        let safe = check_command_safety("git status");
        assert_eq!(safe.safety_level, SafetyLevel::Safe);
    }

    #[test]
    fn allows_normal_dev_commands() {
        let result = check_command_safety("npm run test");
        assert_eq!(result.safety_level, SafetyLevel::Informational);
    }
}
