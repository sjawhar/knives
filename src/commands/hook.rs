//! `knives hook`: harness adapters that never interrupt the calling session.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::cli::{Exit, HookHarness};
use crate::config::{GuidanceRoot, GuidanceRootKind, default_config_path, load};
use crate::hook::claude_code::{Event, EventKind, response};
use crate::hook::guidance::{claim_lines, format_guidance, format_notice, guidance_for};
use crate::hook::resolve::{argument_paths, managed_repo_for};
use crate::hook::state::SessionState;
use crate::store::{Store, default_state_path};

const CLAUDE_CODE: &str = "claude-code";
const RELEVANT_TOOLS: &[&str] = &[
    "Read",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Grep",
    "Glob",
    "Bash",
];

pub fn run(harness: HookHarness) -> Exit {
    let result = match harness {
        HookHarness::ClaudeCode => run_claude_code(),
        HookHarness::Opencode => Err(anyhow::anyhow!(
            "the opencode hook adapter is not implemented"
        )),
    };
    match result {
        Ok(Some(output)) => {
            if let Err(error) = write_output(&output) {
                eprintln!("knives hook: {error:#}");
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!("knives hook: {error:#}"),
    }
    Exit::Ok
}

fn run_claude_code() -> anyhow::Result<Option<String>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let event = Event::parse(&input)?;
    let home = config_home();
    match event.kind() {
        EventKind::SessionStart => session_start(&event, &home),
        EventKind::PostToolUse => post_tool_use(&event, &home),
        EventKind::PreCompact => pre_compact(&event, &home),
        EventKind::SessionEnd => {
            if let Some(session_id) = event.session_id() {
                SessionState::delete(&home, CLAUDE_CODE, session_id);
            }
            Ok(None)
        }
        EventKind::Other => Ok(None),
    }
}

fn session_start(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    let Some(session_id) = event.session_id() else {
        return Ok(None);
    };
    if event.source() == Some("compact") {
        let _ = SessionState::update(home, CLAUDE_CODE, session_id, SessionState::clear)?;
        return Ok(None);
    }
    let Some(cwd) = event.cwd() else {
        return Ok(None);
    };
    let registry = load(&default_config_path())?;
    let Some(matched) = managed_repo_for(&[PathBuf::from(cwd)], &registry.guidance_roots()) else {
        return Ok(None);
    };
    if matched.repo.kind != GuidanceRootKind::Managed {
        return Ok(None);
    }
    let state = SessionState::load(home, CLAUDE_CODE, session_id);
    if state.repo(&matched.repo.root).noticed {
        return Ok(None);
    }
    let notice = notice_for(&matched.repo)?;
    let _ = SessionState::update(home, CLAUDE_CODE, session_id, |state| {
        state.mark(&matched.repo.root, true, false);
    })?;
    response(
        event
            .hook_event_name()
            .ok_or_else(|| anyhow::anyhow!("session-start event has no name"))?,
        &notice,
    )
    .map(Some)
    .map_err(Into::into)
}

fn post_tool_use(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    let Some(session_id) = event.session_id() else {
        return Ok(None);
    };
    let Some(tool_name) = event.tool_name() else {
        return Ok(None);
    };
    if !RELEVANT_TOOLS.contains(&tool_name) {
        return Ok(None);
    }
    let Some(tool_input) = event.tool_input() else {
        return Ok(None);
    };
    let paths = argument_paths(tool_name, tool_input);
    if paths.is_empty() {
        return Ok(None);
    }

    let registry = load(&default_config_path())?;
    let Some(matched) = managed_repo_for(&paths, &registry.guidance_roots()) else {
        return Ok(None);
    };
    let state = SessionState::load(home, CLAUDE_CODE, session_id);
    let flags = state.repo(&matched.repo.root);
    let include_notice = matched.repo.kind == GuidanceRootKind::Managed && !flags.noticed;
    let include_guidance = !flags.guided && !contains_cwd(&matched.repo, event.cwd());
    if !include_notice && !include_guidance {
        return Ok(None);
    }

    let mut parts = Vec::new();
    if include_notice {
        parts.push(notice_for(&matched.repo)?);
    }
    let guidance = include_guidance
        .then(|| guidance_for(&matched.repo, &matched.candidate))
        .flatten();
    if let Some(guidance) = &guidance {
        parts.push(format_guidance(&matched.repo.name, guidance));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let _ = SessionState::update(home, CLAUDE_CODE, session_id, |state| {
        state.mark(&matched.repo.root, include_notice, guidance.is_some());
    })?;
    response(
        event
            .hook_event_name()
            .ok_or_else(|| anyhow::anyhow!("post-tool-use event has no name"))?,
        &parts.join("\n"),
    )
    .map(Some)
    .map_err(Into::into)
}

fn pre_compact(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    if let Some(session_id) = event.session_id() {
        let _ = SessionState::update(home, CLAUDE_CODE, session_id, SessionState::clear)?;
    }
    Ok(None)
}

fn contains_cwd(repo: &GuidanceRoot, cwd: Option<&str>) -> bool {
    cwd.map(PathBuf::from)
        .and_then(|cwd| managed_repo_for(&[cwd], std::slice::from_ref(repo)))
        .is_some()
}

fn config_home() -> PathBuf {
    default_config_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn notice_for(repo: &GuidanceRoot) -> anyhow::Result<String> {
    let store = Store::open(default_state_path())?;
    let visible_claims = claim_lines(&all_claims(&store), &repo.name);
    Ok(format_notice(&repo.name, &repo.root, &visible_claims))
}

fn all_claims(store: &Store) -> Vec<crate::store::Claim> {
    store.claims(None).into_iter().cloned().collect()
}

fn write_output(output: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    Ok(())
}
