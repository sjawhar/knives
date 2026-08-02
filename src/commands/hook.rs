//! `knives hook`: harness adapters that never interrupt the calling session.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::cli::{Exit, HookHarness};
use crate::config::{GuidanceRoot, GuidanceRootKind, default_config_path, load};
use crate::hook::claude_code::{Event, EventKind, response};
use crate::hook::guidance::{claim_lines, format_guidance, format_notice, guidance_for};
use crate::hook::opencode::{self, Event as OpenCodeEvent, EventKind as OpenCodeEventKind};
use crate::hook::resolve::{argument_paths, managed_repo_for};
use crate::hook::state::SessionState;
use crate::store::{Store, default_state_path};

const CLAUDE_CODE: &str = "claude-code";
const OPENCODE: &str = "opencode";
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
const OPENCODE_RELEVANT_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "edit",
    "write",
    "apply_patch",
    "bash",
];

pub fn run(harness: HookHarness) -> Exit {
    let result = match harness {
        HookHarness::ClaudeCode => run_claude_code(),
        HookHarness::Opencode => run_opencode(),
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

fn run_opencode() -> anyhow::Result<Option<String>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let event = match OpenCodeEvent::parse(&input) {
        Ok(event) => event,
        Err(error) => {
            eprintln!("knives hook: {error:#}");
            return opencode::empty_response().map(Some).map_err(Into::into);
        }
    };
    let home = config_home();
    let response = match event.kind() {
        OpenCodeEventKind::ToolExecuteAfter => opencode_tool_after(&event, &home),
        OpenCodeEventKind::ChatSystem => opencode_chat_system(&event),
        OpenCodeEventKind::ShellEnv => opencode_shell_env(&event),
        OpenCodeEventKind::Compacting => opencode_compacting(&event, &home),
        OpenCodeEventKind::Other => opencode::empty_response().map_err(Into::into),
    }?;
    Ok(Some(response))
}

fn opencode_tool_after(event: &OpenCodeEvent, home: &Path) -> anyhow::Result<String> {
    let Some(session_id) = event.session_id() else {
        return opencode::tool_response("").map_err(Into::into);
    };
    let Some(tool) = event.tool() else {
        return opencode::tool_response("").map_err(Into::into);
    };
    if !OPENCODE_RELEVANT_TOOLS.contains(&tool) {
        return opencode::tool_response("").map_err(Into::into);
    }
    let Some(args) = event.args() else {
        return opencode::tool_response("").map_err(Into::into);
    };
    let paths = argument_paths(tool, args);
    if paths.is_empty() {
        return opencode::tool_response("").map_err(Into::into);
    }

    let registry = load(&default_config_path())?;
    let Some(matched) = managed_repo_for(&paths, &registry.guidance_roots()) else {
        return opencode::tool_response("").map_err(Into::into);
    };
    let flags = SessionState::load(home, OPENCODE, session_id).repo(&matched.repo.root);
    let requested = event.parts();
    let include_notice =
        requested.notice && matched.repo.kind == GuidanceRootKind::Managed && !flags.noticed;
    let guidance = (requested.guidance && !flags.guided)
        .then(|| guidance_for(&matched.repo, &matched.candidate))
        .flatten();

    let mut additions = Vec::new();
    if include_notice {
        additions.push(notice_for(&matched.repo)?);
    }
    if let Some(guidance) = guidance {
        additions.push(format_guidance(&matched.repo.name, &guidance));
    }
    let addition = additions.join("\n");
    if !addition.is_empty() {
        let _ = SessionState::update(home, OPENCODE, session_id, |state| {
            state.mark(&matched.repo.root, true, true);
        })?;
    }
    opencode::tool_response(&addition).map_err(Into::into)
}

fn opencode_chat_system(event: &OpenCodeEvent) -> anyhow::Result<String> {
    let Some(directory) = event.directory() else {
        return opencode::system_response("", &[]).map_err(Into::into);
    };
    let registry = load(&default_config_path())?;
    let Some(matched) = managed_repo_for(&[PathBuf::from(directory)], &registry.guidance_roots())
    else {
        return opencode::system_response("", &[]).map_err(Into::into);
    };
    let Some(guidance) = guidance_for(&matched.repo, &matched.candidate) else {
        return opencode::system_response("", &[]).map_err(Into::into);
    };
    let bodies = guidance
        .bodies
        .iter()
        .map(|instruction| instruction.body.clone())
        .collect::<Vec<_>>();
    let system = format_guidance(&matched.repo.name, &guidance);
    opencode::system_response(&system, &bodies).map_err(Into::into)
}

fn opencode_shell_env(event: &OpenCodeEvent) -> anyhow::Result<String> {
    let owner = event.cwd().map(owner_for).transpose()?.flatten();
    opencode::environment_response(owner.as_deref()).map_err(Into::into)
}

fn owner_for(cwd: &str) -> anyhow::Result<Option<String>> {
    let registry = load(&default_config_path())?;
    let Some(matched) = managed_repo_for(&[PathBuf::from(cwd)], &registry.guidance_roots()) else {
        return Ok(None);
    };
    if matched.repo.kind != GuidanceRootKind::Managed {
        return Ok(None);
    }
    if let Some(owner) = std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|owner| !owner.trim().is_empty())
    {
        return Ok(Some(owner));
    }
    if let Some(owner) = current_agent()? {
        return Ok(Some(owner));
    }
    let store = Store::open(default_state_path())?;
    let owners = store
        .claims(None)
        .into_iter()
        .filter(|claim| claim.repo == matched.repo.name)
        .map(|claim| claim.owner.clone())
        .collect::<BTreeSet<_>>();
    Ok((owners.len() == 1)
        .then(|| owners.into_iter().next())
        .flatten())
}

fn current_agent() -> anyhow::Result<Option<String>> {
    let path = default_state_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state: serde_json::Value = serde_json::from_str(&text)?;
    Ok(["currentAgent", "current_agent"]
        .into_iter()
        .find_map(|key| state.get(key).and_then(serde_json::Value::as_str))
        .filter(|owner| !owner.trim().is_empty())
        .map(str::to_owned))
}

fn opencode_compacting(event: &OpenCodeEvent, home: &Path) -> anyhow::Result<String> {
    if let Some(session_id) = event.session_id() {
        SessionState::delete(home, OPENCODE, session_id);
    }
    opencode::empty_response().map_err(Into::into)
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
    response(EventKind::SessionStart.wire_name(), &notice)
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
    response(EventKind::PostToolUse.wire_name(), &parts.join("\n"))
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
