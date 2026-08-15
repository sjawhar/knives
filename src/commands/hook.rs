//! `knives hook`: harness adapters that never interrupt the calling session.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::cli::{Exit, HookHarness};
use crate::config::{GuidanceRoot, GuidanceRootKind, Registry, default_config_path, load};
use crate::hook::claude_code::{
    Event, EventKind, POST_TOOL_USE_WIRE_NAME, SESSION_START_WIRE_NAME, response,
};
use crate::hook::guidance::{claim_lines, format_guidance, format_notice, guidance_for};
use crate::hook::opencode::{self, Event as OpenCodeEvent, EventKind as OpenCodeEventKind};
use crate::hook::resolve::{Match, argument_paths, managed_repo_for, trust_rule_match, url_owner};
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
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("knives hook: {error:#}");
        return opencode::empty_response().map(Some).map_err(Into::into);
    }
    let event = match OpenCodeEvent::parse(&input) {
        Ok(event) => event,
        Err(error) => {
            eprintln!("knives hook: {error:#}");
            return opencode::empty_response().map(Some).map_err(Into::into);
        }
    };
    let kind = event.kind();
    let home = config_home();
    let response = match kind {
        OpenCodeEventKind::ToolExecuteAfter => opencode_tool_after(&event, &home),
        OpenCodeEventKind::ChatSystem => opencode_chat_system(&event, &home),
        OpenCodeEventKind::ShellEnv => opencode_shell_env(&event),
        OpenCodeEventKind::Compacting => opencode_compacting(&event, &home),
        OpenCodeEventKind::Other => opencode::empty_response().map_err(Into::into),
    };
    match response {
        Ok(response) => Ok(Some(response)),
        Err(error) => {
            eprintln!("knives hook: {error:#}");
            empty_opencode_response(kind).map(Some)
        }
    }
}

fn empty_opencode_response(kind: OpenCodeEventKind) -> anyhow::Result<String> {
    match kind {
        OpenCodeEventKind::ToolExecuteAfter => opencode::tool_response(""),
        OpenCodeEventKind::ChatSystem => opencode::system_response("", &[]),
        OpenCodeEventKind::ShellEnv => opencode::environment_response(None),
        OpenCodeEventKind::Compacting | OpenCodeEventKind::Other => opencode::empty_response(),
    }
    .map_err(Into::into)
}

fn opencode_tool_after(event: &OpenCodeEvent, home: &Path) -> anyhow::Result<String> {
    let Some(session_id) = event.session_id() else {
        return opencode::tool_response("").map_err(Into::into);
    };
    let Some(matched) = relevant_tool_match(
        event.tool(),
        event.args(),
        OPENCODE_RELEVANT_TOOLS,
        Some((home, OPENCODE, session_id)),
    )?
    else {
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

fn opencode_chat_system(event: &OpenCodeEvent, home: &Path) -> anyhow::Result<String> {
    let Some(directory) = event.directory() else {
        return opencode::system_response("", &[]).map_err(Into::into);
    };
    let registry = load(&default_config_path())?;
    let cache = event
        .session_id()
        .map(|session_id| (home, OPENCODE, session_id));
    let Some(matched) = match_with_trust(&[PathBuf::from(directory)], &registry, cache)? else {
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
    let owner = event
        .cwd()
        .map(Path::new)
        .map(owner_for)
        .transpose()?
        .flatten();
    opencode::environment_response(owner.as_deref()).map_err(Into::into)
}

pub(crate) fn owner_for(cwd: &Path) -> anyhow::Result<Option<String>> {
    if let Some(owner) = std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|owner| !owner.trim().is_empty())
    {
        return Ok(Some(owner));
    }
    let registry = load(&default_config_path())?;
    let Some(matched) = managed_repo_for(&[cwd.to_path_buf()], &registry.guidance_roots()) else {
        return Ok(None);
    };
    if matched.repo.kind != GuidanceRootKind::Managed {
        return Ok(None);
    }
    let store = Store::open(default_state_path())?;
    if let Some(owner) = store.current_agent() {
        return Ok(Some(owner.to_owned()));
    }
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

fn opencode_compacting(event: &OpenCodeEvent, home: &Path) -> anyhow::Result<String> {
    if let Some(session_id) = event.session_id() {
        let _ = SessionState::update(home, OPENCODE, session_id, SessionState::clear)?;
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
    let Some(matched) = match_with_trust(
        &[PathBuf::from(cwd)],
        &registry,
        Some((home, CLAUDE_CODE, session_id)),
    )?
    else {
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
    response(SESSION_START_WIRE_NAME, &notice)
        .map(Some)
        .map_err(Into::into)
}

fn post_tool_use(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    let Some(session_id) = event.session_id() else {
        return Ok(None);
    };
    let Some(matched) = relevant_tool_match(
        event.tool_name(),
        event.tool_input(),
        RELEVANT_TOOLS,
        Some((home, CLAUDE_CODE, session_id)),
    )?
    else {
        return Ok(None);
    };
    let state = SessionState::load(home, CLAUDE_CODE, session_id);
    let flags = state.repo(&matched.repo.root);
    let include_notice = matched.repo.kind == GuidanceRootKind::Managed && !flags.noticed;
    let include_guidance = !flags.guided
        && event
            .cwd()
            .is_some_and(|cwd| !contains_cwd(&matched.repo, cwd));
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
    response(POST_TOOL_USE_WIRE_NAME, &parts.join("\n"))
        .map(Some)
        .map_err(Into::into)
}

fn relevant_tool_match(
    tool: Option<&str>,
    args: Option<&serde_json::Value>,
    relevant_tools: &[&str],
    cache: Option<(&Path, &str, &str)>,
) -> anyhow::Result<Option<Match>> {
    let Some(tool) = tool else {
        return Ok(None);
    };
    if !relevant_tools.contains(&tool) {
        return Ok(None);
    }
    let Some(args) = args else {
        return Ok(None);
    };
    let paths = argument_paths(tool, args);
    if paths.is_empty() {
        return Ok(None);
    }
    let registry = load(&default_config_path())?;
    match_with_trust(&paths, &registry, cache)
}

fn match_with_trust(
    paths: &[PathBuf],
    registry: &Registry,
    cache: Option<(&Path, &str, &str)>,
) -> anyhow::Result<Option<Match>> {
    if let Some(matched) = managed_repo_for(paths, &registry.guidance_roots()) {
        return Ok(Some(matched));
    }

    let mut cache_error = None;
    let mut probe = |root: &Path| {
        let cached_owners = cache.and_then(|(home, harness, session_id)| {
            SessionState::load(home, harness, session_id)
                .owner_remotes(root)
                .map(<[String]>::to_owned)
        });
        let cache_miss = cached_owners.is_none();
        let owners = cached_owners.map_or_else(
            || match crate::jj::git_toplevel(root) {
                Ok(toplevel) if toplevel.canonicalize().ok().as_deref() == Some(root) => crate::jj::git_remotes(root)
                    .map_or_else(
                        |_| {
                            if root.join(".jj").exists() {
                                eprintln!("knives hook: owner-rule matching requires a colocated .git checkout");
                            }
                            None
                        },
                        |remotes| {
                            Some(
                                remotes
                                    .values()
                                    .filter_map(|url| url_owner(url).map(str::to_owned))
                                    .collect(),
                            )
                        },
                    ),
                Ok(_) => None,
                Err(_) => {
                    if root.join(".jj").exists() {
                        eprintln!("knives hook: owner-rule matching requires a colocated .git checkout");
                    }
                    None
                }
            },
            Some,
        );

        let Some(owners) = owners else {
            return Some(false);
        };

        if cache_miss
            && let Some((home, harness, session_id)) = cache
            && let Err(error) = SessionState::update(home, harness, session_id, |state| {
                state.record_owner_remotes(root, owners.clone());
            })
        {
            cache_error = Some(error);
            return None;
        }
        Some(owners.iter().any(|owner| {
            registry
                .trust
                .owners
                .iter()
                .any(|trusted| trusted.eq_ignore_ascii_case(owner))
        }))
    };
    let matched = trust_rule_match(paths, &registry.trust, &mut probe);
    if let Some(error) = cache_error {
        return Err(error);
    }
    Ok(matched)
}

fn pre_compact(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    if let Some(session_id) = event.session_id() {
        let _ = SessionState::update(home, CLAUDE_CODE, session_id, SessionState::clear)?;
    }
    Ok(None)
}

fn contains_cwd(repo: &GuidanceRoot, cwd: &str) -> bool {
    managed_repo_for(&[PathBuf::from(cwd)], std::slice::from_ref(repo)).is_some()
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

#[cfg(test)]
#[path = "hook_regression_tests.rs"]
mod regression_tests;
