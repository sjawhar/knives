//! `knives hook`: harness adapters that never interrupt the calling session.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::bind;
use crate::cli::{Exit, HookHarness};
use crate::commands::claim::Identity;
use crate::config::{GuidanceRoot, Registry, default_config_path, load};
use crate::hook::claude_code::{
    Event, EventKind, POST_TOOL_USE_WIRE_NAME, SESSION_START_WIRE_NAME, response,
};
use crate::hook::guidance::{
    claim_lines, format_guidance, format_notice, guidance_for, notice_digest,
};
use crate::hook::opencode::{self, Event as OpenCodeEvent, EventKind as OpenCodeEventKind};
use crate::hook::resolve::{Match, argument_paths, match_checkout};
use crate::hook::state::SessionState;
use crate::ids::RepoName;
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

/// How long a hook invocation may live before the watchdog ends it.
///
/// A response is advisory and worthless once the harness's own handler timeout
/// (30s in OMP) has passed, so nothing legitimate is lost at this deadline.
const WATCHDOG_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// The longest deadline an override may set: above this the watchdog stops
/// being a guard, so larger values fall back to the default instead.
const WATCHDOG_DEADLINE_CEILING_MS: u64 = 600_000;

/// End this process at a wall-clock deadline, whatever it is blocked on.
///
/// Harnesses spawn `knives hook` with a piped stdin and can abandon the handler
/// that would write it, leaving the process parked in its stdin read forever.
/// On 2026-08-25 a loaded devbox leaked one such immortal process per agent
/// tool call until ~13k concurrent `knives` processes took the machine down.
/// Dying loudly bounds every invocation's lifetime no matter which harness
/// spawned it or how it misbehaves. `KNIVES_HOOK_DEADLINE_MS` overrides the
/// deadline (tests use it; operators can too); zero and values above the
/// ceiling would disarm the guard, so they fall back to the default.
///
/// Exits with `Exit::Incomplete`, never `Exit::Usage`: a clap usage error (2)
/// is how an old binary without the `hook` subcommand fails, and the Claude
/// Code wrapper and the TypeScript shim both key on that distinction.
fn arm_watchdog() {
    let deadline = std::env::var("KNIVES_HOOK_DEADLINE_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|milliseconds| (1..=WATCHDOG_DEADLINE_CEILING_MS).contains(milliseconds))
        .map_or(WATCHDOG_DEADLINE, std::time::Duration::from_millis);
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        // Best-effort diagnostics: `eprintln!` panics when stderr is gone, and a
        // harness that abandoned this process may well have closed its pipes —
        // the exit must happen regardless.
        let _ = writeln!(
            std::io::stderr(),
            "knives hook: gave up after {}ms; exiting so abandoned invocations cannot accumulate",
            deadline.as_millis()
        );
        std::process::exit(i32::from(Exit::Incomplete.code()));
    });
}

pub fn run(harness: HookHarness) -> Exit {
    arm_watchdog();
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
    let cache = Some((home, OPENCODE, session_id));
    let Some((registry, matched)) = relevant_tool_match(
        &ToolCall {
            tool: event.tool(),
            args: event.args(),
            relevant: OPENCODE_RELEVANT_TOOLS,
        },
        cache,
    )?
    else {
        return opencode::tool_response("").map_err(Into::into);
    };
    if matched.is_managed()
        && let Some(cwd) = event.cwd()
    {
        crate::seen::record_observation(
            standing_in(cwd, &registry, cache)?.as_ref(),
            Path::new(cwd),
            &Identity {
                owner: session_id.to_owned(),
                kind: crate::store::OwnerKind::HarnessSession,
            },
        );
    }
    let repo = guidance_root(&matched);
    let state = SessionState::load(home, OPENCODE, session_id);
    let flags = state.repo(&repo.root);
    let requested = event.parts();
    let notice = notice_if_requested(&repo, &state, requested.notice && matched.is_managed())?;
    let guidance = (requested.guidance && matched.trusted && !flags.guided)
        .then(|| guidance_for(&repo, &matched.candidate))
        .flatten();

    let (notice_text, notice_update) = notice.map_or((None, None), |notice| {
        let (text, update) = notice.into_parts();
        (Some(text), Some(update))
    });
    let mut additions = Vec::new();
    if let Some(text) = notice_text {
        additions.push(text);
    }
    let guidance_rendered = guidance.is_some();
    if let Some(guidance) = guidance {
        additions.push(format_guidance(&repo.name, &guidance));
    }
    let addition = additions.join("\n");
    if !addition.is_empty() {
        let _ = SessionState::update(home, OPENCODE, session_id, move |state| {
            if let Some(update) = notice_update {
                update.apply(state, &repo.root);
            }
            if guidance_rendered {
                state.mark_guided(&repo.root);
            }
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
    if !matched.trusted {
        return opencode::system_response("", &[]).map_err(Into::into);
    }
    let repo = guidance_root(&matched);
    let Some(guidance) = guidance_for(&repo, &matched.candidate) else {
        return opencode::system_response("", &[]).map_err(Into::into);
    };
    let bodies = guidance
        .bodies
        .iter()
        .map(|instruction| instruction.body.clone())
        .collect::<Vec<_>>();
    let system = format_guidance(&repo.name, &guidance);
    opencode::system_response(&system, &bodies).map_err(Into::into)
}

fn opencode_shell_env(event: &OpenCodeEvent) -> anyhow::Result<String> {
    opencode::environment_response(event.session_id()).map_err(Into::into)
}

/// The owner a claim from inside `repo` would carry when no harness names one:
/// the store's current agent, else the sole claimant of that repository.
///
/// `repo` is the entry the caller already bound the working directory to; a
/// directory outside any managed fork, or whose remotes could not be read, is
/// `None` and derives no owner.
pub(crate) fn owner_for(repo: Option<&RepoName>) -> anyhow::Result<Option<String>> {
    if let Some(owner) = std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|owner| !owner.trim().is_empty())
    {
        return Ok(Some(owner));
    }
    let Some(repo) = repo else {
        return Ok(None);
    };
    let store = Store::open(default_state_path())?;
    if let Some(owner) = store.current_agent() {
        return Ok(Some(owner.to_owned()));
    }
    let owners = store
        .claims(None)
        .into_iter()
        .filter(|claim| claim.repo == repo.as_str())
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
    let compact = event.source() == Some("compact");
    if compact {
        let _ = SessionState::update(home, CLAUDE_CODE, session_id, SessionState::clear)?;
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
    let Some(managed) = &matched.managed else {
        return Ok(None);
    };
    crate::seen::record_observation(
        Some(managed),
        Path::new(cwd),
        &Identity {
            owner: session_id.to_owned(),
            kind: crate::store::OwnerKind::HarnessSession,
        },
    );
    let repo = guidance_root(&matched);
    if compact {
        return Ok(None);
    }
    let state = SessionState::load(home, CLAUDE_CODE, session_id);
    let Some(notice) = notice_if_requested(&repo, &state, true)? else {
        return Ok(None);
    };
    let (notice, update) = notice.into_parts();
    let _ = SessionState::update(home, CLAUDE_CODE, session_id, move |state| {
        update.apply(state, &repo.root);
    })?;
    response(SESSION_START_WIRE_NAME, &notice)
        .map(Some)
        .map_err(Into::into)
}

fn post_tool_use(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    let Some(session_id) = event.session_id() else {
        return Ok(None);
    };
    let cache = Some((home, CLAUDE_CODE, session_id));
    let Some((registry, matched)) = relevant_tool_match(
        &ToolCall {
            tool: event.tool_name(),
            args: event.tool_input(),
            relevant: RELEVANT_TOOLS,
        },
        cache,
    )?
    else {
        return Ok(None);
    };
    if matched.is_managed()
        && let Some(cwd) = event.cwd()
    {
        crate::seen::record_observation(
            standing_in(cwd, &registry, cache)?.as_ref(),
            Path::new(cwd),
            &Identity {
                owner: session_id.to_owned(),
                kind: crate::store::OwnerKind::HarnessSession,
            },
        );
    }
    let repo = guidance_root(&matched);
    let state = SessionState::load(home, CLAUDE_CODE, session_id);
    let flags = state.repo(&repo.root);
    let notice = notice_if_requested(&repo, &state, matched.is_managed())?;
    let include_notice = notice.is_some();
    let include_guidance = matched.trusted
        && !flags.guided
        && event
            .cwd()
            .is_some_and(|cwd| !contains_cwd(&repo.root, cwd));
    if !include_notice && !include_guidance {
        return Ok(None);
    }

    let (notice_text, notice_update) = notice.map_or((None, None), |notice| {
        let (text, update) = notice.into_parts();
        (Some(text), Some(update))
    });
    let mut parts = Vec::new();
    if let Some(text) = notice_text {
        parts.push(text);
    }
    let guidance = include_guidance
        .then(|| guidance_for(&repo, &matched.candidate))
        .flatten();
    if let Some(guidance) = &guidance {
        parts.push(format_guidance(&repo.name, guidance));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let _ = SessionState::update(home, CLAUDE_CODE, session_id, move |state| {
        if let Some(update) = notice_update {
            update.apply(state, &repo.root);
        }
        if guidance.is_some() {
            state.mark_guided(&repo.root);
        }
    })?;
    response(POST_TOOL_USE_WIRE_NAME, &parts.join("\n"))
        .map(Some)
        .map_err(Into::into)
}

/// The tool an event says was called, and which tools the harness treats as
/// touching repository content.
struct ToolCall<'a> {
    tool: Option<&'a str>,
    args: Option<&'a serde_json::Value>,
    relevant: &'a [&'a str],
}

/// The touched-path match for a relevant tool call, with the registry it was
/// decided against — loaded only once there is a path to decide, so a pathless
/// call never touches (or fails on) the registry.
fn relevant_tool_match(
    call: &ToolCall<'_>,
    cache: Option<(&Path, &str, &str)>,
) -> anyhow::Result<Option<(Registry, Match)>> {
    let Some(tool) = call.tool else {
        return Ok(None);
    };
    if !call.relevant.contains(&tool) {
        return Ok(None);
    }
    let Some(args) = call.args else {
        return Ok(None);
    };
    let paths = argument_paths(tool, args);
    if paths.is_empty() {
        return Ok(None);
    }
    let registry = load(&default_config_path())?;
    let matched = match_with_trust(&paths, &registry, cache)?;
    Ok(matched.map(|matched| (registry, matched)))
}

/// The entry the event's working directory is inside, read through the same
/// session cache as the touched path: what a sighting keys its workspace on.
/// The touched path may be in another repository; the workspace is the cwd's.
fn standing_in(
    cwd: &str,
    registry: &Registry,
    cache: Option<(&Path, &str, &str)>,
) -> anyhow::Result<Option<RepoName>> {
    Ok(match_with_trust(&[PathBuf::from(cwd)], registry, cache)?
        .and_then(|matched| matched.managed))
}

/// Resolve the touched paths, reading each checkout's remotes once per session.
///
/// A read failure is reported on stderr and yields no remote facts for that
/// checkout; a `[trust] roots` rule still applies. A cache write failure is the
/// command's error, surfaced after resolution so the match itself is not lost.
fn match_with_trust(
    paths: &[PathBuf],
    registry: &Registry,
    cache: Option<(&Path, &str, &str)>,
) -> anyhow::Result<Option<Match>> {
    let mut cache_error = None;
    let mut remotes_of = |checkout: &Path| -> Option<BTreeMap<String, String>> {
        if let Some((home, harness, session_id)) = cache
            && let Some(cached) = SessionState::load(home, harness, session_id).remotes(checkout)
        {
            return Some(cached.clone());
        }
        let remotes = match bind::remotes(checkout) {
            Ok(remotes) => remotes,
            Err(error) => {
                eprintln!("knives hook: {error}");
                return None;
            }
        };
        if let Some((home, harness, session_id)) = cache
            && let Err(error) = SessionState::update(home, harness, session_id, |state| {
                state.record_remotes(checkout, remotes.clone());
            })
        {
            cache_error = Some(error);
        }
        Some(remotes)
    };
    let matched = match_checkout(paths, registry, &mut remotes_of);
    if let Some(error) = cache_error {
        return Err(error);
    }
    Ok(matched)
}

/// What guidance and session state key on: the match's nearest root and name.
fn guidance_root(matched: &Match) -> GuidanceRoot {
    GuidanceRoot {
        name: matched.name(),
        root: matched.root.clone(),
    }
}

fn pre_compact(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    if let Some(session_id) = event.session_id() {
        let _ = SessionState::update(home, CLAUDE_CODE, session_id, SessionState::clear)?;
    }
    Ok(None)
}

/// Whether the session's own repository is the matched root, so its native
/// instructions are not injected a second time. A cwd that no longer exists
/// still counts through its nearest existing ancestor.
fn contains_cwd(root: &Path, cwd: &str) -> bool {
    crate::hook::resolve::nearest_root(Path::new(cwd)).as_deref() == Some(root)
}

fn config_home() -> PathBuf {
    default_config_path()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

struct PreparedNotice {
    text: String,
    update: NoticeStateUpdate,
}

impl PreparedNotice {
    fn into_parts(self) -> (String, NoticeStateUpdate) {
        (self.text, self.update)
    }
}

struct NoticeStateUpdate {
    digest: String,
}

impl NoticeStateUpdate {
    fn apply(self, state: &mut SessionState, root: &Path) {
        state.record_notice(root, self.digest);
    }
}

fn notice_if_requested(
    repo: &GuidanceRoot,
    state: &SessionState,
    requested: bool,
) -> anyhow::Result<Option<PreparedNotice>> {
    if !requested {
        return Ok(None);
    }
    let store = Store::open(default_state_path())?;
    let claims = all_claims(&store);
    let digest = notice_digest(&repo.name, &repo.root, &claims);
    if state.notice_seen(&repo.root, &digest) {
        return Ok(None);
    }
    Ok(Some(PreparedNotice {
        text: format_notice_for(repo, &digest, &claims),
        update: NoticeStateUpdate { digest },
    }))
}

fn format_notice_for(repo: &GuidanceRoot, digest: &str, claims: &[crate::store::Claim]) -> String {
    let observations = crate::seen::load();
    let visible_claims = claim_lines(claims, &repo.name, &observations, jiff::Timestamp::now());
    format_notice(&repo.name, &repo.root, &visible_claims, digest)
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
