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
use crate::ledger_git::{ClaimRecord, ClaimRef, HeldClaim};
use crate::ledger_sweep::{self, Becomes, ClaimOutcome, LedgerFailure, NotApplied, SharedFork};
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
        return if both_anonymous(claim, context.identity) {
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
fn taken_here(claim: &Claim, machine: Option<&str>) -> bool {
    machine.is_none() || claim.ledger_commit.is_none() || claim.machine.as_deref() == machine
}

/// Whether `claim` and `identity` are both anonymous `os-user` names, which
/// cannot be told apart, so that equal names prove nothing between them.
pub fn both_anonymous(claim: &Claim, identity: &Identity) -> bool {
    claim.kind == OwnerKind::OsUser && identity.kind == OwnerKind::OsUser
}

fn permits_by_possession(claim: &Claim, context: &ClaimContext<'_>) -> bool {
    context.in_claimed_workspace && taken_here(claim, context.machine)
}

/// Whether `claim` names `identity` as its holder: the identity match
/// [`decide`] resumes on, which two anonymous `os-user` names never pass.
pub fn names(claim: &Claim, identity: &Identity) -> bool {
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
/// lifecycle notices: `<repo>/<branch> is claimed by` its holder
/// ([`render_holder`]).
///
/// `machine` is `None` on a fork that shares no ledger, and on one that
/// shares its ledger the machine the claim was taken on.
///
/// It is printed to the refused `start`, the refused `finish` and the
/// notice after a resume. The notice after a seizure names the same holder
/// as the one the branch was taken from ([`render_seized`]).
pub fn render_claim_context(
    claim: &Claim,
    machine: Option<&str>,
    last_seen: Option<crate::seen::LastSeen>,
    now: jiff::Timestamp,
) -> String {
    format!(
        "{} is claimed by {}",
        claim.key(),
        render_holder(claim, machine, last_seen, now)
    )
}

/// The line a seize prints of the claim it took, `claim`: what it seized
/// and from whom.
///
/// The holder is named as [`render_claim_context`] names it. The branch is
/// no longer that holder's, so the line never says it is claimed by it.
pub fn render_seized(
    claim: &Claim,
    machine: Option<&str>,
    last_seen: Option<crate::seen::LastSeen>,
    now: jiff::Timestamp,
) -> String {
    format!(
        "seized {} from {}",
        claim.key(),
        render_holder(claim, machine, last_seen, now)
    )
}

/// [`render_seized`] over a claim commit whose record does not read, so
/// that no holder can be named: the claim is named by its ref, `claim`, and
/// the commit this `start` read there.
pub fn render_seized_unread(claim: &ClaimRef, commit: Option<&CommitId>) -> String {
    format!(
        "seized the claim {claim} at {}, whose holder could not be read",
        commit.map_or("", CommitId::as_str)
    )
}

/// The holder of `claim`, as a line about the claim names it: its owner,
/// how that owner was named, the machine it was taken on, when it was
/// taken, its last sighting and its reason.
///
/// `machine` is the machine a claim on a fork that shares its ledger was
/// taken on; on a fork that shares no ledger (`None`) every claim is this
/// machine's, and the line names no machine. `last_seen` is its sighting
/// where its activity can be read here, which it always can on a fork that
/// shares no ledger. Where it cannot (`None`), the holder is named with
/// when the claim was taken instead, and nothing about a sighting: this
/// machine's records of another machine's claim, or of one another config
/// home recorded, are empty for want of looking, and "no activity
/// observed" would say the work is idle.
///
/// The owner name, the machine, the time taken and the reason are the text
/// of whatever session took the claim — another agent of this machine,
/// through `knives start --why`, or one on whichever machine took a ledger
/// claim — and every session reads that record, so each reaches the
/// terminal escaped ([`crate::ledger::inline_human_text`]): a reason holding
/// a terminal control sequence would otherwise drive the terminal of every
/// caller the line is printed to. The age and the sighting are this
/// process's own words about timestamps, and need no escaping.
fn render_holder(
    claim: &Claim,
    machine: Option<&str>,
    last_seen: Option<crate::seen::LastSeen>,
    now: jiff::Timestamp,
) -> String {
    let on_machine = machine.map_or_else(String::new, |machine| {
        format!(" on machine {}", crate::ledger::inline_human_text(machine))
    });
    let when = last_seen.map_or_else(
        || taken_when(claim, now),
        |last_seen| {
            format!(
                "{}, {}",
                claimed_when(claim, now),
                render_last_seen(last_seen, now)
            )
        },
    );
    format!(
        "{} ({}){on_machine}, {when}: {}",
        crate::ledger::inline_human_text(&claim.owner),
        owner_kind_label(claim.kind),
        crate::ledger::inline_human_text(&claim.why),
    )
}

/// When `claim` was taken, by its age: `claimed 3h ago`, or `claimed just
/// now` under a minute ([`crate::ledger::ago`]).
///
/// A time that does not parse, which a record another session wrote can
/// hold, has no age, so the phrase says it cannot be read.
fn claimed_when(claim: &Claim, now: jiff::Timestamp) -> String {
    crate::ledger::ago(&claim.started, now).map_or_else(
        || "claimed at a time that cannot be read".to_owned(),
        |ago| format!("claimed {ago}"),
    )
}

/// When `claim` was taken, by the time its record gives and its age:
/// `taken <time> (3h ago)`, or `taken <time> (just now)`.
///
/// The time is the text of whichever session took the claim, so it goes
/// out escaped. A time that does not parse has no age, so the parentheses
/// say it is not a time that can be read.
fn taken_when(claim: &Claim, now: jiff::Timestamp) -> String {
    format!(
        "taken {} ({})",
        crate::ledger::inline_human_text(&claim.started),
        crate::ledger::ago(&claim.started, now)
            .unwrap_or_else(|| "not a time that can be read".to_owned())
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
/// escaped for the same reason [`render_holder`] escapes them: these rows
/// are printed by the session hook, into the terminal of a caller that
/// wrote none of it. The subject is the branch the state file records,
/// which a ref read from a remote can give a C1 character.
pub fn render_claim_line(
    subject: &str,
    claim: &Claim,
    last_seen: crate::seen::LastSeen,
    now: jiff::Timestamp,
) -> String {
    format!(
        "{} ({}, {}, {}, {}): {}",
        crate::ledger::inline_human_text(subject),
        crate::ledger::inline_human_text(&claim.owner),
        owner_kind_label(claim.kind),
        claimed_when(claim, now),
        render_last_seen(last_seen, now),
        crate::ledger::inline_human_text(&claim.why),
    )
}

/// Renders the observation state without implying a liveness guarantee.
pub fn render_last_seen(last_seen: crate::seen::LastSeen, now: jiff::Timestamp) -> String {
    match last_seen {
        crate::seen::LastSeen::At(timestamp) => crate::ledger::ago(&timestamp.to_string(), now)
            .map_or_else(
                || "not seen within the observation window".to_owned(),
                |ago| format!("last seen {ago}"),
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
/// and Oh My Pi (OMP) each export their session id. OMP exports it as
/// `OMP_SESSION_ID`, or, in other builds, as `PI_SESSION_ID` alone; a shell
/// that has both is named by `OMP_SESSION_ID`.
pub const HARNESS_SESSION_VARIABLES: [&str; 4] = [
    "KNIVES_OWNER",
    "CLAUDE_CODE_SESSION_ID",
    "OMP_SESSION_ID",
    "PI_SESSION_ID",
];

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

    /// The commit of the ledger's claim, as it was read: what a change of
    /// it is leased on.
    pub fn commit(&self) -> Option<&CommitId> {
        self.ledger.as_ref().map(|(commit, _)| commit)
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

/// The last sighting of a claim a write displaced, as this machine can
/// tell it.
#[derive(Debug, Clone, Copy)]
pub enum Sighting<'a> {
    /// Its activity can be read here, and was last seen so.
    Seen(crate::seen::LastSeen),
    /// Its activity cannot be read here: its records are on the machine it
    /// was taken on, which this names.
    Elsewhere(&'a str),
}

impl Sighting<'_> {
    /// The sighting, where its activity can be read here.
    pub const fn last_seen(self) -> Option<crate::seen::LastSeen> {
        match self {
            Self::Seen(last_seen) => Some(last_seen),
            Self::Elsewhere(_) => None,
        }
    }
}

/// How an entry names the last sighting of a claim its write displaced:
/// the sighting's durable token where the claim's activity can be read
/// here, else the machine it was taken on, whose records hold it.
pub fn sighting_provenance(sighting: Sighting<'_>) -> String {
    match sighting {
        Sighting::Seen(last_seen) => format!("last seen {}", last_seen_provenance(last_seen)),
        Sighting::Elsewhere(machine) => format!("on machine {machine}"),
    }
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
        shared.ledger,
        git_dir_of(shared),
        problem_human_text(&failure.cause)
    )
}

/// Why a change of a claim reached nothing: the ledger, its git directory,
/// and `cause`, git's account or this process's ([`unapplied_cause`]).
pub fn unwritten(shared: &SharedFork, cause: &str) -> String {
    format!(
        "the ledger {} could not be written ({}): {}",
        shared.ledger,
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
        NotApplied::MachineRef => format!(
            "git refused this machine's ref refs/knives/{} on origin: {} ({})",
            shared.destination.machine,
            crate::ledger_git::machine_collision_remedy(shared.destination.repository.git_dir()),
            failure.cause
        ),
        NotApplied::Lost { .. } | NotApplied::Failed => failure.cause.clone(),
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
pub fn change_on_ledger<B: Becomes>(
    change: ledger_sweep::ClaimChange<'_, B>,
    scribe: &Scribe,
    draft: &Draft<'_>,
) -> ClaimOutcome<B::Landed> {
    let changed =
        ledger_sweep::change_claim(change, || scribe.record_to(draft).map(|(_, path)| path));
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
    use crate::config::test_support::{EnvironmentGuard, environment_lock, no_harness_session};
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
        let environment = no_harness_session(&["KNIVES_CONFIG_HOME", "KNIVES_OWNER", "USER"]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.set("KNIVES_OWNER", "   ");
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
        let environment = no_harness_session(&["KNIVES_OWNER"]);
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
        let environment = no_harness_session(&["CLAUDE_CODE_SESSION_ID", "USER"]);
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
        let environment = no_harness_session(&["OMP_SESSION_ID", "USER"]);
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
    fn a_pi_session_resolves_as_a_harness_session() {
        // Oh My Pi builds that set no `OMP_SESSION_ID` export the session id
        // as `PI_SESSION_ID`; without it, every session of such a build falls
        // through to the OS user, and they all share one anonymous owner.
        let _lock = environment_lock();
        let environment = no_harness_session(&["PI_SESSION_ID", "USER"]);
        environment.set("PI_SESSION_ID", "01b0-session");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Bound {
            registry: &Registry::default(),
            upstream: None,
        })
        .unwrap();

        assert_eq!(identity.owner, "01b0-session");
        assert_eq!(identity.kind, crate::store::OwnerKind::HarnessSession);
    }

    #[test]
    fn an_omp_session_outranks_a_pi_session() {
        // A shell that carries both is named by `OMP_SESSION_ID`, so an OMP
        // session keeps the owner name its claims were taken under.
        let _lock = environment_lock();
        let environment = no_harness_session(&["OMP_SESSION_ID", "PI_SESSION_ID", "USER"]);
        environment.set("OMP_SESSION_ID", "01a0-session");
        environment.set("PI_SESSION_ID", "01b0-session");
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
        let environment =
            no_harness_session(&["KNIVES_CONFIG_HOME", "CLAUDE_CODE_SESSION_ID", "USER"]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.set("CLAUDE_CODE_SESSION_ID", "   ");
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
    fn a_blank_pi_session_falls_back_to_the_os_user() {
        let _lock = environment_lock();
        let environment = no_harness_session(&["KNIVES_CONFIG_HOME", "PI_SESSION_ID", "USER"]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.set("PI_SESSION_ID", "   ");
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
        let environment = no_harness_session(&["KNIVES_CONFIG_HOME", "USER"]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
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
        let environment = no_harness_session(&["KNIVES_CONFIG_HOME", "USER"]);
        let home = tempfile::tempdir().unwrap();
        state_with_one_claimant(home.path(), "harness-session");
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
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
        let environment = no_harness_session(&["KNIVES_CONFIG_HOME", "USER"]);
        let home = tempfile::tempdir().unwrap();
        state_with_one_claimant(home.path(), "os-user");
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
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
        let environment = no_harness_session(&["KNIVES_CONFIG_HOME", "USER"]);
        let home = tempfile::tempdir().unwrap();
        state_with_one_claimant(home.path(), "harness-session");
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
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
            render_claim_context(&claim, Some("machine-two"), None, now),
            "a-repo/feat/alpha is claimed by owner-two (harness-session) on machine machine-two, \
             taken 2026-01-01T00:00:00Z (2d ago): w"
        );
        // Where it can be read, the sighting is today's.
        assert_eq!(
            render_claim_context(
                &claim,
                Some("machine-two"),
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
            render_claim_context(&claim, Some("machine\u{7}two"), None, now),
            render_seized(&claim, Some("machine\u{7}two"), None, now),
            render_seized(
                &claim,
                None,
                Some(crate::seen::LastSeen::NoneSinceClaim),
                now,
            ),
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

    /// Every line about a claim that says when it was taken: of `local`, on
    /// a fork that shares no ledger, and of `shared`, on the ledger, taken
    /// on machine one, both last seen at `seen`.
    fn every_line(
        local: &Claim,
        shared: &Claim,
        seen: crate::seen::LastSeen,
        now: jiff::Timestamp,
    ) -> [String; 6] {
        [
            render_claim_context(local, None, Some(seen), now),
            render_seized(local, None, Some(seen), now),
            render_claim_context(shared, Some("machine-one"), None, now),
            render_claim_context(shared, Some("machine-one"), Some(seen), now),
            render_seized(shared, Some("machine-one"), None, now),
            render_claim_line("feat/alpha", local, seen, now),
        ]
    }

    #[test]
    fn every_line_says_when_a_claim_was_taken_in_words_that_read_right() {
        // Given: claims taken thirty seconds ago, last seen ten seconds ago,
        // and the same claims with a time taken that does not parse.
        let now = "2026-01-01T00:00:30Z".parse().unwrap();
        let seen = crate::seen::LastSeen::At("2026-01-01T00:00:20Z".parse().unwrap());
        let local = held("owner-one", OwnerKind::HarnessSession);
        let shared = on_ledger("owner-one", "machine-one");
        let local_unparsed = Claim {
            started: "last tuesday".into(),
            ..held("owner-one", OwnerKind::HarnessSession)
        };
        let shared_unparsed = Claim {
            started: "last tuesday".into(),
            ..on_ledger("owner-one", "machine-one")
        };

        // When: each line about each claim is rendered.
        let recent = every_line(&local, &shared, seen, now);
        let unparsed = every_line(&local_unparsed, &shared_unparsed, seen, now);

        // Then: a claim under a minute old reads "just now", never "now
        // ago", as its sighting does.
        assert_eq!(
            recent,
            [
                "a-repo/feat/alpha is claimed by owner-one (harness-session), claimed just now, \
                 last seen just now: w",
                "seized a-repo/feat/alpha from owner-one (harness-session), claimed just now, \
                 last seen just now: w",
                "a-repo/feat/alpha is claimed by owner-one (harness-session) on machine \
                 machine-one, taken 2026-01-01T00:00:00Z (just now): w",
                "a-repo/feat/alpha is claimed by owner-one (harness-session) on machine \
                 machine-one, claimed just now, last seen just now: w",
                "seized a-repo/feat/alpha from owner-one (harness-session) on machine \
                 machine-one, taken 2026-01-01T00:00:00Z (just now): w",
                "feat/alpha (owner-one, harness-session, claimed just now, last seen just now): w",
            ]
        );
        // And: a time that does not parse is said to be one that cannot be
        // read, never "unknown ago".
        assert_eq!(
            unparsed,
            [
                "a-repo/feat/alpha is claimed by owner-one (harness-session), claimed at a time \
                 that cannot be read, last seen just now: w",
                "seized a-repo/feat/alpha from owner-one (harness-session), claimed at a time \
                 that cannot be read, last seen just now: w",
                "a-repo/feat/alpha is claimed by owner-one (harness-session) on machine \
                 machine-one, taken last tuesday (not a time that can be read): w",
                "a-repo/feat/alpha is claimed by owner-one (harness-session) on machine \
                 machine-one, claimed at a time that cannot be read, last seen just now: w",
                "seized a-repo/feat/alpha from owner-one (harness-session) on machine \
                 machine-one, taken last tuesday (not a time that can be read): w",
                "feat/alpha (owner-one, harness-session, claimed at a time that cannot be read, \
                 last seen just now): w",
            ]
        );
    }

    #[test]
    fn a_seize_on_a_fork_that_shares_no_ledger_names_what_it_took_and_from_whom() {
        // Given: owner one's claim, taken three hours ago, last seen an hour
        // ago.
        let claim = held("owner-one", OwnerKind::HarnessSession);
        let now = "2026-01-01T03:00:00Z".parse().unwrap();
        let seen = crate::seen::LastSeen::At("2026-01-01T02:00:00Z".parse().unwrap());

        // When: a seize names the claim it took.
        let line = render_seized(&claim, None, Some(seen), now);

        // Then: one sentence, which never says the branch is still claimed
        // by its former holder.
        assert_eq!(
            line,
            "seized a-repo/feat/alpha from owner-one (harness-session), claimed 3h ago, last \
             seen 1h ago: w"
        );
    }

    #[test]
    fn a_seize_on_a_fork_that_shares_its_ledger_names_the_holder_and_its_machine() {
        // Given: owner one's claim on the ledger, taken on machine one three
        // hours ago.
        let claim = on_ledger("owner-one", "machine-one");
        let now = "2026-01-01T03:00:00Z".parse().unwrap();

        // When: a seize names the claim it took, where its activity cannot
        // be read here, and where it can.
        let elsewhere = render_seized(&claim, Some("machine-one"), None, now);
        let here = render_seized(
            &claim,
            Some("machine-one"),
            Some(crate::seen::LastSeen::NoneSinceClaim),
            now,
        );

        // Then: what it seized, from whom, on which machine, and when that
        // claim was taken or last seen.
        assert_eq!(
            elsewhere,
            "seized a-repo/feat/alpha from owner-one (harness-session) on machine machine-one, \
             taken 2026-01-01T00:00:00Z (3h ago): w"
        );
        assert_eq!(
            here,
            "seized a-repo/feat/alpha from owner-one (harness-session) on machine machine-one, \
             claimed 3h ago, no activity observed since claimed: w"
        );
    }

    #[test]
    fn a_seize_of_a_claim_whose_holder_cannot_be_read_names_its_ref_and_commit() {
        let _lock = environment_lock();
        // Given: the claim ref of a branch, holding a commit whose record
        // does not read.
        let claim = ClaimRef::new(
            &UpstreamName::new("acme/demo"),
            &BranchName::new("feat/alpha"),
        )
        .unwrap();
        let commit = CommitId::new("0123456789abcdef0123456789abcdef01234567");

        // When: a seize names the claim it took.
        let line = render_seized_unread(&claim, Some(&commit));

        // Then: it names the claim by its ref and commit.
        assert_eq!(
            line,
            "seized the claim refs/knives-claims/acme/demo/feat%2Falpha at \
             0123456789abcdef0123456789abcdef01234567, whose holder could not be read"
        );
    }
}
