//! Claim identity resolution.
//!
//! [`current_identity`] records both the claimant's name and how it was resolved.
//! The claim-lifecycle matrix introduced in Task 2 consumes that resolution kind.
//!
//! On a fork that shares its ledger, `start` and `finish` read a branch's
//! claims the same way ([`SharedClaims`]), weigh them with the same rule
//! ([`decide`], [`SharedClaims::readable`]), and say what became of a read or
//! a change of the ledger's claim in the same words.
//!
//! Enforcement layer three. Advisory on purpose: layers one and two are
//! default-correct paths and detectors, and hard refusal waits for evidence
//! that advice was insufficient.
// allow: SIZE_OK: claim coordination keeps identity-resolution behavior beside its tests.

use crate::commands::hook::owner_for;
use crate::config::Registry;
use crate::ids::{BranchName, BranchTarget, CommitId, UpstreamName};
use crate::ledger::{Draft, Scribe, inline_human_text, problem_human_text};
use crate::ledger_git::{ClaimRecord, ClaimRef, HeldClaim, Refusal};
use crate::ledger_sweep::{self, ClaimOutcome, LedgerFailure, NotApplied, SharedFork};
use crate::store::{Claim, OwnerKind};

/// The registry the current directory was bound against, and the fork it is
/// inside, by the name its claims are kept under, when it is inside one.
///
/// Together they name, from the claims in knives state, who is acting there.
/// The registry also says which names a state file an older knives left still
/// keeps forks under, and such a file is refused rather than read
/// ([`crate::store::Store::open`]).
#[derive(Debug, Clone, Copy)]
pub struct Bound<'a> {
    pub registry: &'a Registry,
    pub upstream: Option<&'a UpstreamName>,
}

/// A claimant and the source that established its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub owner: String,
    pub kind: OwnerKind,
}

/// The inputs relevant to taking or resuming a claim.
#[derive(Debug)]
pub struct ClaimContext<'a> {
    /// Every claim present on the branch. On a fork that shares its ledger:
    /// the ledger's claim, and a local claim, one in this machine's state
    /// file with no [`Claim::ledger_commit`]. A copy of a ledger claim in
    /// the state file is never one: it decides nothing. On any other fork:
    /// the state file's claim.
    pub held: Vec<&'a Claim>,
    pub identity: &'a Identity,
    pub in_claimed_workspace: bool,
    /// This machine's name on the fork's ledger; `None` on a fork that
    /// shares no ledger, where every claim is this machine's.
    pub machine: Option<&'a str>,
}

/// The mutually exclusive outcomes for a claim attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    Take,
    Resume { possession: bool },
    RefuseAnonymous,
    RefuseHeld,
}

/// Chooses the claim outcome from identity, possession, and the claims
/// present.
///
/// With no claim present the caller takes the branch. It resumes only when
/// every claim present permits it, and is refused by the first one that
/// does not ([`refusing`]), whichever kind of claim that is: a local claim
/// an older knives took binds its own machine as it always did, and a
/// ledger claim binds every machine that shares the ledger. `possession` is
/// whether standing in the workspace is what permitted it.
pub fn decide(context: &ClaimContext<'_>) -> ClaimDecision {
    if context.held.is_empty() {
        return ClaimDecision::Take;
    }
    if let Some(claim) = refusing(context) {
        return if claim.kind == OwnerKind::OsUser && context.identity.kind == OwnerKind::OsUser {
            ClaimDecision::RefuseAnonymous
        } else {
            ClaimDecision::RefuseHeld
        };
    }
    ClaimDecision::Resume {
        possession: context
            .held
            .iter()
            .any(|claim| permits_by_possession(claim, context)),
    }
}

/// The first claim present that does not permit the caller to resume, if
/// one does not.
///
/// A claim permits its own owner: the same name resolved the same way,
/// never two anonymous `os-user` callers, who cannot be told apart. It
/// also permits a caller standing in the branch's workspace, as long as
/// it was taken on this machine ([`taken_here`]): the directory is evidence
/// of who works there only on the machine whose directory it is.
pub fn refusing<'a>(context: &ClaimContext<'a>) -> Option<&'a Claim> {
    context
        .held
        .iter()
        .copied()
        .find(|claim| !permits_by_possession(claim, context) && !names(claim, context.identity))
}

/// Whether `claim` was taken on this machine, the one named `machine` on
/// the fork's ledger: every claim on a fork that shares no ledger (`None`),
/// a local claim, and a ledger claim whose machine is this one.
pub fn taken_here(claim: &Claim, machine: Option<&str>) -> bool {
    machine.is_none() || claim.ledger_commit.is_none() || claim.machine.as_deref() == machine
}

fn permits_by_possession(claim: &Claim, context: &ClaimContext<'_>) -> bool {
    context.in_claimed_workspace && taken_here(claim, context.machine)
}

/// Whether `claim` names `identity` as its holder.
fn names(claim: &Claim, identity: &Identity) -> bool {
    claim.kind == identity.kind && claim.kind != OwnerKind::OsUser && claim.owner == identity.owner
}

/// Whether standing in the claimed workspace lets this caller release the
/// claim.
///
/// [`decide`] resumes on possession alone, which is right for taking work up:
/// the directory is the evidence that the claim's own session left it there.
/// Handing the claim back is the opposite question. A takeover gives the
/// directory to its new holder, and an agent runs `knives finish` from inside
/// it, so possession alone would let any later session standing there hand
/// back a claim it never took, and the directory would go with it. Equal owner
/// names release, whatever kind of identity each side is; two anonymous
/// callers under one OS user name go on releasing each other's claims from
/// inside the directory, as they always have.
pub fn possession_permits_release(claim: &Claim, identity: &Identity) -> bool {
    claim.owner == identity.owner
}

/// Renders the complete claim context shared by refusals and other claim
/// lifecycle notices.
///
/// The owner name and the reason are the text of whatever session took the
/// claim — another agent of this machine, through `knives start --why` — and
/// every session of the machine reads that state file, so both reach the
/// terminal escaped ([`crate::ledger::inline_human_text`]): a reason holding
/// a terminal control sequence would otherwise drive the terminal of every
/// caller this context is printed to, which is the refused `start`, the
/// refused `finish`, the notice after a seizure and the notice after a
/// resume. The age and the sighting are this process's own words about
/// timestamps, and need no escaping.
pub fn render_claim_context(
    claim: &Claim,
    last_seen: crate::seen::LastSeen,
    now: jiff::Timestamp,
) -> String {
    let claimed_age =
        crate::ledger::age(&claim.started, now).unwrap_or_else(|| "unknown".to_owned());
    format!(
        "{} is claimed by {} ({}), claimed {claimed_age} ago, {}: {}",
        claim.key(),
        crate::ledger::inline_human_text(&claim.owner),
        owner_kind_label(claim.kind),
        render_last_seen(last_seen, now),
        crate::ledger::inline_human_text(&claim.why),
    )
}

/// [`render_claim_context`] for a claim on a fork that shares its ledger,
/// taken on machine `machine`.
///
/// `last_seen` is its sighting where its activity can be read here; where
/// it cannot (`None`), the line says when the claim was taken instead, and
/// nothing about a sighting: this machine's records of another machine's
/// claim, or of one another config home recorded, are empty for want of
/// looking, and "no activity observed" would say the work is idle. Every
/// value but the age and the sighting was written by whichever session took
/// the claim, on whichever machine, so each reaches the terminal escaped.
pub fn render_shared_claim_context(
    claim: &Claim,
    machine: &str,
    last_seen: Option<crate::seen::LastSeen>,
    now: jiff::Timestamp,
) -> String {
    let claimed_age =
        crate::ledger::age(&claim.started, now).unwrap_or_else(|| "unknown".to_owned());
    let holder = format!(
        "{} is claimed by {} ({}) on machine {}",
        claim.key(),
        crate::ledger::inline_human_text(&claim.owner),
        owner_kind_label(claim.kind),
        crate::ledger::inline_human_text(machine),
    );
    let why = crate::ledger::inline_human_text(&claim.why);
    last_seen.map_or_else(
        || {
            format!(
                "{holder}, taken {} ({claimed_age} ago): {why}",
                crate::ledger::inline_human_text(&claim.started)
            )
        },
        |last_seen| {
            format!(
                "{holder}, claimed {claimed_age} ago, {}: {why}",
                render_last_seen(last_seen, now)
            )
        },
    )
}

/// Why the activity of `claim`, taken on machine `machine`, cannot be read
/// on this machine, `here`.
///
/// Its sightings and its process scan are records of the machine that took
/// it, and of the config home that recorded it: on another machine there
/// are none, and on this one a config home that did not record the claim
/// (a new one under the same machine name) has none either. The place is
/// where the claim says it was taken, which its holder wrote, so it goes
/// out escaped, as the machine names do.
pub fn unreadable_activity(claim: &Claim, machine: &str, here: &str) -> String {
    if machine == here {
        format!(
            "its activity cannot be read here: this config home did not record that claim, \
             which was taken in {}",
            claim.place.as_deref().map_or_else(
                || "a place it does not record".to_owned(),
                crate::ledger::inline_human_text
            )
        )
    } else {
        format!(
            "its activity cannot be read from this machine ({}): its sightings and its \
             processes are on machine {}",
            crate::ledger::inline_human_text(here),
            crate::ledger::inline_human_text(machine)
        )
    }
}

/// Renders a compact claim row for an advisory surface.
///
/// The subject, the owner name and the reason are another session's text,
/// escaped for the same reason [`render_claim_context`] escapes them: these
/// rows are printed by the session hook, into the terminal of a caller that
/// wrote none of it. The subject is the branch the state file records, which
/// a ref read from a remote can give a C1 character.
pub fn render_claim_line(
    subject: &str,
    claim: &Claim,
    last_seen: crate::seen::LastSeen,
    now: jiff::Timestamp,
) -> String {
    let claimed_age =
        crate::ledger::age(&claim.started, now).unwrap_or_else(|| "unknown".to_owned());
    format!(
        "{} ({}, {}, claimed {claimed_age} ago, {}): {}",
        crate::ledger::inline_human_text(subject),
        crate::ledger::inline_human_text(&claim.owner),
        owner_kind_label(claim.kind),
        render_last_seen(last_seen, now),
        crate::ledger::inline_human_text(&claim.why),
    )
}

/// Renders the observation state without implying a liveness guarantee.
pub fn render_last_seen(last_seen: crate::seen::LastSeen, now: jiff::Timestamp) -> String {
    match last_seen {
        crate::seen::LastSeen::At(timestamp) => crate::ledger::age(&timestamp.to_string(), now)
            .map_or_else(
                || "not seen within the observation window".to_owned(),
                |age| format!("last seen {age} ago"),
            ),
        crate::seen::LastSeen::NoneSinceClaim => "no activity observed since claimed".to_owned(),
        crate::seen::LastSeen::NoneWithinWindow => {
            "not seen within the observation window".to_owned()
        }
        crate::seen::LastSeen::Unreadable => {
            "the jj operation log could not be read, so no sighting is reported".to_owned()
        }
    }
}

/// The durable token for a claim observation.
pub fn last_seen_provenance(last_seen: crate::seen::LastSeen) -> String {
    match last_seen {
        crate::seen::LastSeen::At(timestamp) => timestamp.to_string(),
        crate::seen::LastSeen::NoneSinceClaim => "none-since-claim".to_owned(),
        crate::seen::LastSeen::NoneWithinWindow => "none-within-window".to_owned(),
        crate::seen::LastSeen::Unreadable => "unreadable-operation-log".to_owned(),
    }
}

/// The stable, human-readable identity-source label.
pub const fn owner_kind_label(kind: OwnerKind) -> &'static str {
    match kind {
        OwnerKind::HarnessSession => "harness-session",
        OwnerKind::WorkspaceDerived => "workspace-derived",
        OwnerKind::OsUser => "os-user",
    }
}

/// How a harness names the session a shell belongs to, in precedence order.
///
/// `KNIVES_OWNER` is what this tool's own `OpenCode` plugin injects; Claude Code
/// and OMP each export their session id.
pub const HARNESS_SESSION_VARIABLES: [&str; 3] =
    ["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "OMP_SESSION_ID"];

/// Resolves the identity that should own a claim.
///
/// A harness that names its session (`HARNESS_SESSION_VARIABLES`) is the
/// identity. When none does, `bound` — the fork the caller already bound, by
/// the name its claims are kept under (dispatch binds the working directory
/// once per invocation; a verb passes the fork it acts on) — can identify its
/// active owner from knives state. The OS user is the fallback for a human at a
/// terminal.
///
/// A blank harness variable is a harness bug, not an identity: every session of
/// that harness would share one claim.
pub fn current_identity(bound: Bound<'_>) -> anyhow::Result<Identity> {
    if let Some(identity) = harness_identity() {
        return Ok(identity);
    }
    if let Some(owner) = owner_for(bound)? {
        return Ok(Identity {
            owner,
            kind: OwnerKind::WorkspaceDerived,
        });
    }
    Ok(os_user())
}

/// [`current_identity`] for a caller standing in no fork.
///
/// No claim in knives state can name it: the harness's session, else the OS
/// user. What a caller that has no registry to bind against, or no fork to
/// act on, is.
pub fn identity_outside_a_fork() -> Identity {
    harness_identity().unwrap_or_else(os_user)
}

/// The session a harness names (`HARNESS_SESSION_VARIABLES`), if one does.
fn harness_identity() -> Option<Identity> {
    harness_session().map(|(_, owner)| Identity {
        owner,
        kind: OwnerKind::HarnessSession,
    })
}

/// The variable a harness names this session by, and its value.
pub fn harness_session() -> Option<(&'static str, String)> {
    HARNESS_SESSION_VARIABLES.iter().find_map(|&name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| (name, value))
    })
}

fn os_user() -> Identity {
    Identity {
        owner: std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
        kind: OwnerKind::OsUser,
    }
}

/// The claims of a branch of a fork that shares its ledger, as `start` and
/// `finish` find them on the ledger's remote and in this machine's state
/// file.
#[derive(Debug)]
pub struct SharedClaims {
    /// The ledger's claim: its commit, and the claim it records, as the
    /// state file keeps a copy of it, or why its record does not read.
    pub ledger: Option<(CommitId, Result<Claim, String>)>,
    /// What the state file holds for the branch: a local claim, or a copy
    /// of a ledger claim.
    pub recorded: Option<Claim>,
}

impl SharedClaims {
    pub fn new(target: &BranchTarget, held: Option<HeldClaim>, recorded: Option<Claim>) -> Self {
        let ledger = held.map(|held| {
            let claim = held
                .record
                .map(|record| copy_of(target, &held.commit, record));
            (held.commit, claim)
        });
        Self { ledger, recorded }
    }

    /// The ledger's claim, where its record reads.
    pub fn on_ledger(&self) -> Option<&Claim> {
        self.ledger
            .as_ref()
            .and_then(|(_, claim)| claim.as_ref().ok())
    }

    /// A local claim: what the state file holds, where that is not a copy of
    /// a ledger claim, which decides nothing.
    pub fn local(&self) -> Option<&Claim> {
        self.recorded
            .as_ref()
            .filter(|claim| claim.ledger_commit.is_none())
    }

    /// Every claim present whose holder can be read: the ledger's, then a
    /// local one.
    pub fn present(&self) -> Vec<&Claim> {
        self.on_ledger().into_iter().chain(self.local()).collect()
    }

    /// Whether any claim is present, one whose holder cannot be read
    /// included.
    pub fn any(&self) -> bool {
        self.ledger.is_some() || self.local().is_some()
    }

    /// Whether `claim`'s activity, its sightings and the processes in its
    /// workspace, can be read on this machine, `here`.
    ///
    /// A local claim's can. The ledger's can only where it was taken on this
    /// machine and this config home recorded that same claim commit: another
    /// machine's records are not here, and neither are those of another
    /// config home under this machine's name.
    pub fn readable(&self, claim: &Claim, here: &str) -> bool {
        claim.ledger_commit.is_none()
            || (claim.machine.as_deref() == Some(here)
                && self
                    .recorded
                    .as_ref()
                    .is_some_and(|recorded| recorded.ledger_commit == claim.ledger_commit))
    }
}

/// The ledger's claim `record` at `commit`, as this machine's state file
/// keeps a copy of it: under `target`, the branch the command is for,
/// whatever its record says.
pub fn copy_of(target: &BranchTarget, commit: &CommitId, record: ClaimRecord) -> Claim {
    Claim {
        repo: target.repo.to_string(),
        branch: target.branch.to_string(),
        owner: record.owner,
        kind: record.kind,
        why: record.why,
        started: record.started,
        files: Vec::new(),
        place: record.place,
        machine: Some(record.machine),
        ledger_commit: Some(commit.as_str().to_owned()),
    }
}

/// The machine `claim` was taken on: a ledger claim's own, and this one,
/// `here`, for a local claim, which this machine took.
pub fn claimed_on<'a>(claim: &'a Claim, here: &'a str) -> &'a str {
    claim
        .ledger_commit
        .as_ref()
        .and(claim.machine.as_deref())
        .unwrap_or(here)
}

/// How an entry names the last sighting of a claim its write displaced.
///
/// The sighting's durable token where the claim's activity can be read here
/// (`last_seen`), else the machine it was taken on, whose records hold it.
pub fn sighting_provenance(last_seen: Option<crate::seen::LastSeen>, machine: &str) -> String {
    last_seen.map_or_else(
        || format!("on machine {machine}"),
        |last_seen| format!("last seen {}", last_seen_provenance(last_seen)),
    )
}

/// The git directory a shared fork's ledger travels through, as a terminal
/// reads it: a path off disk.
pub fn git_dir_of(shared: &SharedFork) -> String {
    inline_human_text(
        &shared
            .destination
            .repository
            .git_dir()
            .display()
            .to_string(),
    )
}

/// Why the claim of `branch` could not be read from the ledger: where it
/// was read from, and git's account of why, which quotes the remote.
pub fn unread_claim(branch: &BranchName, shared: &SharedFork, failure: &LedgerFailure) -> String {
    format!(
        "the claim of {branch} could not be read from the ledger {} ({}): {}",
        failure.ledger,
        git_dir_of(shared),
        problem_human_text(&failure.cause)
    )
}

/// Why a change of a claim reached nothing: the ledger, its git directory,
/// and `cause`, git's account or this process's ([`unapplied_cause`]).
pub fn unwritten(shared: &SharedFork, failure: &LedgerFailure, cause: &str) -> String {
    format!(
        "the ledger {} could not be written ({}): {}",
        failure.ledger,
        git_dir_of(shared),
        problem_human_text(cause)
    )
}

/// What a claim change the remote applied nothing of failed on.
///
/// git's own account, or, where git refused this machine's ref, that
/// another machine may be committing under its name and how to rename it.
pub fn unapplied_cause(shared: &SharedFork, why: &NotApplied, failure: &LedgerFailure) -> String {
    match why {
        NotApplied::Refused {
            refusal: Refusal::MachineRef,
            ..
        } => format!(
            "git refused this machine's ref refs/knives/{} on origin: another machine may be \
             committing under the same name; give each machine a unique knives.machine with \
             `git --git-dir={} config knives.machine <unique-name>` ({})",
            shared.destination.machine,
            shared.destination.repository.git_dir().display(),
            failure.cause
        ),
        _ => failure.cause.clone(),
    }
}

/// The holder of a claim commit whose record does not read, as far as it
/// can be named.
///
/// By its ref and commit, and why its record does not read, which is the
/// text of whoever wrote the commit, so it goes out escaped.
pub fn unreadable_holder(
    target: &BranchTarget,
    claim: &ClaimRef,
    commit: &CommitId,
    cause: &str,
) -> String {
    format!(
        "{} is claimed on the ledger by {claim} at {}, but the holder cannot be read: {}",
        target.key(),
        commit.as_str(),
        problem_human_text(cause),
    )
}

/// [`ledger_sweep::change_claim`] with the entry `draft`, written by
/// `scribe`; the problems its commit passed over go to stderr, one line
/// each, as a sweep's report shows them.
pub fn change_on_ledger(
    change: ledger_sweep::ClaimChange<'_>,
    scribe: &Scribe,
    draft: &Draft<'_>,
) -> ClaimOutcome {
    let changed = ledger_sweep::change_claim(change, || scribe.record_to(draft));
    for problem in &changed.problems {
        eprintln!("!! {}", problem_human_text(problem));
    }
    changed.outcome
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use std::path::Path;

    use super::*;
    use crate::config::test_support::{EnvironmentGuard, environment_lock};
    use crate::store::{Claim, OwnerKind};

    fn held(owner: &str, kind: OwnerKind) -> Claim {
        Claim {
            repo: "a-repo".into(),
            branch: "feat/alpha".into(),
            owner: owner.into(),
            kind,
            why: "w".into(),
            started: "2026-01-01T00:00:00Z".into(),
            files: Vec::new(),
            place: None,
            machine: None,
            ledger_commit: None,
        }
    }

    fn identity(owner: &str, kind: OwnerKind) -> Identity {
        Identity {
            owner: owner.into(),
            kind,
        }
    }

    /// `owner`'s claim on the ledger, taken on `machine`, as `start` reads
    /// it: with its machine and its claim commit.
    fn on_ledger(owner: &str, machine: &str) -> Claim {
        Claim {
            machine: Some(machine.into()),
            ledger_commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
            ..held(owner, OwnerKind::HarnessSession)
        }
    }

    /// What `start` weighs on machine `here` of a fork that shares its
    /// ledger, for `caller`, with the claims `held` present.
    fn shared<'a>(
        here: &'a str,
        held: Vec<&'a Claim>,
        caller: &'a Identity,
        in_claimed_workspace: bool,
    ) -> ClaimContext<'a> {
        ClaimContext {
            held,
            identity: caller,
            in_claimed_workspace,
            machine: Some(here),
        }
    }

    #[test]
    fn a_blank_knives_owner_falls_back_to_the_os_user() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.set("KNIVES_OWNER", "   ");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.remove("OMP_SESSION_ID");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    fn a_harness_owner_resolves_as_a_harness_session() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        environment.set("KNIVES_OWNER", "agent-one");
        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "agent-one");
        assert_eq!(identity.kind, crate::store::OwnerKind::HarnessSession);
    }

    #[test]
    fn a_claude_code_session_resolves_as_a_harness_session() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        environment.remove("KNIVES_OWNER");
        environment.set("CLAUDE_CODE_SESSION_ID", "abc-123");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "abc-123");
        assert_eq!(identity.kind, crate::store::OwnerKind::HarnessSession);
    }

    #[test]
    fn an_omp_session_resolves_as_a_harness_session() {
        // OMP exports its session id into every shell it spawns and nothing
        // else; without this, two OMP sessions in one repository fall through to
        // the same derived owner and resume each other's claims.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.set("OMP_SESSION_ID", "01a0-session");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "01a0-session");
        assert_eq!(identity.kind, crate::store::OwnerKind::HarnessSession);
    }

    #[test]
    fn a_blank_claude_code_session_falls_back_to_the_os_user() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.remove("KNIVES_OWNER");
        environment.set("CLAUDE_CODE_SESSION_ID", "   ");
        environment.remove("OMP_SESSION_ID");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    fn the_user_fallback_is_anonymous() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.remove("OMP_SESSION_ID");
        environment.set("USER", "terminal-user");
        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    /// A state file whose only claim, on `repo`, is held by `state-owner` with
    /// the given identity kind.
    fn state_with_one_claimant(home: &Path, kind: &str) {
        std::fs::write(
            home.join("state.json"),
            format!(
                r#"{{"claims":{{"repo/feat/owner":{{"repo":"repo","branch":"feat/owner","owner":"state-owner","kind":"{kind}","why":"test","started":"2026-01-01T00:00:00Z","files":[]}}}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_bound_repository_with_one_claimant_resolves_as_workspace_derived() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        let home = tempfile::tempdir().unwrap();
        state_with_one_claimant(home.path(), "harness-session");
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.remove("OMP_SESSION_ID");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: Some(&UpstreamName::new("repo")),
        })
        .unwrap();

        assert_eq!(identity.owner, "state-owner");
        assert_eq!(identity.kind, crate::store::OwnerKind::WorkspaceDerived);
    }

    #[test]
    fn an_anonymous_claimant_seeds_no_derived_owner() {
        // An OS-user claim names nobody in particular. Deriving it as the
        // repository's owner would hand the next anonymous caller the same
        // name — and `decide` would let them resume a claim that is not theirs.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        let home = tempfile::tempdir().unwrap();
        state_with_one_claimant(home.path(), "os-user");
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.remove("OMP_SESSION_ID");
        environment.set("USER", "state-owner");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: Some(&UpstreamName::new("repo")),
        })
        .unwrap();

        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    fn an_unbound_directory_derives_no_owner_from_state() {
        // Outside any managed fork (or in one whose remotes could not be read,
        // which dispatch reports the same way), the claims in state say nothing
        // about who is standing here: the identity is the OS user's.
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "OMP_SESSION_ID",
            "USER",
        ]);
        let home = tempfile::tempdir().unwrap();
        state_with_one_claimant(home.path(), "harness-session");
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.remove("OMP_SESSION_ID");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    #[should_panic(expected = "CLAUDE_CODE_SESSION_ID was not captured")]
    fn environment_guard_rejects_mutation_of_an_uncaptured_variable() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_OWNER"]);

        environment.set("CLAUDE_CODE_SESSION_ID", "abc-123");
    }
    #[test]
    fn an_unclaimed_branch_is_taken() {
        let identity = identity("agent-one", OwnerKind::HarnessSession);
        let context = ClaimContext {
            held: Vec::new(),
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(decide(&context), ClaimDecision::Take);
    }

    #[test]
    fn the_same_harness_session_resumes() {
        let claim = held("agent-one", OwnerKind::HarnessSession);
        let identity = identity("agent-one", OwnerKind::HarnessSession);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(
            decide(&context),
            ClaimDecision::Resume { possession: false }
        );
    }

    #[test]
    fn a_different_harness_session_is_refused() {
        let claim = held("agent-one", OwnerKind::HarnessSession);
        let identity = identity("agent-two", OwnerKind::HarnessSession);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseHeld);
    }

    #[test]
    fn two_anonymous_owners_never_match_even_with_equal_strings() {
        let claim = held("terminal-user", OwnerKind::OsUser);
        let identity = identity("terminal-user", OwnerKind::OsUser);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseAnonymous);
    }

    #[test]
    fn possession_of_the_claimed_workspace_resumes_whatever_the_identity() {
        let claim = held("someone-else", OwnerKind::HarnessSession);
        let identity = identity("terminal-user", OwnerKind::OsUser);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: true,
            machine: None,
        };

        assert_eq!(decide(&context), ClaimDecision::Resume { possession: true });
    }

    #[test]
    fn mixed_kinds_with_equal_strings_are_strangers() {
        let claim = held("abc", OwnerKind::HarnessSession);
        let identity = identity("abc", OwnerKind::WorkspaceDerived);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseHeld);
    }

    #[test]
    fn matching_workspace_derived_identities_resume() {
        let claim = held("abc", OwnerKind::WorkspaceDerived);
        let identity = identity("abc", OwnerKind::WorkspaceDerived);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(
            decide(&context),
            ClaimDecision::Resume { possession: false }
        );
    }

    #[test]
    fn an_anonymous_challenger_to_a_harness_claim_is_refused_not_anonymous() {
        let claim = held("abc", OwnerKind::HarnessSession);
        let identity = identity("ubuntu", OwnerKind::OsUser);
        let context = ClaimContext {
            held: vec![&claim],
            identity: &identity,
            in_claimed_workspace: false,
            machine: None,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseHeld);
    }

    #[test]
    fn possession_releases_only_the_claim_of_the_same_owner_name() {
        let claim = held("agent-one", OwnerKind::HarnessSession);

        assert!(possession_permits_release(
            &claim,
            &identity("agent-one", OwnerKind::HarnessSession)
        ));
        assert!(!possession_permits_release(
            &claim,
            &identity("agent-two", OwnerKind::HarnessSession)
        ));
    }

    #[test]
    fn possession_releases_across_identity_kinds_that_name_the_same_owner() {
        // The name is the owner; how each side came by it is not the question a
        // release asks. Two anonymous callers under one OS user name keep
        // releasing each other's claims from inside the directory.
        let claim = held("terminal-user", OwnerKind::OsUser);

        assert!(possession_permits_release(
            &claim,
            &identity("terminal-user", OwnerKind::OsUser)
        ));
        assert!(possession_permits_release(
            &held("agent-one", OwnerKind::HarnessSession),
            &identity("agent-one", OwnerKind::WorkspaceDerived)
        ));
    }

    #[test]
    fn a_ledger_claim_of_another_owner_refuses_on_every_machine() {
        // Given: owner one's claim on the ledger, taken on machine one.
        let theirs = on_ledger("owner-one", "machine-one");
        let caller = identity("owner-two", OwnerKind::HarnessSession);

        // When/Then: owner two is refused by it on machine one and on
        // machine two alike.
        for here in ["machine-one", "machine-two"] {
            let context = shared(here, vec![&theirs], &caller, false);
            assert_eq!(decide(&context), ClaimDecision::RefuseHeld, "on {here}");
            assert_eq!(refusing(&context), Some(&theirs), "on {here}");
        }
    }

    #[test]
    fn a_ledger_claim_resumes_its_owner_on_any_machine() {
        // Given: owner one's claim on the ledger, taken on machine one.
        let mine = on_ledger("owner-one", "machine-one");
        let caller = identity("owner-one", OwnerKind::HarnessSession);

        // When/Then: owner one resumes it by name on either machine.
        for here in ["machine-one", "machine-two"] {
            assert_eq!(
                decide(&shared(here, vec![&mine], &caller, false)),
                ClaimDecision::Resume { possession: false },
                "on {here}"
            );
        }
    }

    #[test]
    fn the_worked_case_of_a_local_claim_and_a_ledger_claim_of_two_owners() {
        // Given: X's local claim on machine A, which an older knives took,
        // and Y's claim on the ledger, taken on machine B; Z owns neither.
        let x_local = held("owner-x", OwnerKind::HarnessSession);
        let y_ledger = on_ledger("owner-y", "machine-b");
        let (x, y, z) = (
            identity("owner-x", OwnerKind::HarnessSession),
            identity("owner-y", OwnerKind::HarnessSession),
            identity("owner-z", OwnerKind::HarnessSession),
        );
        // Machine A weighs both claims; machine B has only the ledger's.
        let on_a = vec![&y_ledger, &x_local];
        let on_b = vec![&y_ledger];

        // X on A is refused by Y's claim: the refusal a `--force --why`
        // seizes, which replaces X's local claim by a ledger claim.
        let x_on_a = shared("machine-a", on_a.clone(), &x, false);
        assert_eq!(decide(&x_on_a), ClaimDecision::RefuseHeld);
        assert_eq!(refusing(&x_on_a), Some(&y_ledger));
        // Y on A is refused by X's local claim.
        let y_on_a = shared("machine-a", on_a.clone(), &y, false);
        assert_eq!(decide(&y_on_a), ClaimDecision::RefuseHeld);
        assert_eq!(refusing(&y_on_a), Some(&x_local));
        // Y on B resumes.
        assert_eq!(
            decide(&shared("machine-b", on_b.clone(), &y, false)),
            ClaimDecision::Resume { possession: false }
        );
        // Z is refused everywhere.
        assert_eq!(
            decide(&shared("machine-a", on_a, &z, false)),
            ClaimDecision::RefuseHeld
        );
        assert_eq!(
            decide(&shared("machine-b", on_b, &z, false)),
            ClaimDecision::RefuseHeld
        );
    }

    #[test]
    fn standing_in_the_workspace_resumes_a_ledger_claim_only_on_the_machine_that_took_it() {
        // Given: owner one's claim on the ledger, taken on machine one, and
        // owner two standing in the branch's workspace.
        let theirs = on_ledger("owner-one", "machine-one");
        let caller = identity("owner-two", OwnerKind::HarnessSession);

        // When/Then: on machine one the workspace is the claim's own, and
        // resumes it; on machine two a workspace at the same path is
        // another directory, and resumes nothing.
        assert_eq!(
            decide(&shared("machine-one", vec![&theirs], &caller, true)),
            ClaimDecision::Resume { possession: true }
        );
        let elsewhere = shared("machine-two", vec![&theirs], &caller, true);
        assert_eq!(decide(&elsewhere), ClaimDecision::RefuseHeld);
        assert_eq!(refusing(&elsewhere), Some(&theirs));
    }

    #[test]
    fn standing_in_the_workspace_resumes_a_local_claim_on_a_fork_that_shares_its_ledger() {
        // Given: another owner's local claim, which counts as this
        // machine's, and the caller standing in its workspace.
        let theirs = held("owner-one", OwnerKind::HarnessSession);
        let caller = identity("owner-two", OwnerKind::HarnessSession);

        // When/Then: it resumes by possession.
        assert_eq!(
            decide(&shared("machine-one", vec![&theirs], &caller, true)),
            ClaimDecision::Resume { possession: true }
        );
    }

    #[test]
    fn a_resume_any_claim_permits_only_by_the_workspace_is_a_resume_by_possession() {
        // Given: the caller's own claim on the ledger, and another owner's
        // local claim, with the caller standing in the workspace.
        let mine = on_ledger("owner-one", "machine-two");
        let theirs = held("owner-two", OwnerKind::HarnessSession);
        let caller = identity("owner-one", OwnerKind::HarnessSession);

        // When/Then: each permits, the local one only by possession.
        assert_eq!(
            decide(&shared("machine-one", vec![&mine, &theirs], &caller, true)),
            ClaimDecision::Resume { possession: true }
        );
        // And: away from the workspace, the local claim refuses.
        assert_eq!(
            decide(&shared("machine-one", vec![&mine, &theirs], &caller, false)),
            ClaimDecision::RefuseHeld
        );
    }

    #[test]
    fn two_anonymous_callers_are_refused_as_such_by_a_ledger_claim() {
        let theirs = Claim {
            kind: OwnerKind::OsUser,
            ..on_ledger("terminal-user", "machine-one")
        };
        let caller = identity("terminal-user", OwnerKind::OsUser);

        assert_eq!(
            decide(&shared("machine-two", vec![&theirs], &caller, false)),
            ClaimDecision::RefuseAnonymous
        );
    }

    #[test]
    fn on_a_fork_that_shares_no_ledger_every_claim_is_this_machines() {
        // A copy of a ledger claim left behind by a registry that named a
        // ledger once is still the one claim the state file holds.
        let left = on_ledger("owner-one", "machine-one");

        assert!(taken_here(&left, None));
        assert!(taken_here(&left, Some("machine-one")));
        assert!(!taken_here(&left, Some("machine-two")));
        assert!(taken_here(
            &held("owner-one", OwnerKind::HarnessSession),
            Some("machine-two")
        ));
    }

    #[test]
    fn a_claim_whose_activity_cannot_be_read_shows_when_it_was_taken_and_no_sighting() {
        let claim = on_ledger("owner-two", "machine-two");
        let now = "2026-01-03T00:00:00Z".parse().unwrap();

        assert_eq!(
            render_shared_claim_context(&claim, "machine-two", None, now),
            "a-repo/feat/alpha is claimed by owner-two (harness-session) on machine machine-two, \
             taken 2026-01-01T00:00:00Z (2d ago): w"
        );
        // Where it can be read, the sighting is today's.
        assert_eq!(
            render_shared_claim_context(
                &claim,
                "machine-two",
                Some(crate::seen::LastSeen::NoneSinceClaim),
                now
            ),
            "a-repo/feat/alpha is claimed by owner-two (harness-session) on machine machine-two, \
             claimed 2d ago, no activity observed since claimed: w"
        );
    }

    #[test]
    fn the_activity_line_names_both_machines_or_the_place_this_config_home_did_not_record() {
        let claim = Claim {
            place: Some("box-two 4026531836".into()),
            ..on_ledger("owner-two", "machine-two")
        };

        assert_eq!(
            unreadable_activity(&claim, "machine-two", "machine-one"),
            "its activity cannot be read from this machine (machine-one): its sightings and its \
             processes are on machine machine-two"
        );
        assert_eq!(
            unreadable_activity(&claim, "machine-two", "machine-two"),
            "its activity cannot be read here: this config home did not record that claim, which \
             was taken in box-two 4026531836"
        );
    }

    #[test]
    fn a_shared_claims_values_reach_the_terminal_escaped() {
        // The owner, the reason, the time, the place and the machine are
        // the text of whichever session took the claim, on whichever
        // machine.
        let claim = Claim {
            why: "fix \u{1b}]0;pwned\u{7}".into(),
            started: "2026-01-01\u{1b}[2J".into(),
            place: Some("box\u{9b}two".into()),
            ..on_ledger("owner\u{1b}[31m", "machine\u{7}two")
        };
        let now = "2026-01-03T00:00:00Z".parse().unwrap();

        let lines = [
            render_shared_claim_context(&claim, "machine\u{7}two", None, now),
            unreadable_activity(&claim, "machine\u{7}two", "machine-one"),
            unreadable_activity(&claim, "machine\u{7}two", "machine\u{7}two"),
        ];

        for line in lines {
            assert!(
                !line.contains(['\u{1b}', '\u{7}', '\u{9b}']),
                "a control character reached the terminal: {line:?}"
            );
        }
    }
}
