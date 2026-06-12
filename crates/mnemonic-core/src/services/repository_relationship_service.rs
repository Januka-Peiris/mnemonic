use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{
    CreateRepositoryRelationshipInput, DiscoveredRepository, RelevantRepositoriesSuggestionResult,
    RepositoryRelationship, RepositoryRelationshipsResult, RepositoryScopeSuggestion,
    SuggestRelevantRepositoriesInput, UpdateRepositoryRelationshipInput,
};
use crate::repositories::{
    repository_relationship_repository,
    repository_relationship_repository::AppRepositoryRelationshipRow, repository_repository,
};
use crate::services::workspace_script_service;
use crate::state::AppState;

#[derive(Debug, Clone)]
struct RelationshipDraft {
    app_relationship_id: Option<String>,
    from_repo_id: String,
    from_repo_name: String,
    to_repo_id: String,
    to_repo_name: String,
    kind: String,
    label: Option<String>,
    notes: Option<String>,
    sources: BTreeSet<String>,
    config_paths: BTreeSet<String>,
}

pub fn list_repository_relationships(
    state: &AppState,
) -> Result<RepositoryRelationshipsResult, String> {
    let repositories = repository_repository::list(&state.db)?;
    let repo_by_id = repositories
        .iter()
        .map(|repo| (repo.id.clone(), repo.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut drafts: BTreeMap<String, RelationshipDraft> = BTreeMap::new();
    let mut warnings = Vec::new();

    for app_row in repository_relationship_repository::list(&state.db)? {
        match app_row_to_draft(&app_row, &repo_by_id) {
            Ok(draft) => merge_draft(&mut drafts, draft),
            Err(warning) => warnings.push(warning),
        }
    }

    for from_repo in &repositories {
        let config = workspace_script_service::load_config_from_root(Path::new(&from_repo.path));
        for warning in config.repository_relationship_warnings {
            warnings.push(format!("{}: {}", from_repo.name, warning));
        }
        let config_path = config.path.clone().unwrap_or_else(|| {
            Path::new(&from_repo.path)
                .join(".forge/config.json")
                .display()
                .to_string()
        });
        for relationship in config.repository_relationships {
            let Some(to_repo) =
                resolve_repository_target(&relationship.to, from_repo, &repositories)
            else {
                warnings.push(format!(
                    "{}: repository relationship target `{}` could not be resolved.",
                    from_repo.name, relationship.to
                ));
                continue;
            };
            if to_repo.id == from_repo.id {
                warnings.push(format!(
                    "{}: repository relationship target `{}` resolves to the same repository.",
                    from_repo.name, relationship.to
                ));
                continue;
            }

            merge_draft(
                &mut drafts,
                RelationshipDraft {
                    app_relationship_id: None,
                    from_repo_id: from_repo.id.clone(),
                    from_repo_name: from_repo.name.clone(),
                    to_repo_id: to_repo.id.clone(),
                    to_repo_name: to_repo.name.clone(),
                    kind: relationship.kind,
                    label: relationship.label,
                    notes: relationship.notes,
                    sources: BTreeSet::from(["config".to_string()]),
                    config_paths: BTreeSet::from([config_path.clone()]),
                },
            );
        }
    }

    let mut relationships = drafts
        .into_values()
        .map(draft_to_relationship)
        .collect::<Vec<_>>();
    relationships.sort_by(|a, b| {
        a.from_repo_name
            .cmp(&b.from_repo_name)
            .then(a.to_repo_name.cmp(&b.to_repo_name))
            .then(a.kind.cmp(&b.kind))
    });

    Ok(RepositoryRelationshipsResult {
        relationships,
        warnings,
    })
}

pub fn create_app_repository_relationship(
    state: &AppState,
    input: CreateRepositoryRelationshipInput,
) -> Result<RepositoryRelationshipsResult, String> {
    let input = sanitize_create_input(input)?;
    validate_relationship_refs(state, &input.from_repo_id, &input.to_repo_id, &input.kind)?;
    let id = format!(
        "repo-rel-{}-{:016x}",
        timestamp_millis(),
        stable_hash(&format!(
            "{}:{}:{}:{}:{}",
            input.from_repo_id,
            input.to_repo_id,
            input.kind,
            input.label.as_deref().unwrap_or(""),
            input.notes.as_deref().unwrap_or("")
        ))
    );
    repository_relationship_repository::insert(&state.db, &id, &input)?;
    list_repository_relationships(state)
}

pub fn update_app_repository_relationship(
    state: &AppState,
    input: UpdateRepositoryRelationshipInput,
) -> Result<RepositoryRelationshipsResult, String> {
    let input = sanitize_update_input(input)?;
    validate_relationship_refs(state, &input.from_repo_id, &input.to_repo_id, &input.kind)?;
    repository_relationship_repository::update(&state.db, &input)?;
    list_repository_relationships(state)
}

pub fn delete_app_repository_relationship(
    state: &AppState,
    relationship_id: &str,
) -> Result<RepositoryRelationshipsResult, String> {
    repository_relationship_repository::delete(&state.db, relationship_id)?;
    list_repository_relationships(state)
}

pub fn suggest_relevant_repositories_for_task(
    state: &AppState,
    input: SuggestRelevantRepositoriesInput,
) -> Result<RelevantRepositoriesSuggestionResult, String> {
    let source_repo_id = required(input.source_repo_id, "Source repository")?;
    let task_prompt = input.task_prompt.trim().to_string();
    let repositories = repository_repository::list(&state.db)?;
    if !repositories.iter().any(|repo| repo.id == source_repo_id) {
        return Err("Source repository was not found.".to_string());
    }
    let relationships = list_repository_relationships(state)?;
    Ok(suggest_from_data(
        &source_repo_id,
        &task_prompt,
        &repositories,
        &relationships.relationships,
        relationships.warnings,
    ))
}

fn merge_draft(drafts: &mut BTreeMap<String, RelationshipDraft>, incoming: RelationshipDraft) {
    let key = relationship_key(&incoming.from_repo_id, &incoming.to_repo_id, &incoming.kind);
    if let Some(existing) = drafts.get_mut(&key) {
        if existing.app_relationship_id.is_none() {
            existing.app_relationship_id = incoming.app_relationship_id;
        }
        if existing.label.is_none()
            || (incoming.sources.contains("app") && incoming.label.is_some())
        {
            existing.label = incoming.label;
        }
        if existing.notes.is_none()
            || (incoming.sources.contains("app") && incoming.notes.is_some())
        {
            existing.notes = incoming.notes;
        }
        existing.sources.extend(incoming.sources);
        existing.config_paths.extend(incoming.config_paths);
    } else {
        drafts.insert(key, incoming);
    }
}

#[derive(Debug, Clone)]
struct ScopeDraft {
    repo: DiscoveredRepository,
    score: f32,
    reasons: BTreeSet<String>,
    relationship_kinds: BTreeSet<String>,
    sources: BTreeSet<String>,
}

fn suggest_from_data(
    source_repo_id: &str,
    task_prompt: &str,
    repositories: &[DiscoveredRepository],
    relationships: &[RepositoryRelationship],
    warnings: Vec<String>,
) -> RelevantRepositoriesSuggestionResult {
    let repo_by_id = repositories
        .iter()
        .map(|repo| (repo.id.clone(), repo.clone()))
        .collect::<BTreeMap<_, _>>();
    let task_lower = task_prompt.to_ascii_lowercase();
    let task_tokens = task_tokens(&task_lower);
    let mut drafts: BTreeMap<String, ScopeDraft> = BTreeMap::new();

    if let Some(source_repo) = repo_by_id.get(source_repo_id) {
        add_scope_score(
            &mut drafts,
            source_repo,
            100.0,
            "Current task starts in this repository.".to_string(),
            None,
            None,
        );
    }

    for repo in repositories {
        if repo.id == source_repo_id {
            continue;
        }
        if mentioned_repo(&task_lower, repo) {
            add_scope_score(
                &mut drafts,
                repo,
                42.0,
                format!("Task text explicitly mentions `{}`.", repo.name),
                None,
                None,
            );
        }
    }

    for relationship in relationships {
        let (related_repo_id, direction, base_score) =
            if relationship.from_repo_id == source_repo_id {
                (&relationship.to_repo_id, "outbound", 58.0)
            } else if relationship.to_repo_id == source_repo_id {
                (&relationship.from_repo_id, "inbound", 46.0)
            } else {
                continue;
            };
        let Some(repo) = repo_by_id.get(related_repo_id) else {
            continue;
        };

        let kind_label = relationship.kind.replace('_', " ");
        add_scope_score(
            &mut drafts,
            repo,
            base_score,
            format!(
                "Explicit {direction} `{}` relationship connects {} → {}.",
                kind_label, relationship.from_repo_name, relationship.to_repo_name
            ),
            Some(relationship.kind.clone()),
            None,
        );
        for source in &relationship.sources {
            add_scope_score(
                &mut drafts,
                repo,
                0.0,
                String::new(),
                Some(relationship.kind.clone()),
                Some(source.clone()),
            );
        }

        if relationship_kind_matches_task(&relationship.kind, &task_lower) {
            add_scope_score(
                &mut drafts,
                repo,
                18.0,
                format!(
                    "Task keywords match `{}` relationship concerns.",
                    relationship.kind.replace('_', " ")
                ),
                Some(relationship.kind.clone()),
                None,
            );
        }

        let relationship_text = format!(
            "{} {}",
            relationship.label.as_deref().unwrap_or(""),
            relationship.notes.as_deref().unwrap_or("")
        )
        .to_ascii_lowercase();
        if !relationship_text.is_empty()
            && task_tokens
                .iter()
                .any(|token| token.len() >= 4 && relationship_text.contains(token))
        {
            add_scope_score(
                &mut drafts,
                repo,
                12.0,
                "Task overlaps relationship label/notes.".to_string(),
                Some(relationship.kind.clone()),
                None,
            );
        }
    }

    let mut suggestions = drafts
        .into_values()
        .map(|draft| RepositoryScopeSuggestion {
            repo_id: draft.repo.id,
            repo_name: draft.repo.name,
            repo_path: draft.repo.path,
            score: draft.score.min(100.0),
            selected_by_default: draft.score >= 40.0,
            reasons: draft
                .reasons
                .into_iter()
                .filter(|reason| !reason.is_empty())
                .collect(),
            relationship_kinds: draft.relationship_kinds.into_iter().collect(),
            sources: source_order(draft.sources),
        })
        .collect::<Vec<_>>();
    suggestions.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.repo_name.cmp(&b.repo_name))
    });

    RelevantRepositoriesSuggestionResult {
        source_repo_id: source_repo_id.to_string(),
        suggestions,
        warnings,
    }
}

fn add_scope_score(
    drafts: &mut BTreeMap<String, ScopeDraft>,
    repo: &DiscoveredRepository,
    score: f32,
    reason: String,
    relationship_kind: Option<String>,
    source: Option<String>,
) {
    let draft = drafts.entry(repo.id.clone()).or_insert_with(|| ScopeDraft {
        repo: repo.clone(),
        score: 0.0,
        reasons: BTreeSet::new(),
        relationship_kinds: BTreeSet::new(),
        sources: BTreeSet::new(),
    });
    draft.score += score;
    if !reason.is_empty() {
        draft.reasons.insert(reason);
    }
    if let Some(kind) = relationship_kind {
        draft.relationship_kinds.insert(kind);
    }
    if let Some(source) = source {
        draft.sources.insert(source);
    }
}

fn task_tokens(task_lower: &str) -> Vec<String> {
    task_lower
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(str::to_string)
        .collect()
}

fn mentioned_repo(task_lower: &str, repo: &DiscoveredRepository) -> bool {
    let name = repo.name.to_ascii_lowercase();
    task_lower.contains(&name)
        || repo
            .path
            .rsplit('/')
            .next()
            .map(|tail| task_lower.contains(&tail.to_ascii_lowercase()))
            .unwrap_or(false)
}

fn relationship_kind_matches_task(kind: &str, task_lower: &str) -> bool {
    let keywords: &[&str] = match kind {
        "frontend_backend" => &[
            "api", "endpoint", "route", "auth", "login", "frontend", "backend", "server", "client",
        ],
        "sdk_api" => &["sdk", "api", "client", "contract", "version", "endpoint"],
        "shared_schema" => &[
            "schema", "type", "types", "model", "protobuf", "openapi", "graphql", "contract",
        ],
        "deployment_dependency" => &[
            "deploy",
            "release",
            "infra",
            "environment",
            "env",
            "config",
            "rollback",
        ],
        "event_flow" => &["event", "queue", "webhook", "pubsub", "message", "stream"],
        "depends_on" => &["dependency", "depends", "import", "package", "library"],
        "related" => &["integration", "cross-repo", "coordination", "related"],
        _ => &[],
    };
    keywords.iter().any(|keyword| task_lower.contains(keyword))
}

fn app_row_to_draft(
    row: &AppRepositoryRelationshipRow,
    repo_by_id: &BTreeMap<String, DiscoveredRepository>,
) -> Result<RelationshipDraft, String> {
    let from_repo = repo_by_id.get(&row.from_repo_id).ok_or_else(|| {
        format!(
            "App-managed repository relationship `{}` references missing source repository `{}`.",
            row.id, row.from_repo_id
        )
    })?;
    let to_repo = repo_by_id.get(&row.to_repo_id).ok_or_else(|| {
        format!(
            "App-managed repository relationship `{}` references missing target repository `{}`.",
            row.id, row.to_repo_id
        )
    })?;

    Ok(RelationshipDraft {
        app_relationship_id: Some(row.id.clone()),
        from_repo_id: from_repo.id.clone(),
        from_repo_name: from_repo.name.clone(),
        to_repo_id: to_repo.id.clone(),
        to_repo_name: to_repo.name.clone(),
        kind: row.kind.clone(),
        label: row.label.clone(),
        notes: row.notes.clone(),
        sources: BTreeSet::from(["app".to_string()]),
        config_paths: BTreeSet::new(),
    })
}

fn draft_to_relationship(draft: RelationshipDraft) -> RepositoryRelationship {
    let sources = source_order(draft.sources);
    let config_paths = draft.config_paths.into_iter().collect::<Vec<_>>();
    let app_relationship_id = draft.app_relationship_id;
    RepositoryRelationship {
        id: relationship_key(&draft.from_repo_id, &draft.to_repo_id, &draft.kind),
        read_only: app_relationship_id.is_none(),
        app_relationship_id,
        from_repo_id: draft.from_repo_id,
        from_repo_name: draft.from_repo_name,
        to_repo_id: draft.to_repo_id,
        to_repo_name: draft.to_repo_name,
        kind: draft.kind,
        label: draft.label,
        notes: draft.notes,
        sources,
        config_paths,
    }
}

fn source_order(sources: BTreeSet<String>) -> Vec<String> {
    let mut ordered = Vec::new();
    if sources.contains("app") {
        ordered.push("app".to_string());
    }
    if sources.contains("config") {
        ordered.push("config".to_string());
    }
    ordered.extend(
        sources
            .into_iter()
            .filter(|source| source != "app" && source != "config"),
    );
    ordered
}

fn relationship_key(from_repo_id: &str, to_repo_id: &str, kind: &str) -> String {
    format!("{from_repo_id}:{to_repo_id}:{kind}")
}

fn resolve_repository_target<'a>(
    target: &str,
    from_repo: &DiscoveredRepository,
    repositories: &'a [DiscoveredRepository],
) -> Option<&'a DiscoveredRepository> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }

    if let Some(repo) = repositories.iter().find(|repo| repo.id == target) {
        return Some(repo);
    }
    if let Some(repo) = repositories.iter().find(|repo| repo.name == target) {
        return Some(repo);
    }
    if let Some(repo) = repositories.iter().find(|repo| repo.path == target) {
        return Some(repo);
    }

    let target_path = PathBuf::from(target);
    let candidate = if target_path.is_absolute() {
        target_path
    } else {
        Path::new(&from_repo.path).join(target)
    };
    let canonical_candidate = canonicalize(&candidate);
    repositories
        .iter()
        .find(|repo| canonicalize(Path::new(&repo.path)) == canonical_candidate)
}

fn canonicalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn sanitize_create_input(
    input: CreateRepositoryRelationshipInput,
) -> Result<CreateRepositoryRelationshipInput, String> {
    Ok(CreateRepositoryRelationshipInput {
        from_repo_id: required(input.from_repo_id, "Source repository")?,
        to_repo_id: required(input.to_repo_id, "Target repository")?,
        kind: required(input.kind, "Relationship kind")?,
        label: clean_optional(input.label),
        notes: clean_optional(input.notes),
    })
}

fn sanitize_update_input(
    input: UpdateRepositoryRelationshipInput,
) -> Result<UpdateRepositoryRelationshipInput, String> {
    Ok(UpdateRepositoryRelationshipInput {
        id: required(input.id, "Relationship id")?,
        from_repo_id: required(input.from_repo_id, "Source repository")?,
        to_repo_id: required(input.to_repo_id, "Target repository")?,
        kind: required(input.kind, "Relationship kind")?,
        label: clean_optional(input.label),
        notes: clean_optional(input.notes),
    })
}

fn required(value: String, label: &str) -> Result<String, String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        Err(format!("{label} is required."))
    } else {
        Ok(trimmed)
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_relationship_refs(
    state: &AppState,
    from_repo_id: &str,
    to_repo_id: &str,
    kind: &str,
) -> Result<(), String> {
    if from_repo_id == to_repo_id {
        return Err("Source and target repositories must be different.".to_string());
    }
    if !crate::models::repository_relationship::is_valid_relationship_kind(kind) {
        return Err(format!(
            "Unsupported repository relationship kind `{kind}`."
        ));
    }
    if repository_repository::get(&state.db, from_repo_id)?.is_none() {
        return Err("Source repository was not found.".to_string());
    }
    if repository_repository::get(&state.db, to_repo_id)?.is_none() {
        return Err("Target repository was not found.".to_string());
    }
    Ok(())
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn stable_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::models::{DiscoveredRepository, DiscoveredWorktree};

    fn repo(id: &str, name: &str, path: &str) -> DiscoveredRepository {
        DiscoveredRepository {
            id: id.to_string(),
            name: name.to_string(),
            path: path.to_string(),
            current_branch: Some("main".to_string()),
            head: Some("abc".to_string()),
            is_dirty: false,
            worktrees: Vec::<DiscoveredWorktree>::new(),
            last_scanned_at: "1".to_string(),
        }
    }

    #[test]
    fn app_relationship_crud_round_trips() {
        let db = Database::in_memory().expect("db");
        let frontend = repo("repo-front", "frontend", "/tmp/front");
        let backend = repo("repo-back", "backend", "/tmp/back");
        repository_repository::upsert(&db, &frontend).expect("front");
        repository_repository::upsert(&db, &backend).expect("back");

        let created = repository_relationship_repository::insert(
            &db,
            "rel-1",
            &CreateRepositoryRelationshipInput {
                from_repo_id: frontend.id.clone(),
                to_repo_id: backend.id.clone(),
                kind: "frontend_backend".to_string(),
                label: Some("Frontend calls API".to_string()),
                notes: None,
            },
        )
        .expect("insert");
        assert_eq!(created.id, "rel-1");

        let updated = repository_relationship_repository::update(
            &db,
            &UpdateRepositoryRelationshipInput {
                id: "rel-1".to_string(),
                from_repo_id: frontend.id,
                to_repo_id: backend.id,
                kind: "depends_on".to_string(),
                label: None,
                notes: Some("Runtime dependency".to_string()),
            },
        )
        .expect("update");
        assert_eq!(updated.kind, "depends_on");
        assert_eq!(updated.notes.as_deref(), Some("Runtime dependency"));

        repository_relationship_repository::delete(&db, "rel-1").expect("delete");
        assert!(repository_relationship_repository::list(&db)
            .expect("list")
            .is_empty());
    }

    #[test]
    fn merge_deduplicates_app_and_config_sources() {
        let mut drafts = BTreeMap::new();
        merge_draft(
            &mut drafts,
            RelationshipDraft {
                app_relationship_id: Some("rel-1".to_string()),
                from_repo_id: "a".to_string(),
                from_repo_name: "a".to_string(),
                to_repo_id: "b".to_string(),
                to_repo_name: "b".to_string(),
                kind: "depends_on".to_string(),
                label: Some("App label".to_string()),
                notes: None,
                sources: BTreeSet::from(["app".to_string()]),
                config_paths: BTreeSet::new(),
            },
        );
        merge_draft(
            &mut drafts,
            RelationshipDraft {
                app_relationship_id: None,
                from_repo_id: "a".to_string(),
                from_repo_name: "a".to_string(),
                to_repo_id: "b".to_string(),
                to_repo_name: "b".to_string(),
                kind: "depends_on".to_string(),
                label: Some("Config label".to_string()),
                notes: Some("Config notes".to_string()),
                sources: BTreeSet::from(["config".to_string()]),
                config_paths: BTreeSet::from(["/tmp/a/.forge/config.json".to_string()]),
            },
        );

        let relationship = draft_to_relationship(drafts.into_values().next().expect("draft"));
        assert_eq!(relationship.sources, vec!["app", "config"]);
        assert_eq!(relationship.app_relationship_id.as_deref(), Some("rel-1"));
        assert_eq!(relationship.label.as_deref(), Some("App label"));
        assert_eq!(relationship.notes.as_deref(), Some("Config notes"));
    }

    #[test]
    fn scope_suggestion_includes_source_and_related_repo() {
        let frontend = repo("repo-front", "frontend", "/tmp/front");
        let backend = repo("repo-back", "backend", "/tmp/back");
        let result = suggest_from_data(
            &frontend.id,
            "Update the login API contract",
            &[frontend.clone(), backend.clone()],
            &[RepositoryRelationship {
                id: "rel".to_string(),
                app_relationship_id: Some("rel".to_string()),
                from_repo_id: frontend.id.clone(),
                from_repo_name: frontend.name.clone(),
                to_repo_id: backend.id.clone(),
                to_repo_name: backend.name.clone(),
                kind: "frontend_backend".to_string(),
                label: Some("Frontend calls backend API".to_string()),
                notes: None,
                sources: vec!["app".to_string()],
                config_paths: vec![],
                read_only: false,
            }],
            vec![],
        );

        assert_eq!(result.suggestions.len(), 2);
        assert_eq!(result.suggestions[0].repo_id, frontend.id);
        let backend_suggestion = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.repo_id == backend.id)
            .expect("backend suggestion");
        assert!(backend_suggestion.selected_by_default);
        assert!(backend_suggestion
            .relationship_kinds
            .contains(&"frontend_backend".to_string()));
        assert!(backend_suggestion
            .reasons
            .iter()
            .any(|reason| reason.contains("Task keywords match")));
    }

    #[test]
    fn scope_suggestion_includes_explicitly_mentioned_repo_without_relationship() {
        let frontend = repo("repo-front", "frontend", "/tmp/front");
        let docs = repo("repo-docs", "docs-site", "/tmp/docs-site");
        let result = suggest_from_data(
            &frontend.id,
            "Update docs-site examples for the new copy",
            &[frontend.clone(), docs.clone()],
            &[],
            vec![],
        );

        let docs_suggestion = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.repo_id == docs.id)
            .expect("docs suggestion");
        assert!(docs_suggestion.selected_by_default);
        assert!(docs_suggestion
            .reasons
            .iter()
            .any(|reason| reason.contains("explicitly mentions")));
    }
}
