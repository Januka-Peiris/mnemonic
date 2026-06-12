use std::path::Path;

use crate::context::discovery;
use crate::context::graph::FileGraph;
use crate::context::overlay;
use crate::context::schema::{ContextPreview, ContextSegment, SelectConfig, WorkspaceOverlay};
use crate::context::select;
use crate::context::token_fit;
use crate::repositories::workspace_repository;
use crate::state::AppState;

pub fn build_context_preview(
    state: &AppState,
    workspace_id: &str,
    prompt_hint: Option<&str>,
    cfg: &SelectConfig,
) -> Result<ContextPreview, String> {
    // Get workspace root
    let workspace = workspace_repository::get_detail(&state.db, workspace_id)?
        .ok_or_else(|| format!("Workspace {workspace_id} not found"))?;
    let primary_path = workspace
        .summary
        .workspace_root_path
        .clone()
        .unwrap_or_else(|| workspace.worktree_path.clone());
    let root = Path::new(&primary_path);

    // Load or build the repo map
    let map_result = discovery::build_repo_map(root, false, &state.db);

    let (map, meta, stale_map) = match map_result {
        Ok((m, meta)) => {
            // Check staleness against current HEAD
            let stale = discovery::is_stale(root, &state.db);
            (m, Some(meta), stale)
        }
        Err(err) => {
            // No map available — return overlay-only preview
            let overlay = overlay::build_workspace_overlay(state, workspace_id);
            let segments = overlay_only_segments(&overlay);
            let total_tokens: u32 = segments.iter().map(|s| s.estimated_tokens).sum();
            return Ok(ContextPreview {
                included: segments,
                excluded: vec![],
                estimated_tokens_context: total_tokens,
                estimated_tokens_total: total_tokens,
                stale_map: true,
                low_signal: true,
                signal_score: 0.0,
                warning: Some(format!(
                    "Repo map unavailable: {err}. Using changed-file diffs only."
                )),
            });
        }
    };

    let signal_score = meta.as_ref().map(|m| m.quality.signal_score).unwrap_or(0.0);
    let low_signal = signal_score < cfg.signal_score_threshold;

    // If low signal, return overlay-only
    if low_signal {
        let overlay = overlay::build_workspace_overlay(state, workspace_id);
        let segments = overlay_only_segments(&overlay);
        let total_tokens: u32 = segments.iter().map(|s| s.estimated_tokens).sum();
        return Ok(ContextPreview {
            included: segments,
            excluded: vec![],
            estimated_tokens_context: total_tokens,
            estimated_tokens_total: total_tokens,
            stale_map,
            low_signal: true,
            signal_score,
            warning: Some(format!(
                "Repo map signal score {signal_score:.2} is below threshold {:.2}. Using changed-file diffs only.",
                cfg.signal_score_threshold
            )),
        });
    }

    // Full pipeline
    let overlay_data = overlay::build_workspace_overlay(state, workspace_id);
    let graph = FileGraph::build(&map.entries);
    let prompt = prompt_hint.unwrap_or("");
    let candidates = select::build_candidate_pool(prompt, &overlay_data, &map, &graph, cfg);
    let (included, excluded) = token_fit::fit_to_budget(candidates, &map, &overlay_data, cfg);

    let estimated_tokens_context: u32 = included.iter().map(|s| s.estimated_tokens).sum();

    let mut warning = None;
    if stale_map {
        warning = Some(
            "Repo map is stale (default branch has new commits). Consider refreshing.".to_string(),
        );
    }

    Ok(ContextPreview {
        included,
        excluded,
        estimated_tokens_context,
        estimated_tokens_total: estimated_tokens_context, // prompt total computed at injection time
        stale_map,
        low_signal: false,
        signal_score,
        warning,
    })
}

fn overlay_only_segments(overlay: &WorkspaceOverlay) -> Vec<ContextSegment> {
    use crate::context::schema::estimate_tokens;
    let mut segs = Vec::new();
    for f in &overlay.changed {
        let content = format!(
            "### {} (changed: +{} -{} lines)\n```diff\n{}\n```",
            f.path, f.additions, f.deletions, f.diff
        );
        segs.push(ContextSegment {
            path: f.path.clone(),
            tier: "mandatory".to_string(),
            render_mode: "diff_hunks".to_string(),
            estimated_tokens: estimate_tokens(&content),
            content,
        });
    }
    for f in &overlay.new_files {
        let content = format!("### {} (new file)\n{}", f.path, f.diff);
        segs.push(ContextSegment {
            path: f.path.clone(),
            tier: "mandatory".to_string(),
            render_mode: "full".to_string(),
            estimated_tokens: estimate_tokens(&content),
            content,
        });
    }
    segs
}
