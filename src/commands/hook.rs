//! `knives hook`: harness adapters that never interrupt the calling session.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::cli::{Exit, HookHarness};
use crate::commands::claim::Identity;
use crate::config::{GuidanceRoot, Registry, default_config_path, load};
use crate::hook::claude_code::{
    Event, EventKind, POST_TOOL_USE_WIRE_NAME, SESSION_START_WIRE_NAME, response,
};
use crate::hook::guidance::{
    Guidance, body_digest, claim_lines, envelope_nonce, format_guidance, format_notice,
    format_refusal, guidance_for, mention_line, notice_digest,
};
use crate::hook::opencode::{self, Event as OpenCodeEvent, EventKind as OpenCodeEventKind};
use crate::hook::resolve::{Managed, Match, argument_paths, match_checkout};
use crate::hook::state::{Envelope, SessionState};
use crate::ids::UpstreamName;
use crate::lock::LockError;
use crate::store::{OwnerKind, Store, StoreError, default_state_path};

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
        OpenCodeEventKind::ChatSystem => opencode_chat_system(&event),
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
    let Some((registry, matched)) = relevant_tool_match(&ToolCall {
        tool: event.tool(),
        args: event.args(),
        relevant: OPENCODE_RELEVANT_TOOLS,
    })?
    else {
        return opencode::tool_response("").map_err(Into::into);
    };
    if matched.is_managed()
        && let Some(cwd) = event.cwd()
    {
        crate::seen::record_observation(
            standing_in(cwd, &registry).as_ref(),
            Path::new(cwd),
            &Identity {
                owner: session_id.to_owned(),
                kind: crate::store::OwnerKind::HarnessSession,
            },
        );
    }
    let repo = guidance_root(&matched);
    let requested = event.parts();
    let context = event.context();
    let mut unlocked = SessionState::load(home, OPENCODE, session_id);
    if let Some(context) = &context {
        unlocked.reconcile(context.turn, &context.guidance);
    }
    let notice = notice_if_requested(
        &repo,
        matched.managed.as_ref().filter(|_| requested.notice),
        &unlocked,
    )?;
    let guidance = (requested.guidance && matched.trusted && !unlocked.repo(&repo.root).guided)
        .then(|| guidance_for(&repo, &matched.candidate))
        .flatten();
    let candidates = Candidates::new(&repo, notice.as_ref(), guidance.as_ref(), &event.system());
    let outstanding = candidates.outstanding(&unlocked);
    if outstanding.is_empty() {
        return opencode::tool_response("").map_err(Into::into);
    }

    // Parallel tool calls each found these parts outstanding above; only the
    // one that records them under the session lock renders them.
    let nonce = envelope_nonce();
    let envelope = context.as_ref().map(|context| Envelope {
        nonce: &nonce,
        turn: context.turn,
    });
    let mut claimed = None;
    let outstanding = match SessionState::update(home, OPENCODE, session_id, |state| {
        if let Some(context) = &context {
            state.reconcile(context.turn, &context.guidance);
        }
        let locked = candidates.outstanding(state);
        candidates.record(state, &locked, envelope);
        claimed = Some(locked);
    }) {
        Ok(_) => claimed.unwrap_or_default(),
        // Another call of this session holds the record: it delivers what it
        // claims, and whatever it leaves stays due for a later call.
        Err(error) if matches!(error.downcast_ref(), Some(LockError::Held { .. })) => {
            Outstanding::default()
        }
        Err(error) => {
            // The state file is a saving: a read-only config home gets its
            // additions on every event rather than never.
            eprintln!("knives hook: {error:#}");
            outstanding
        }
    };

    let mut additions = Vec::new();
    if outstanding.notice
        && let Some(notice) = notice
    {
        additions.push(notice.text);
    }
    if let Some(guidance) =
        guidance.and_then(|guidance| guidance.keeping(&outstanding.bodies, &outstanding.mentions))
    {
        additions.push(format_guidance(&repo.name, &guidance, &nonce));
    }
    opencode::tool_response(&additions.join("\n")).map_err(Into::into)
}

/// What one tool call could add, and which of its parts the session's system
/// prompt already carries, so no session state can make them due again.
struct Candidates<'a> {
    repo: &'a GuidanceRoot,
    notice: Option<&'a PreparedNotice>,
    guidance: Option<&'a Guidance>,
    /// Per instruction file of `guidance`: its body's digest, and whether the
    /// system prompt already carries the body.
    bodies: Vec<(String, bool)>,
    /// Per mention of `guidance`: whether the system prompt already carries its line.
    mentions_held: Vec<bool>,
}

impl<'a> Candidates<'a> {
    fn new(
        repo: &'a GuidanceRoot,
        notice: Option<&'a PreparedNotice>,
        guidance: Option<&'a Guidance>,
        system: &[&str],
    ) -> Self {
        let held = |text: &str| system.iter().any(|entry| entry.contains(text));
        let bodies = guidance.map_or_else(Vec::new, |guidance| {
            guidance
                .bodies
                .iter()
                .map(|file| (body_digest(&file.body), held(&file.body)))
                .collect()
        });
        let mentions_held = guidance.map_or_else(Vec::new, |guidance| {
            guidance
                .mentions
                .iter()
                .map(|path| held(&mention_line(path)))
                .collect()
        });
        Self {
            repo,
            notice,
            guidance,
            bodies,
            mentions_held,
        }
    }

    /// The parts `state` says the session does not have yet.
    fn outstanding(&self, state: &SessionState) -> Outstanding {
        let notice = self
            .notice
            .is_some_and(|notice| !state.notice_seen(&self.repo.root, &notice.update.digest));
        let guidance = self.guidance.is_some() && !state.repo(&self.repo.root).guided;
        if !guidance {
            return Outstanding {
                notice,
                ..Outstanding::default()
            };
        }
        Outstanding {
            notice,
            guidance,
            bodies: self
                .bodies
                .iter()
                .map(|(digest, held)| !held && !state.guidance_body_seen(digest))
                .collect(),
            mentions: self.mentions_held.iter().map(|held| !held).collect(),
        }
    }

    /// Records `outstanding` as delivered. A root whose guidance was due is
    /// guided even when the session held all of it already.
    ///
    /// With an `envelope` (the harness shows the model's context), the block
    /// is recorded as a delivery, and the root's mark rests on it and on every
    /// earlier block that carried one of its bodies, so the mark goes when the
    /// context loses them.
    fn record(
        &self,
        state: &mut SessionState,
        outstanding: &Outstanding,
        envelope: Option<Envelope<'_>>,
    ) {
        if outstanding.notice
            && let Some(notice) = self.notice
        {
            state.record_notice(&self.repo.root, notice.update.digest.clone());
        }
        if outstanding.guidance {
            state.mark_guided(&self.repo.root);
        }
        let rendered = self
            .bodies
            .iter()
            .zip(&outstanding.bodies)
            .filter(|(_, rendered)| **rendered)
            .map(|((digest, _), _)| digest.clone())
            .collect::<BTreeSet<_>>();
        for digest in &rendered {
            state.record_guidance_body(digest.clone());
        }
        let Some(envelope) = envelope.filter(|_| outstanding.guidance) else {
            return;
        };
        for ((digest, held), rendered) in self.bodies.iter().zip(&outstanding.bodies) {
            if !held && !rendered {
                state.rest_on_body(&self.repo.root, digest);
            }
        }
        if outstanding.renders() {
            state.record_delivery(envelope, &self.repo.root, rendered);
        }
    }
}

/// The parts of a tool call's candidates the session does not have yet.
#[derive(Debug, Default)]
struct Outstanding {
    notice: bool,
    /// The root's guidance was due, whether or not any of it is left to render.
    guidance: bool,
    /// Per instruction file and per mention: whether to render it.
    bodies: Vec<bool>,
    mentions: Vec<bool>,
}

impl Outstanding {
    const fn is_empty(&self) -> bool {
        !self.notice && !self.guidance
    }

    /// Whether any instruction file or mention is left to render.
    fn renders(&self) -> bool {
        self.bodies
            .iter()
            .chain(&self.mentions)
            .any(|render| *render)
    }
}

fn opencode_chat_system(event: &OpenCodeEvent) -> anyhow::Result<String> {
    let Some(directory) = event.directory() else {
        return opencode::system_response("", &[]).map_err(Into::into);
    };
    let registry = load(&default_config_path())?;
    let Some(matched) = match_checkout(&[PathBuf::from(directory)], &registry) else {
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
    let system = format_guidance(&repo.name, &guidance, &envelope_nonce());
    opencode::system_response(&system, &bodies).map_err(Into::into)
}

fn opencode_shell_env(event: &OpenCodeEvent) -> anyhow::Result<String> {
    opencode::environment_response(event.session_id()).map_err(Into::into)
}

/// The owner a claim from inside `repo` would carry when no harness names one:
/// the store's current agent, else the sole claimant of that repository.
///
/// `repo` is the fork the caller already bound the working directory to, by
/// the name its claims are kept under; a directory outside any managed fork,
/// or whose remotes could not be read, is `None` and derives no owner. An
/// OS-user claim names nobody in particular, so it seeds no derived owner:
/// otherwise one anonymous claim would hand every later anonymous caller the
/// same "derived" name, and they would resume each other's claims.
pub(crate) fn owner_for(repo: Option<&UpstreamName>) -> anyhow::Result<Option<String>> {
    if let Some(owner) = std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|owner| !owner.trim().is_empty())
    {
        return Ok(Some(owner));
    }
    let Some(repo) = repo else {
        return Ok(None);
    };
    // Claims and the current agent only: no branch statement is asked about.
    let store = Store::open(default_state_path(), &[])?;
    if let Some(owner) = store.current_agent() {
        return Ok(Some(owner.to_owned()));
    }
    let owners = store
        .claims(Some(repo))
        .into_iter()
        .filter(|claim| claim.kind != OwnerKind::OsUser)
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
    let Some(matched) = match_checkout(&[PathBuf::from(cwd)], &registry) else {
        return Ok(None);
    };
    let Some(managed) = &matched.managed else {
        return Ok(None);
    };
    crate::seen::record_observation(
        Some(&managed.upstream),
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
    let Some(notice) = notice_if_requested(&repo, Some(managed), &state)? else {
        return Ok(None);
    };
    let (notice, update) = notice.into_parts();
    remember(home, CLAUDE_CODE, session_id, move |state| {
        update.apply(state, &repo.root);
    });
    response(SESSION_START_WIRE_NAME, &notice)
        .map(Some)
        .map_err(Into::into)
}

fn post_tool_use(event: &Event, home: &Path) -> anyhow::Result<Option<String>> {
    let Some(session_id) = event.session_id() else {
        return Ok(None);
    };
    let Some((registry, matched)) = relevant_tool_match(&ToolCall {
        tool: event.tool_name(),
        args: event.tool_input(),
        relevant: RELEVANT_TOOLS,
    })?
    else {
        return Ok(None);
    };
    if matched.is_managed()
        && let Some(cwd) = event.cwd()
    {
        crate::seen::record_observation(
            standing_in(cwd, &registry).as_ref(),
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
    let notice = notice_if_requested(&repo, matched.managed.as_ref(), &state)?;
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
        parts.push(format_guidance(&repo.name, guidance, &envelope_nonce()));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    remember(home, CLAUDE_CODE, session_id, move |state| {
        if let Some(update) = notice_update {
            update.apply(state, &repo.root);
        }
        if guidance.is_some() {
            state.mark_guided(&repo.root);
        }
    });
    response(POST_TOOL_USE_WIRE_NAME, &parts.join("\n"))
        .map(Some)
        .map_err(Into::into)
}

/// Persist a session-state change, reporting a failure without failing the
/// response. The state file is a saving — one notice, one guidance per session
/// — and losing the saving beats losing the answer: a read-only config home
/// gets its guidance on every event rather than never.
fn remember(home: &Path, harness: &str, session_id: &str, apply: impl FnOnce(&mut SessionState)) {
    if let Err(error) = SessionState::update(home, harness, session_id, apply) {
        eprintln!("knives hook: {error:#}");
    }
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
fn relevant_tool_match(call: &ToolCall<'_>) -> anyhow::Result<Option<(Registry, Match)>> {
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
    let matched = match_checkout(&paths, &registry);
    Ok(matched.map(|matched| (registry, matched)))
}

/// The fork the event's working directory is inside, by the name its claims
/// are kept under: what a sighting keys its workspace on. The touched path may
/// be in another repository; the workspace is the cwd's.
fn standing_in(cwd: &str, registry: &Registry) -> Option<UpstreamName> {
    match_checkout(&[PathBuf::from(cwd)], registry)
        .and_then(|matched| matched.managed)
        .map(|managed| managed.upstream)
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

/// The notice for `repo`, the root of the managed fork `managed`, when one was
/// asked for (`managed` is `None` when it was not) and the session lacks it.
///
/// The roster is the claims kept under the fork's upstream name; the notice
/// names the fork by its registry key.
///
/// A state file this knives refuses to read ([`StoreError::Unmigrated`], or
/// [`StoreError::FormerNames`] for one still keeping forks under their registry
/// keys) is the notice instead: the claims it holds cannot be read, and an
/// empty roster would tell the agent nobody holds the branch it is about to
/// take. The refusal goes into the agent's context, not stderr, because the
/// model never reads a hook's stderr; its digest is the refusal's, so it
/// repeats once per session until the migration runs.
fn notice_if_requested(
    repo: &GuidanceRoot,
    managed: Option<&Managed>,
    state: &SessionState,
) -> anyhow::Result<Option<PreparedNotice>> {
    let Some(managed) = managed else {
        return Ok(None);
    };
    let store = match Store::open(default_state_path(), &[]) {
        Ok(store) => store,
        Err(error @ (StoreError::Unmigrated { .. } | StoreError::FormerNames { .. })) => {
            let text = error.to_string();
            let digest = format!("unmigrated:{}", body_digest(&text));
            if state.notice_seen(&repo.root, &digest) {
                return Ok(None);
            }
            return Ok(Some(PreparedNotice {
                text: format_refusal(&repo.name, &text),
                update: NoticeStateUpdate { digest },
            }));
        }
        Err(error) => return Err(error.into()),
    };
    let claims = all_claims(&store);
    let digest = notice_digest(&managed.upstream, &repo.root, &claims);
    if state.notice_seen(&repo.root, &digest) {
        return Ok(None);
    }
    Ok(Some(PreparedNotice {
        text: format_notice_for(repo, &managed.upstream, &digest, &claims),
        update: NoticeStateUpdate { digest },
    }))
}

fn format_notice_for(
    repo: &GuidanceRoot,
    upstream: &UpstreamName,
    digest: &str,
    claims: &[crate::store::Claim],
) -> String {
    let observations = crate::seen::load();
    let visible_claims = claim_lines(claims, upstream, &observations, jiff::Timestamp::now());
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
