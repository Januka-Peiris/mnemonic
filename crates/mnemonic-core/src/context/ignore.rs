use std::path::Path;

pub struct IgnoreSet {
    hard_prefixes: Vec<String>,
    hard_extensions: Vec<String>,
    hard_exact: Vec<String>,
    conditional_prefixes: Vec<String>,
    conditional_patterns: Vec<String>,
}

impl IgnoreSet {
    pub fn load(_repo_root: &Path) -> Self {
        // In a future iteration, parse .forgeignore and .gitignore here.
        // For now, use built-in defaults only (covers the vast majority of cases).
        Self::with_defaults()
    }

    fn with_defaults() -> Self {
        Self {
            hard_prefixes: vec![
                ".git/".into(),
                ".forge/context/".into(),
                ".cursor/".into(),
                ".claude/".into(),
                ".github/".into(),
                "node_modules/".into(),
                ".venv/".into(),
                "venv/".into(),
                "__pycache__/".into(),
                ".mypy_cache/".into(),
                ".pytest_cache/".into(),
                ".ruff_cache/".into(),
                ".tox/".into(),
                "coverage/".into(),
                "dist/".into(),
                "build/".into(),
                "target/".into(),
                ".next/".into(),
                ".nuxt/".into(),
                ".turbo/".into(),
                ".cache/".into(),
                "tmp/".into(),
                "temp/".into(),
                "out/".into(),
            ],
            hard_extensions: vec![
                "png".into(),
                "jpg".into(),
                "jpeg".into(),
                "gif".into(),
                "webp".into(),
                "svg".into(),
                "pdf".into(),
                "zip".into(),
                "gz".into(),
                "tar".into(),
                "woff".into(),
                "woff2".into(),
                "ttf".into(),
                "ico".into(),
                "lock".into(),
                "icns".into(),
                "mp4".into(),
                "mov".into(),
                "avi".into(),
                "pem".into(),
                "key".into(),
                "crt".into(),
                "p12".into(),
                "der".into(),
            ],
            hard_exact: vec![".env".into()],
            conditional_prefixes: vec![
                "tests/".into(),
                "test/".into(),
                "migrations/".into(),
                "alembic/".into(),
                "docs/".into(),
                "assets/".into(),
                "media/".into(),
                "public/".into(),
                "static/".into(),
            ],
            conditional_patterns: vec![
                "test_".into(),  // file starts with test_
                ".test.".into(), // contains .test.
                ".spec.".into(), // contains .spec.
            ],
        }
    }

    pub fn should_exclude(&self, path: &str) -> bool {
        let norm = path.replace('\\', "/");
        // Check hard exact matches
        let filename = norm.split('/').next_back().unwrap_or("");
        if self
            .hard_exact
            .iter()
            .any(|e| filename == e.as_str() || norm == e.as_str())
        {
            return true;
        }
        // Check .env.* pattern
        if filename.starts_with(".env.") {
            return true;
        }
        // Check hard prefixes (any path component)
        for prefix in &self.hard_prefixes {
            if norm.starts_with(prefix.as_str()) || norm.contains(&format!("/{}", prefix)) {
                return true;
            }
        }
        // Check hard extensions
        if let Some(ext) = Path::new(&norm).extension().and_then(|e| e.to_str()) {
            if self.hard_extensions.iter().any(|x| x == ext) {
                return true;
            }
        }
        false
    }

    pub fn is_conditional_exclude(&self, path: &str) -> bool {
        let norm = path.replace('\\', "/");
        let filename = norm.split('/').next_back().unwrap_or("");
        // Directory prefix
        for prefix in &self.conditional_prefixes {
            if norm.starts_with(prefix.as_str()) || norm.contains(&format!("/{}", prefix)) {
                return true;
            }
        }
        // Filename patterns
        for pat in &self.conditional_patterns {
            if filename.contains(pat.as_str()) {
                return true;
            }
        }
        // Markdown files
        if path.ends_with(".md") || path.ends_with(".rst") || path.ends_with(".txt") {
            return true;
        }
        false
    }

    pub fn detect_flags(&self, path: &str) -> crate::context::schema::RepoFlags {
        let norm = path.replace('\\', "/");
        let filename = norm.split('/').next_back().unwrap_or("").to_lowercase();
        let is_test = self.is_conditional_exclude(path)
            && (filename.starts_with("test")
                || filename.contains(".test.")
                || filename.contains(".spec.")
                || norm.contains("/tests/")
                || norm.contains("/test/"));
        let is_config = matches!(
            filename.as_str(),
            "package.json"
                | "cargo.toml"
                | "pyproject.toml"
                | "setup.py"
                | "setup.cfg"
                | "tsconfig.json"
                | "vite.config.ts"
                | "vite.config.js"
                | "tailwind.config.js"
                | "tailwind.config.ts"
                | "eslint.config.js"
                | ".eslintrc"
                | ".prettierrc"
                | "jest.config.js"
                | "jest.config.ts"
                | "webpack.config.js"
                | "rollup.config.js"
                | "babel.config.js"
                | ".babelrc"
                | "dockerfile"
                | "docker-compose.yml"
                | "docker-compose.yaml"
                | "makefile"
                | "justfile"
        ) || filename.ends_with(".toml")
            || filename.ends_with(".yaml")
            || filename.ends_with(".yml")
            || filename.ends_with(".json")
            || filename.ends_with(".ini")
            || filename.ends_with(".cfg");
        let is_generated = filename.contains(".generated.")
            || filename.contains(".gen.")
            || norm.contains("/generated/")
            || norm.contains("/gen/")
            || norm.contains("/__generated__/");
        let is_binary = matches!(
            Path::new(&norm)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
            "png" | "jpg" | "gif" | "ico" | "woff" | "woff2" | "ttf" | "pdf" | "zip" | "gz" | "tar"
        );
        crate::context::schema::RepoFlags {
            is_test,
            is_config,
            is_generated,
            is_binary,
        }
    }

    pub fn detect_language(path: &str) -> String {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        match ext {
            "rs" => "rust",
            "ts" | "tsx" => "typescript",
            "js" | "jsx" | "mjs" | "cjs" => "javascript",
            "py" => "python",
            "go" => "go",
            "java" => "java",
            "kt" => "kotlin",
            "swift" => "swift",
            "c" | "h" => "c",
            "cpp" | "cc" | "cxx" | "hpp" => "cpp",
            "cs" => "csharp",
            "rb" => "ruby",
            "php" => "php",
            "sh" | "bash" | "zsh" => "shell",
            "sql" => "sql",
            "html" | "htm" => "html",
            "css" | "scss" | "sass" | "less" => "css",
            "json" => "json",
            "toml" => "toml",
            "yaml" | "yml" => "yaml",
            "md" | "mdx" => "markdown",
            _ => "unknown",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_node_modules() {
        let ig = IgnoreSet::with_defaults();
        assert!(ig.should_exclude("node_modules/react/index.js"));
        assert!(ig.should_exclude("src/node_modules/foo/bar.js"));
    }

    #[test]
    fn excludes_target_dir() {
        let ig = IgnoreSet::with_defaults();
        assert!(ig.should_exclude("target/debug/forge"));
    }

    #[test]
    fn excludes_images() {
        let ig = IgnoreSet::with_defaults();
        assert!(ig.should_exclude("assets/logo.png"));
        assert!(ig.should_exclude("src/icons/icon.ico"));
    }

    #[test]
    fn does_not_exclude_source() {
        let ig = IgnoreSet::with_defaults();
        assert!(!ig.should_exclude("src/App.tsx"));
        assert!(!ig.should_exclude("src-tauri/src/lib.rs"));
    }

    #[test]
    fn conditional_excludes_tests() {
        let ig = IgnoreSet::with_defaults();
        assert!(ig.is_conditional_exclude("tests/test_auth.py"));
        assert!(ig.is_conditional_exclude("src/auth.test.ts"));
    }
}
