//! The claims notice: who holds which branch of a managed fork, told once per
//! session and afterwards only as it changes.
//!
//! The session keeps the roster it was told ([`HeldRoster`]) and how each
//! claim was last shown to it. The first notice in a session lists every
//! claim with its reason cut to one line; a later one lists only the claims
//! taken, changed or released since; and the claim on the branch of the
//! workspace a call touched is given whole whenever the session was not
//! last shown it whole as it is now. A roster the session already holds is
//! said again only as one line, the first time a call touches another
//! workspace of the fork. What the session was told rests on the block that
//! told it, where the harness reports the claims notices in the context: once
//! that block is gone, the roster is told whole again
//! ([`crate::hook::state::SessionState::reconcile`]).

use std::collections::BTreeMap;

use crate::commands::claim::{owner_kind_label, render_claim_line};
use crate::commands::wip::workspace_for;
use crate::config::GuidanceRoot;
use crate::hook::guidance::{FNV_OFFSET_BASIS, envelope_nonce, fnv1a, key_attribute, key_named};
use crate::hook::state::{Given, HeldRoster};
use crate::ids::UpstreamName;
use crate::jj::WorkspaceActivity;
use crate::seen::{self, Seen};
use crate::store::Claim;

/// The longest a reason runs in a one-line claim row, in characters.
pub const REASON_CHARS: usize = 80;

/// The claims of the fork whose claims are kept under `repo`, by branch.
pub fn roster<'a>(claims: &'a [Claim], repo: &UpstreamName) -> Vec<&'a Claim> {
    let mut roster = claims
        .iter()
        .filter(|claim| claim.repo == repo.as_str())
        .collect::<Vec<_>>();
    roster.sort_unstable_by(|left, right| left.branch.cmp(&right.branch));
    roster
}

/// The fields of a claim a notice shows besides its branch, in the order
/// [`claim_fingerprint`] and [`roster_digest`] hash them.
const fn fields(claim: &Claim) -> [&str; 4] {
    [
        claim.owner.as_str(),
        owner_kind_label(claim.kind),
        claim.started.as_str(),
        claim.why.as_str(),
    ]
}

fn hash_fields(mut hash: u64, fields: &[&str]) -> u64 {
    for (index, field) in fields.iter().enumerate() {
        if index != 0 {
            hash = fnv1a(hash, b"\x1f");
        }
        hash = fnv1a(hash, field.as_bytes());
    }
    hash
}

/// What a notice shows of `claim` besides its branch, as one key: the claim
/// on a branch whose key moved is a changed claim.
pub fn claim_fingerprint(claim: &Claim) -> String {
    format!("{:016x}", hash_fields(FNV_OFFSET_BASIS, &fields(claim)))
}

/// The key a session remembers how the claim on a branch was last shown by:
/// the fork its claims are kept under, and the branch.
pub fn branch_key(claim: &Claim) -> String {
    format!("{}\x1f{}", claim.repo, claim.branch)
}

/// The time-invariant digest of a fork's [`roster`]: the name its claims are
/// kept under and each claim's branch and shown fields. Elapsed-time words
/// are not in it, so a roster keeps its digest as it ages.
pub fn roster_digest(repo: &UpstreamName, roster: &[&Claim]) -> String {
    let mut hash = fnv1a(FNV_OFFSET_BASIS, repo.as_str().as_bytes());
    for claim in roster {
        hash = fnv1a(hash, b"\x1e");
        let [owner, kind, started, why] = fields(claim);
        hash = hash_fields(hash, &[claim.branch.as_str(), owner, kind, started, why]);
    }
    format!("{hash:016x}")
}

/// The roster the session holds once it was told `roster`, whose digest is
/// `digest`.
pub fn held(roster: &[&Claim], digest: &str) -> HeldRoster {
    HeldRoster {
        digest: digest.to_owned(),
        claims: roster
            .iter()
            .map(|claim| (claim.branch.clone(), claim_fingerprint(claim)))
            .collect(),
    }
}

/// What a claims notice says, decided against what the session holds.
#[derive(Debug, PartialEq, Eq)]
pub struct NoticePlan<'a> {
    /// The digest of the roster the notice tells ([`roster_digest`]).
    pub digest: String,
    pub form: Form<'a>,
    /// The claims on the branch of the workspace the call touched
    /// ([`Workspace`]), each with whether to give it whole: it was not last
    /// told to the session as fully as a notice can, as it is now.
    pub here: Vec<(&'a Claim, bool)>,
    /// The branches read checked out in that workspace that hold no claim.
    pub unclaimed: Vec<String>,
}

/// The workspace a call touched: the name of its directory, and the branches
/// its checkout has checked out ([`crate::bind::checked_out_branches`]).
#[derive(Debug, Clone, Copy)]
pub struct Workspace<'a> {
    pub name: Option<&'a str>,
    pub branches: &'a [String],
}

impl Workspace<'_> {
    /// Whether `claim` is on this workspace's branch: a branch read checked
    /// out here, or the branch the directory is named for, as `knives start`
    /// names a workspace ([`workspace_for`]). Either counts: a checkout made
    /// by hand is named as anyone liked, and the branch at a jj checkout's
    /// detached HEAD is its working copy's parent, which for a workspace
    /// `knives start` made for a new branch is the base, where `main` is.
    /// A claim told too often is the safe side: `workspace_for` flattens `/`
    /// to `-`, so `feat/a-b` and `feat-a/b` share a name and are both here.
    fn holds(&self, claim: &Claim) -> bool {
        self.branches.contains(&claim.branch)
            || self
                .name
                .is_some_and(|name| workspace_for(&claim.branch) == name)
    }
}

/// What a claim row's sighting and age are read from, and when.
#[derive(Debug)]
pub struct Sightings<'a> {
    pub seen: &'a Seen,
    pub now: jiff::Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Form<'a> {
    /// The session holds no roster of this fork: every claim not `here`.
    Roster(Vec<&'a Claim>),
    /// What changed since the roster the session holds, whose digest is
    /// `since`; claims `here` are not repeated in it.
    Changes {
        since: String,
        claimed: Vec<&'a Claim>,
        changed: Vec<&'a Claim>,
        released: Vec<String>,
    },
    /// The session holds this roster already.
    Unchanged,
}

/// What the session holds that a notice is planned against.
pub struct Holding<'a, F: Fn(&Claim) -> bool> {
    /// The roster of this fork the session holds.
    pub roster: Option<&'a HeldRoster>,
    /// Whether a notice of this digest reached the session for this workspace.
    pub noticed: bool,
    /// Whether the claim was last told to the session as fully as a notice
    /// can (whole, or cut only to fit the cap), as it is now.
    pub had_whole: F,
}

impl<F: Fn(&Claim) -> bool> std::fmt::Debug for Holding<'_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Holding")
            .field("roster", &self.roster)
            .field("noticed", &self.noticed)
            .finish_non_exhaustive()
    }
}

/// The notice for a call that touched `workspace` of a fork whose
/// [`roster`] has the digest `digest`; `None`: nothing to tell.
///
/// The claims "here" are those on the workspace's branch
/// ([`Workspace::holds`]).
pub fn plan<'a, F: Fn(&Claim) -> bool>(
    roster: &[&'a Claim],
    digest: &str,
    workspace: &Workspace<'_>,
    holding: &Holding<'_, F>,
) -> Option<NoticePlan<'a>> {
    let here = roster
        .iter()
        .copied()
        .filter(|claim| workspace.holds(claim))
        .map(|claim| (claim, !(holding.had_whole)(claim)))
        .collect::<Vec<_>>();
    let unclaimed = workspace
        .branches
        .iter()
        .filter(|branch| !roster.iter().any(|claim| &claim.branch == *branch))
        .cloned()
        .collect::<Vec<_>>();
    let elsewhere = |claim: &&Claim| !here.iter().any(|(held, _)| std::ptr::eq(*held, *claim));
    let Some(held) = holding.roster else {
        let others = roster.iter().copied().filter(elsewhere).collect();
        return Some(NoticePlan {
            digest: digest.to_owned(),
            form: Form::Roster(others),
            here,
            unclaimed,
        });
    };
    if held.digest != digest {
        let claimed = roster
            .iter()
            .copied()
            .filter(elsewhere)
            .filter(|claim| !held.claims.contains_key(&claim.branch))
            .collect::<Vec<_>>();
        let changed = roster
            .iter()
            .copied()
            .filter(elsewhere)
            .filter(|claim| {
                held.claims
                    .get(&claim.branch)
                    .is_some_and(|fingerprint| *fingerprint != claim_fingerprint(claim))
            })
            .collect::<Vec<_>>();
        let released = held
            .claims
            .keys()
            .filter(|branch| !roster.iter().any(|claim| &claim.branch == *branch))
            .cloned()
            .collect::<Vec<_>>();
        let any_here = here.iter().any(|(_, whole)| *whole);
        if !claimed.is_empty() || !changed.is_empty() || !released.is_empty() || any_here {
            return Some(NoticePlan {
                digest: digest.to_owned(),
                form: Form::Changes {
                    since: held.digest.clone(),
                    claimed,
                    changed,
                    released,
                },
                here,
                unclaimed,
            });
        }
    }
    if holding.noticed && here.iter().all(|(_, whole)| !*whole) {
        return None;
    }
    Some(NoticePlan {
        digest: digest.to_owned(),
        form: Form::Unchanged,
        here,
        unclaimed,
    })
}

/// Each claim the notice `plan` shows, and how much of it was given.
///
/// That is what the session was last shown of it once the notice is
/// delivered. `cut` says, per claim here, whether the notice cut its reason
/// or folded it into the summary to fit the cap ([`Rendered::cut`]): such a
/// claim was given as fully as the cap allows, never whole.
pub fn shown<'a>(plan: &NoticePlan<'a>, cut: &[bool]) -> Vec<(&'a Claim, Given)> {
    let rows = |claims: &[&'a Claim]| {
        claims
            .iter()
            .map(|claim| (*claim, Given::Line))
            .collect::<Vec<_>>()
    };
    let mut shown = match &plan.form {
        Form::Roster(others) => rows(others),
        Form::Changes {
            claimed, changed, ..
        } => rows(claimed).into_iter().chain(rows(changed)).collect(),
        Form::Unchanged => Vec::new(),
    };
    // A Changes notice gives only the claims here it gives whole
    // ([`notice_lines`]); every other form gives them all.
    let changes = matches!(plan.form, Form::Changes { .. });
    shown.extend(
        plan.here
            .iter()
            .enumerate()
            .filter(|(_, (_, whole))| !changes || *whole)
            .map(|(index, (claim, whole))| {
                let given = match (*whole, cut.get(index).copied().unwrap_or(false)) {
                    (false, _) => Given::Line,
                    (true, true) => Given::Fitted,
                    (true, false) => Given::Whole,
                };
                (*claim, given)
            }),
    );
    shown
}

/// One line of a notice's always-given part.
enum Line {
    Text(String),
    /// The claim `plan.here[index]`: the line up to its reason, and the
    /// reason, which the cap may cut ([`format_notice`]).
    Here {
        index: usize,
        lead: String,
        reason: String,
    },
}

impl Line {
    fn chars(&self) -> usize {
        match self {
            Self::Text(text) => text.chars().count(),
            Self::Here { lead, reason, .. } => lead.chars().count() + reason.chars().count(),
        }
    }

    fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Here { lead, reason, .. } => lead + &reason,
        }
    }
}

/// The lines of a notice: those always given (the header, then the claim on
/// the touched workspace's branch), the claim rows the cap may leave out,
/// and the usage lines that close a roster.
struct Lines {
    head: Vec<Line>,
    rows: Vec<String>,
    tail: Vec<String>,
}

/// The line of the claim `plan.here[index]`, given whole or cut to a line:
/// the line up to its reason, and the reason, apart.
fn here_line(index: usize, claim: &Claim, whole: bool, sightings: &Sightings<'_>) -> Line {
    // The row with no reason is the row up to its reason: a row ends with
    // its reason.
    let full = claim_line(claim, whole, sightings);
    let bare = claim_line(
        &Claim {
            why: String::new(),
            ..claim.clone()
        },
        whole,
        sightings,
    );
    let (lead, reason) = full.strip_prefix(&bare).map_or_else(
        || (full.clone(), String::new()),
        |reason| (bare.clone(), reason.to_owned()),
    );
    Line::Here {
        index,
        lead: format!("This workspace's branch is claimed: {lead}"),
        reason,
    }
}

/// The lines of the notice `plan` describes, for the workspace `root`.
fn notice_lines(root: &std::path::Path, plan: &NoticePlan<'_>, sightings: &Sightings<'_>) -> Lines {
    let digest = &plan.digest;
    let line = |claim: &Claim, whole: bool| claim_line(claim, whole, sightings);
    let here = |filter: &dyn Fn(bool) -> bool| {
        plan.here
            .iter()
            .enumerate()
            .filter(|(_, (_, whole))| filter(*whole))
            .map(|(index, (claim, whole))| here_line(index, claim, *whole, sightings))
            .collect::<Vec<_>>()
    };
    let text = |text: String| Line::Text(text);
    let mut head = Vec::new();
    let mut rows = Vec::new();
    let mut tail = Vec::new();
    match &plan.form {
        Form::Roster(others) => {
            head.push(text(format!(
                "{} is a fork managed by knives, and another agent may be working in it.",
                root.display()
            )));
            head.extend(here(&|_| true));
            head.push(text(match (others.len(), plan.here.is_empty()) {
                (0, true) => "No branch is claimed here right now.".to_owned(),
                (0, false) => "No other branch is claimed here right now.".to_owned(),
                (count, true) => {
                    format!("Branches claimed here ({count}), each reason cut to one line:")
                }
                (count, false) => {
                    format!("Other branches claimed here ({count}), each reason cut to one line:")
                }
            }));
            rows.extend(
                others
                    .iter()
                    .map(|claim| format!("- {}", line(claim, false))),
            );
            tail.extend(USAGE.iter().map(|line| (*line).to_owned()));
        }
        Form::Changes {
            since,
            claimed,
            changed,
            released,
        } => {
            head.push(text(format!(
                "The claims on this fork changed since the notice with digest {since}; only the \
                 changes follow."
            )));
            // A claim here the session had whole and that did not change is
            // no change.
            head.extend(here(&|whole| whole));
            rows.extend(
                claimed
                    .iter()
                    .map(|claim| format!("- newly claimed: {}", line(claim, false))),
            );
            rows.extend(
                changed
                    .iter()
                    .map(|claim| format!("- claim changed: {}", line(claim, false))),
            );
            rows.extend(
                released.iter().map(|branch| {
                    format!("- released: {}", crate::ledger::inline_human_text(branch))
                }),
            );
        }
        Form::Unchanged => {
            // That no claim is here is said only of a branch read checked
            // out here: one neither checked out nor its directory's name may
            // be claimed all the same, and the roster above names every claim.
            head.push(text(format!(
                "{}: the claims on this fork are unchanged since the notice with digest {digest}.",
                root.display()
            )));
            if plan.here.is_empty() && !plan.unclaimed.is_empty() {
                head.push(text(format!(
                    "No claim is on the branch checked out here: {}.",
                    plan.unclaimed
                        .iter()
                        .map(|branch| crate::ledger::inline_human_text(branch))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            head.extend(here(&|_| true));
        }
    }
    Lines { head, rows, tail }
}

/// The most characters a notice runs to.
///
/// Claude Code takes a hook's `additionalContext` whole only up to 10,000
/// characters, and above that shows the model a file path and the first
/// 2,000; this leaves room for the envelope's JSON. Every harness gets the
/// same notice.
pub const NOTICE_CHARS: usize = 9_500;

/// The least room a notice is given beside guidance in one answer.
///
/// With less, the guidance waits for the next call (Claude Code's path in
/// [`crate::commands::hook`]). Every step [`format_notice_within`] takes fits
/// a notice within this much.
pub const NOTICE_FLOOR_CHARS: usize = 2_000;

/// How to work in a managed fork, said with the full roster.
const USAGE: [&str; 4] = [
    "Use knives rather than jj or git directly here: `knives status` for the state of",
    "every branch, `knives start <branch>` to take a branch and get your own workspace,",
    "`knives finish <branch>` as soon as your active work there stops — the branch,",
    "its bookmark, and any open pull request all survive the release.",
];

/// A notice as written, and, per claim here (`plan.here`), whether its reason
/// was cut to fit the cap: such a claim was not given whole ([`shown`]).
#[derive(Debug)]
pub struct Rendered {
    pub text: String,
    pub cut: Vec<bool>,
}

/// The notice `plan` describes for the workspace `repo`, within
/// [`NOTICE_CHARS`] ([`format_notice_within`]).
pub fn format_notice(
    repo: &GuidanceRoot,
    plan: &NoticePlan<'_>,
    sightings: &Sightings<'_>,
    nonce: &str,
) -> Rendered {
    format_notice_within(
        repo,
        plan,
        sightings,
        Bounds {
            nonce,
            cap: NOTICE_CHARS,
        },
    )
}

/// The envelope a notice goes in, and the most characters it runs to.
#[derive(Debug, Clone, Copy)]
pub struct Bounds<'a> {
    /// The envelope's nonce: the call's, which its guidance shares
    /// ([`crate::hook::state::Envelope`]).
    pub nonce: &'a str,
    /// No less than [`NOTICE_FLOOR_CHARS`].
    pub cap: usize,
}

/// The notice `plan` describes for the workspace `repo`, within `bounds`.
///
/// `repo` is the workspace's root and the name the registry gives its fork.
/// Rows that do not fit are left out and counted, and when the lines always
/// given are too long alone, the reason of a claim here is cut, the last one
/// first, and says how much of it is shown.
#[allow(
    clippy::too_many_lines,
    reason = "the cap is met by four ordered steps that share one measure of the notice"
)]
pub fn format_notice_within(
    repo: &GuidanceRoot,
    plan: &NoticePlan<'_>,
    sightings: &Sightings<'_>,
    bounds: Bounds<'_>,
) -> Rendered {
    let Bounds { nonce, cap } = bounds;
    let Lines {
        mut head,
        rows,
        tail,
    } = notice_lines(&repo.root, plan, sightings);
    let open = format!(
        "\n\n<knives-notice-{nonce} repo=\"{}\" digest=\"{}\">",
        key_attribute(&repo.name),
        plan.digest
    );
    let close = format!("</knives-notice-{nonce}>");
    let key = crate::ledger::inline_human_text(&key_named(&repo.name, "…"));
    let more = |left: usize| {
        format!(
            "… and {left} more not shown here: `knives status {key} --no-landed` lists every \
             claim."
        )
    };
    // Each line costs its characters and the newline before it.
    let cost = |line: &String| line.chars().count() + 1;
    let total = |head: &[Line], kept: usize| {
        open.chars().count()
            + head.iter().map(|line| line.chars() + 1).sum::<usize>()
            + rows.iter().take(kept).map(cost).sum::<usize>()
            + if kept < rows.len() {
                cost(&more(rows.len() - kept))
            } else {
                0
            }
            + tail.iter().map(cost).sum::<usize>()
            + close.chars().count()
            + 1
    };
    // 1. Leave rows out, the last first.
    let mut kept = rows.len();
    while total(&head, kept) > cap && kept > 0 {
        kept -= 1;
    }
    // 2. Cut the reasons of the claims here, the last first.
    let mut cut = vec![false; plan.here.len()];
    for position in (0..head.len()).rev() {
        let over = total(&head, kept).saturating_sub(cap);
        if over == 0 {
            break;
        }
        let Some(Line::Here { index, reason, .. }) = head.get_mut(position) else {
            continue;
        };
        let length = reason.chars().count();
        // The marker as long as it can be, so the reason fits with it.
        let marker = |shown: usize| format!("… (reason cut: {shown} of {length} characters)");
        let room = length
            .saturating_sub(over)
            .saturating_sub(marker(length).chars().count());
        let shortened = reason.chars().take(room).collect::<String>() + &marker(room);
        if shortened.chars().count() < length {
            *reason = shortened;
            if let Some(flag) = cut.get_mut(*index) {
                *flag = true;
            }
        }
    }
    // 3. Fold the claims here that still do not fit, the last first, into one
    //    line naming their branches as far as it fits.
    let mut folded = Vec::new();
    while total(&head, kept) > cap {
        let Some(position) = head
            .iter()
            .rposition(|line| matches!(line, Line::Here { .. }))
        else {
            break;
        };
        if let Some(Line::Text(_)) = head.get(position + 1).filter(|_| !folded.is_empty()) {
            head.remove(position + 1);
        }
        if let Line::Here { index, .. } = head.remove(position) {
            if let Some(flag) = cut.get_mut(index) {
                *flag = true;
            }
            if let Some((claim, _)) = plan.here.get(index) {
                folded.insert(0, crate::ledger::inline_human_text(&claim.branch));
            }
        }
        let lead = format!("… and {} more claims here, cut to fit: ", folded.len());
        head.insert(position, Line::Text(lead.clone()));
        let room = cap.saturating_sub(total(&head, kept));
        let list = folded.join(", ");
        let named = if list.chars().count() <= room {
            list
        } else {
            list.chars()
                .take(room.saturating_sub(1))
                .collect::<String>()
                + "…"
        };
        if let Some(Line::Text(summary)) = head.get_mut(position) {
            *summary = lead + &named;
        }
    }
    // 4. Cut the longest line always given, as a last resort: a header that
    //    names a long path or many branches.
    while total(&head, kept) > cap {
        let over = total(&head, kept) - cap;
        let Some(Line::Text(longest)) = head
            .iter_mut()
            .filter(|line| matches!(line, Line::Text(_)))
            .max_by_key(|line| line.chars())
        else {
            break;
        };
        let length = longest.chars().count();
        if length <= 1 {
            break;
        }
        *longest = longest
            .chars()
            .take(length.saturating_sub(over + 1))
            .collect::<String>()
            + "…";
    }
    let left = (kept < rows.len()).then(|| more(rows.len() - kept));
    let text = std::iter::once(open)
        .chain(head.into_iter().map(Line::into_text))
        .chain(rows.into_iter().take(kept))
        .chain(left)
        .chain(tail)
        .chain([close])
        .collect::<Vec<_>>()
        .join("\n");
    Rendered { text, cut }
}

/// The notice in place of the claim roster when knives cannot read the claims:
/// the agent must not take the silence as "nobody holds this branch".
pub fn format_refusal(repo_name: &str, refusal: &str) -> String {
    let nonce = envelope_nonce();
    [
        String::new(),
        String::new(),
        format!(
            "<knives-notice-{nonce} repo=\"{}\">",
            key_attribute(repo_name)
        ),
        "knives cannot read who has claimed which branch of this fork, so do not assume a \
         branch is free:"
            .to_owned(),
        refusal.to_owned(),
        format!("</knives-notice-{nonce}>"),
    ]
    .join("\n")
}

/// The reason `why` as one line of at most [`REASON_CHARS`] characters,
/// ending in "…" when anything was cut.
pub fn one_line(why: &str) -> String {
    let mut lines = why.lines();
    let first = lines.next().unwrap_or_default();
    let mut characters = first.chars();
    let kept = characters.by_ref().take(REASON_CHARS).collect::<String>();
    if characters.next().is_some() || lines.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}

/// One claim row: its reason whole, or cut to one line ([`one_line`]).
///
/// Hooks never open a jj repository or walk operations — identity is read by
/// git — so an empty operation stream is marked window-exhausted rather than
/// claiming no activity.
pub fn claim_line(claim: &Claim, whole: bool, sightings: &Sightings<'_>) -> String {
    let activity = WorkspaceActivity {
        moves: BTreeMap::new(),
        horizon: Some(sightings.now),
    };
    let last_seen = seen::last_seen(claim, &activity, sightings.seen);
    if whole {
        return render_claim_line(&claim.branch, claim, last_seen, sightings.now);
    }
    let cut = Claim {
        why: one_line(&claim.why),
        ..claim.clone()
    };
    render_claim_line(&cut.branch, &cut, last_seen, sightings.now)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "tests index known non-empty fixture results"
    )]

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::Rendered;
    use super::{
        Bounds, Form, Holding, NOTICE_CHARS, NOTICE_FLOOR_CHARS, NoticePlan, REASON_CHARS,
        Sightings, Workspace, claim_fingerprint, claim_line, format_notice, format_notice_within,
        held, one_line, plan, roster, roster_digest, shown,
    };
    use crate::config::GuidanceRoot;
    use crate::hook::guidance::KEY_CHARS;
    use crate::hook::state::{Given, HeldRoster};
    use crate::ids::UpstreamName;
    use crate::seen::Seen;
    use crate::store::{Claim, OwnerKind};

    /// A workspace in a directory named `name`, whose checked-out branch
    /// was not read.
    const fn named(name: &str) -> Workspace<'_> {
        Workspace {
            name: Some(name),
            branches: &[],
        }
    }

    fn claim(branch: &str, owner: &str, why: &str) -> Claim {
        Claim {
            repo: "beta".to_owned(),
            branch: branch.to_owned(),
            owner: owner.to_owned(),
            kind: OwnerKind::HarnessSession,
            why: why.to_owned(),
            started: "2026-01-01T00:00:00Z".to_owned(),
            files: Vec::new(),
            place: None,
            machine: None,
            ledger_commit: None,
        }
    }

    const fn nothing_whole(_: &Claim) -> bool {
        false
    }

    fn holding(roster: Option<&HeldRoster>, noticed: bool) -> Holding<'_, fn(&Claim) -> bool> {
        Holding {
            roster,
            noticed,
            had_whole: nothing_whole,
        }
    }

    fn at(time: &str) -> jiff::Timestamp {
        time.parse().expect("valid timestamp")
    }

    fn text(root: &str, planned: &NoticePlan<'_>) -> String {
        rendered(root, planned).text
    }

    fn rendered(root: &str, planned: &NoticePlan<'_>) -> Rendered {
        format_notice(
            &GuidanceRoot {
                name: "beta".to_owned(),
                root: PathBuf::from(root),
            },
            planned,
            &Sightings {
                seen: &Seen::default(),
                now: at("2026-01-03T00:00:00Z"),
            },
            "0123",
        )
    }

    #[test]
    fn the_digest_ignores_rolling_ages_and_order_and_moves_with_any_claim() {
        // Rendering rolls elapsed-time words; the digest is a function of the
        // stored claims alone, so an ageing roster is not told again.
        let beta = UpstreamName::new("beta");
        let claims = vec![claim("feat/first", "agent-one", "porting")];
        let seen = Seen::default();
        let row = |now| {
            claim_line(
                &claims[0],
                true,
                &Sightings {
                    seen: &seen,
                    now: at(now),
                },
            )
        };
        assert_ne!(
            row("2026-01-01T01:00:00Z"),
            row("2026-01-01T03:00:00Z"),
            "ages must roll for this test to bite"
        );
        let digest = roster_digest(&beta, &roster(&claims, &beta));
        assert_eq!(digest, roster_digest(&beta, &roster(&claims, &beta)));

        let mut grown = claims.clone();
        grown.push(Claim {
            kind: OwnerKind::WorkspaceDerived,
            started: "2026-01-02T00:00:00Z".to_owned(),
            ..claim("feat/second", "agent-two", "reviewing")
        });
        // Pinned: a change to the hash would make every held roster read as
        // changed, and every session be told its whole roster again.
        let expected = "23bb3f9be9b9c19d";
        assert_eq!(roster_digest(&beta, &roster(&grown, &beta)), expected);
        let mut reordered = grown.clone();
        reordered.reverse();
        assert_eq!(roster_digest(&beta, &roster(&reordered, &beta)), expected);
        assert_ne!(digest, expected);
        let mut reworded = grown;
        reworded[0].why = "porting, round two".to_owned();
        assert_ne!(roster_digest(&beta, &roster(&reworded, &beta)), expected);
    }

    #[test]
    fn a_claim_row_carries_owner_kind_claimed_age_and_last_seen() {
        // Removing any claim provenance cell would make a live notice unable to
        // distinguish both who took work and what evidence supports it.
        let claims = [
            claim("feat/x", "agent-a", "porting"),
            claim("feat/y", "agent-b", "waiting"),
        ];
        let seen = Seen {
            owners: BTreeMap::from([(
                OwnerKind::HarnessSession,
                BTreeMap::from([("agent-a".to_owned(), "2026-01-02T00:00:00Z".to_owned())]),
            )]),
            workspaces: BTreeMap::new(),
        };
        let sightings = Sightings {
            seen: &seen,
            now: at("2026-01-03T00:00:00Z"),
        };

        let rows = claims
            .iter()
            .map(|claim| claim_line(claim, false, &sightings))
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            [
                "feat/x (agent-a, harness-session, claimed 2d ago, last seen 1d ago): porting",
                "feat/y (agent-b, harness-session, claimed 2d ago, not seen within the \
                 observation window): waiting",
            ]
        );
    }

    #[test]
    fn a_reason_is_cut_to_one_line() {
        let long = "x".repeat(REASON_CHARS + 5);
        assert_eq!(one_line("short"), "short");
        assert_eq!(one_line(""), "");
        assert_eq!(one_line("first\nsecond"), "first…");
        assert_eq!(one_line(&long), format!("{}…", "x".repeat(REASON_CHARS)));
        assert_eq!(
            one_line(&"é".repeat(REASON_CHARS)),
            "é".repeat(REASON_CHARS)
        );
    }

    #[test]
    fn a_fork_without_claims_says_so() {
        let beta = UpstreamName::new("beta");
        let roster = roster(&[], &beta);
        let digest = roster_digest(&beta, &roster);
        let planned =
            plan(&roster, &digest, &named("main"), &holding(None, false)).expect("a notice");

        let text = text("/w/main", &planned);

        assert!(
            text.contains("\nNo branch is claimed here right now.\n"),
            "{text}"
        );
        assert!(text.contains(&format!("digest=\"{digest}\"")), "{text}");
    }

    #[test]
    fn the_first_notice_lists_every_claim_cut_and_the_workspaces_whole() {
        // Given: two claims, one on the branch of the touched workspace, each
        // with a reason longer than a row.
        let long = "y".repeat(200);
        let claims = [
            claim("feat/here", "agent-a", &long),
            claim("feat/there", "agent-b", &long),
        ];
        let beta = UpstreamName::new("beta");
        let roster = roster(&claims, &beta);

        // When: the session holds no roster.
        let planned =
            plan(&roster, "d1", &named("feat-here"), &holding(None, false)).expect("a notice");

        // Then: the touched branch's claim is whole, the other cut.
        assert_eq!(
            planned,
            NoticePlan {
                digest: "d1".to_owned(),
                form: Form::Roster(vec![&claims[1]]),
                here: vec![(&claims[0], true)],
                unclaimed: Vec::new(),
            }
        );
        let text = text("/w/feat-here", &planned);
        let meta = "harness-session, claimed 2d ago, not seen within the observation window";
        assert!(
            text.contains(&format!(
                "This workspace's branch is claimed: feat/here (agent-a, {meta}): {long}\n"
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "- feat/there (agent-b, {meta}): {}…\n",
                "y".repeat(REASON_CHARS)
            )),
            "{text}"
        );
        assert!(
            text.contains("Other branches claimed here (1), each reason cut to one line:"),
            "{text}"
        );
        assert!(text.contains("`knives start <branch>`"), "{text}");
    }

    #[test]
    fn a_changed_roster_tells_only_the_changes() {
        // Given: the session holds a roster of three claims.
        let beta = UpstreamName::new("beta");
        let before = [
            claim("feat/gone", "agent-c", "done"),
            claim("feat/kept", "agent-a", "same"),
            claim("feat/moved", "agent-b", "old reason"),
        ];
        let held_roster = held(&roster(&before, &beta), "old");

        // When: one claim's reason changed, one was released, one was taken.
        let after = [
            claim("feat/kept", "agent-a", "same"),
            claim("feat/moved", "agent-b", "new reason"),
            claim("feat/new", "agent-d", "fresh"),
        ];
        let roster = roster(&after, &beta);
        let planned = plan(
            &roster,
            "new",
            &named("main"),
            &holding(Some(&held_roster), false),
        )
        .expect("a notice");

        // Then: only the three changes, none of the unchanged claim.
        assert_eq!(
            planned.form,
            Form::Changes {
                since: "old".to_owned(),
                claimed: vec![&after[2]],
                changed: vec![&after[1]],
                released: vec!["feat/gone".to_owned()],
            }
        );
        let text = text("/w/main", &planned);
        assert!(!text.contains("feat/kept"), "{text}");
        assert!(text.contains("- newly claimed: feat/new"), "{text}");
        assert!(text.contains("- claim changed: feat/moved"), "{text}");
        assert!(text.contains("- released: feat/gone\n"), "{text}");
        assert!(!text.contains("`knives start <branch>`"), "{text}");
    }

    #[test]
    fn an_unchanged_roster_is_one_line_at_a_new_workspace_and_nothing_again() {
        let beta = UpstreamName::new("beta");
        let claims = [claim("feat/a", "agent-a", "porting")];
        let roster = roster(&claims, &beta);
        let digest = roster_digest(&beta, &roster);
        let held_roster = held(&roster, &digest);

        let first = plan(
            &roster,
            &digest,
            &named("main"),
            &holding(Some(&held_roster), false),
        )
        .expect("a line");
        assert_eq!(first.form, Form::Unchanged);
        let text = text("/w/main", &first);
        assert!(
            text.contains(&format!(
                "/w/main: the claims on this fork are unchanged since the notice with digest \
                 {digest}.\n</knives-notice-0123>"
            )),
            "{text}"
        );
        assert!(!text.contains("feat/a"), "{text}");

        assert_eq!(
            plan(
                &roster,
                &digest,
                &named("main"),
                &holding(Some(&held_roster), true)
            ),
            None
        );
    }

    #[test]
    fn a_claim_given_whole_is_not_given_whole_again_until_it_changes() {
        let beta = UpstreamName::new("beta");
        let claims = [claim("feat/a", "agent-a", "porting")];
        let roster_before = roster(&claims, &beta);
        let digest = roster_digest(&beta, &roster_before);
        let held_roster = held(&roster_before, &digest);
        let fingerprint = claim_fingerprint(&claims[0]);
        let holding = Holding {
            roster: Some(&held_roster),
            noticed: true,
            had_whole: |claim: &Claim| claim_fingerprint(claim) == fingerprint,
        };

        // A workspace of the branch, once the session had its claim whole and
        // had this digest here: nothing.
        assert_eq!(
            plan(&roster_before, &digest, &named("feat-a"), &holding),
            None
        );

        // The claim changed: whole again, as the change.
        let changed = [claim("feat/a", "agent-a", "porting, round two")];
        let roster_after = roster(&changed, &beta);
        let digest_after = roster_digest(&beta, &roster_after);
        let planned =
            plan(&roster_after, &digest_after, &named("feat-a"), &holding).expect("the change");
        assert_eq!(planned.here, vec![(&changed[0], true)]);
        assert!(
            matches!(&planned.form, Form::Changes { claimed, changed, released, .. }
                if claimed.is_empty() && changed.is_empty() && released.is_empty()),
            "{planned:?}"
        );
        assert_ne!(
            claim_fingerprint(&changed[0]),
            claim_fingerprint(&claims[0])
        );
    }

    #[test]
    fn a_workspace_holds_the_claim_on_the_branch_it_has_checked_out_whatever_its_name() {
        // Given: a claim on feat/a, and a checkout named `my-fix` with feat/a
        // checked out; the session holds the roster.
        let beta = UpstreamName::new("beta");
        let claims = [claim("feat/a", "agent-a", "porting")];
        let roster = roster(&claims, &beta);
        let digest = roster_digest(&beta, &roster);
        let held_roster = held(&roster, &digest);
        let branches = ["feat/a".to_owned()];
        let workspace = Workspace {
            name: Some("my-fix"),
            branches: &branches,
        };

        // When: a call touches it.
        let planned = plan(
            &roster,
            &digest,
            &workspace,
            &holding(Some(&held_roster), true),
        )
        .expect("the claim here");

        // Then: the claim is here, whole (B1).
        assert_eq!(planned.here, vec![(&claims[0], true)]);
        let text = text("/w/my-fix", &planned);
        assert!(
            text.contains("This workspace's branch is claimed: feat/a (agent-a,"),
            "{text}"
        );
    }

    #[test]
    fn an_unclaimed_branch_read_checked_out_is_named_and_nothing_is_claimed_otherwise() {
        let beta = UpstreamName::new("beta");
        let claims = [claim("feat/a", "agent-a", "porting")];
        let roster = roster(&claims, &beta);
        let digest = roster_digest(&beta, &roster);
        let held_roster = held(&roster, &digest);
        let branches = ["feat/b".to_owned()];
        let read = Workspace {
            name: Some("my-fix"),
            branches: &branches,
        };

        // A branch read checked out and holding no claim is named as such.
        let planned =
            plan(&roster, &digest, &read, &holding(Some(&held_roster), false)).expect("a line");
        let text_read = text("/w/my-fix", &planned);
        assert!(
            text_read.contains("\nNo claim is on the branch checked out here: feat/b.\n"),
            "{text_read}"
        );

        // With no branch read, nothing is said of claims here.
        let planned = plan(
            &roster,
            &digest,
            &named("my-fix"),
            &holding(Some(&held_roster), false),
        )
        .expect("a line");
        let text_unread = text("/w/my-fix", &planned);
        assert!(!text_unread.contains("No claim"), "{text_unread}");
    }

    #[test]
    fn shown_lists_what_each_form_prints() {
        let beta = UpstreamName::new("beta");
        let claims = [
            claim("feat/a", "agent-a", "porting"),
            claim("feat/b", "agent-b", "review"),
        ];
        let roster = roster(&claims, &beta);
        let first = plan(&roster, "d", &named("feat-a"), &holding(None, false)).expect("roster");
        assert_eq!(
            shown(&first, &[false]),
            vec![(&claims[1], Given::Line), (&claims[0], Given::Whole)]
        );
        // The claim here cut to fit is given as fully as the cap allows,
        // never whole.
        assert_eq!(
            shown(&first, &[true]),
            vec![(&claims[1], Given::Line), (&claims[0], Given::Fitted)]
        );

        // A Changes notice that leaves the claim here out shows only the delta.
        let before = [claim("feat/a", "agent-a", "porting")];
        let held_roster = held(&super::roster(&before, &beta), "old");
        let key_a = claim_fingerprint(&claims[0]);
        let holding_a = Holding {
            roster: Some(&held_roster),
            noticed: false,
            had_whole: |claim: &Claim| claim_fingerprint(claim) == key_a,
        };
        let changes = plan(&roster, "new", &named("feat-a"), &holding_a).expect("changes");
        assert_eq!(shown(&changes, &[false]), vec![(&claims[1], Given::Line)]);
    }

    /// The first notice of `claims` for the workspace `feat-here`, whose
    /// claim is `claims[0]`.
    fn first_notice(claims: &[Claim]) -> String {
        let beta = UpstreamName::new("beta");
        let roster = roster(claims, &beta);
        let planned =
            plan(&roster, "d1", &named("feat-here"), &holding(None, false)).expect("a notice");
        text("/w/feat-here", &planned)
    }

    #[test]
    fn a_notice_at_its_cap_keeps_every_row_and_one_character_more_leaves_rows_out() {
        // Given: the claim here, with a whole reason longer than a row, and
        // as many other claims as fit under the cap: one more is the first
        // the cap leaves out. Bounded, so a cap that never leaves a row out
        // fails here instead of looping.
        let long = "z".repeat(300);
        let mut claims = vec![claim("feat/here", "agent-a", &long)];
        for index in 1..=500 {
            claims.push(claim(&format!("feat/x{index:03}"), "agent-b", "review"));
            if first_notice(&claims).contains("more not shown here") {
                break;
            }
        }
        assert!(
            first_notice(&claims).contains("more not shown here"),
            "500 claims never reached the cap"
        );
        claims.pop();
        let below = first_notice(&claims).chars().count();
        // Padded to exactly the cap by the last claim's branch, which a row
        // prints whole.
        let last = claims.len() - 1;
        claims[last]
            .branch
            .push_str(&"p".repeat(NOTICE_CHARS - below));

        // When / Then: at the cap, every row and no count.
        let at = first_notice(&claims);
        assert_eq!(at.chars().count(), NOTICE_CHARS, "{at}");
        assert!(!at.contains("more not shown here"), "{at}");
        assert!(at.contains(&claims[last].branch), "{at}");

        // And: one character more leaves the last rows out, counts them, and
        // keeps the claim here whole, the usage lines and the cap.
        claims[last].branch.push('p');
        let over = first_notice(&claims);
        assert!(
            over.chars().count() <= NOTICE_CHARS,
            "{}",
            over.chars().count()
        );
        assert!(over.contains(&format!("): {long}\n")), "{over}");
        assert!(!over.contains(&claims[last].branch), "{over}");
        let others = claims.len() - 1;
        let kept = over.lines().filter(|line| line.starts_with("- ")).count();
        assert!(
            over.contains(&format!(
                "… and {} more not shown here: `knives status beta --no-landed` lists every claim.",
                others - kept
            )),
            "{over}"
        );
        assert!(over.contains("`knives start <branch>`"), "{over}");
    }

    #[test]
    fn a_claim_seized_with_the_same_reason_reads_changed() {
        // Given: a session that holds feat/a as agent-a's.
        let beta = UpstreamName::new("beta");
        let before = [claim("feat/a", "agent-a", "porting")];
        let held_roster = held(&roster(&before, &beta), "old");

        // When: agent-b holds it now, with the same reason.
        let after = [claim("feat/a", "agent-b", "porting")];
        let roster = roster(&after, &beta);
        let planned = plan(
            &roster,
            "new",
            &named("main"),
            &holding(Some(&held_roster), false),
        )
        .expect("a notice");

        // Then: a changed claim, naming the new holder.
        assert_eq!(
            planned.form,
            Form::Changes {
                since: "old".to_owned(),
                claimed: Vec::new(),
                changed: vec![&after[0]],
                released: Vec::new(),
            }
        );
        assert!(text("/w/main", &planned).contains("- claim changed: feat/a (agent-b,"));
    }

    #[test]
    fn an_unchanged_roster_with_a_claim_here_says_no_absence_and_gives_the_claim() {
        // Given: a session that holds the roster, and a workspace whose
        // claimed branch is read checked out there.
        let beta = UpstreamName::new("beta");
        let claims = [claim("feat/a", "agent-a", "porting")];
        let roster = roster(&claims, &beta);
        let digest = roster_digest(&beta, &roster);
        let held_roster = held(&roster, &digest);
        let branches = ["feat/a".to_owned()];
        let workspace = Workspace {
            name: Some("my-fix"),
            branches: &branches,
        };

        // When / Then: the one line and the claim, never a line of absence.
        let planned = plan(
            &roster,
            &digest,
            &workspace,
            &holding(Some(&held_roster), false),
        )
        .expect("a line");
        assert_eq!(planned.form, Form::Unchanged);
        let text = text("/w/my-fix", &planned);
        assert!(
            text.contains("This workspace's branch is claimed: feat/a"),
            "{text}"
        );
        assert!(!text.contains("No claim"), "{text}");
    }

    #[test]
    fn a_changes_notice_gives_the_claim_here_before_the_other_changes() {
        // Given: a session holding an older roster; feat/a is here and new.
        let beta = UpstreamName::new("beta");
        let before = [claim("feat/b", "agent-b", "review")];
        let held_roster = held(&roster(&before, &beta), "old");
        let after = [
            claim("feat/a", "agent-a", "porting"),
            claim("feat/c", "agent-c", "fresh"),
        ];
        let roster = roster(&after, &beta);

        // When: a call touches feat/a's workspace.
        let planned = plan(
            &roster,
            "new",
            &named("feat-a"),
            &holding(Some(&held_roster), false),
        )
        .expect("a notice");
        let text = text("/w/feat-a", &planned);

        // Then: right after the header, the claim here; then the rest.
        let here = text
            .find("This workspace's branch is claimed: feat/a")
            .expect("here");
        let new = text.find("- newly claimed: feat/c").expect("new");
        let gone = text.find("- released: feat/b").expect("released");
        assert!(here < new && new < gone, "{text}");
    }

    #[test]
    fn a_branch_read_or_the_directory_name_each_count() {
        // Given: claims on feat/a-b and feat-a/b, which both flatten to
        // `feat-a-b`, and on feat/c.
        let beta = UpstreamName::new("beta");
        let claims = [
            claim("feat-a/b", "agent-x", "one"),
            claim("feat/a-b", "agent-y", "two"),
            claim("feat/c", "agent-z", "three"),
        ];
        let roster = roster(&claims, &beta);

        // When / Then: a branch read counts whatever the directory's name,
        // and the name counts whatever branch is read: at a jj checkout's
        // detached HEAD the branch read can be the base, `main` (F1).
        let c = ["feat/c".to_owned()];
        let planned = plan(
            &roster,
            "d",
            &Workspace {
                name: Some("my-fix"),
                branches: &c,
            },
            &holding(None, false),
        )
        .expect("a notice");
        assert_eq!(planned.here, vec![(&claims[2], true)]);
        let main = ["main".to_owned()];
        let planned = plan(
            &roster,
            "d",
            &Workspace {
                name: Some("feat-c"),
                branches: &main,
            },
            &holding(None, false),
        )
        .expect("a notice");
        assert_eq!(planned.here, vec![(&claims[2], true)]);

        // And: a name two branches flatten to holds both: a claim told too
        // often is the safe side (N29).
        let planned =
            plan(&roster, "d", &named("feat-a-b"), &holding(None, false)).expect("a notice");
        assert_eq!(planned.here.len(), 2);
    }

    /// The cap probe's case: the first notice of `claims`, at the workspace
    /// `feat-here`, with every claim's branch checked out there when
    /// `current` (a detached `HEAD` at several branch tips).
    fn sol_case(claims: &[Claim], current: bool) -> (NoticePlan<'_>, Rendered) {
        let beta = UpstreamName::new("beta");
        let branches = if current {
            claims.iter().map(|claim| claim.branch.clone()).collect()
        } else {
            Vec::new()
        };
        let workspace = Workspace {
            name: Some("feat-here"),
            branches: &branches,
        };
        let roster = roster(claims, &beta);
        let planned = plan(&roster, "d1", &workspace, &holding(None, false)).expect("a notice");
        let rendered = rendered("/w/feat-here", &planned);
        (planned, rendered)
    }

    #[test]
    fn the_cap_controls_a_short_notice_and_crowded_rows() {
        let short = [claim("feat/here", "agent-a", "short")];
        let (_, rendered) = sol_case(&short, true);
        assert!(rendered.text.chars().count() <= NOTICE_CHARS);
        assert!(rendered.text.contains("): short\n"), "{}", rendered.text);
        assert_eq!(rendered.cut, [false]);

        let crowded = (0..500)
            .map(|n| claim(&format!("feat/other-{n}"), "agent-a", "review"))
            .collect::<Vec<_>>();
        let (_, rendered) = sol_case(&crowded, false);
        assert!(rendered.text.chars().count() <= NOTICE_CHARS);
        assert!(
            rendered.text.contains("more not shown here"),
            "{}",
            rendered.text
        );
    }

    #[test]
    fn a_current_claim_with_a_10000_character_reason_is_cut_to_the_cap() {
        // Given: one claim here whose reason is 10,000 characters, most of
        // them more than one byte (the review's first case rendered 10,634).
        let reason = "é—x".repeat(3_333) + "é";
        assert_eq!(reason.chars().count(), 10_000);
        assert!(reason.len() > 10_000);
        let claims = [claim("feat/here", "agent-a", &reason)];

        // When: its first notice is written.
        let (planned, rendered) = sol_case(&claims, true);

        // Then: within the cap, the branch and holder kept, the reason cut
        // with how much of it is shown, and the claim recorded as fitted,
        // not whole.
        let text = &rendered.text;
        assert!(
            text.chars().count() <= NOTICE_CHARS,
            "{}",
            text.chars().count()
        );
        assert!(
            text.contains("This workspace's branch is claimed: feat/here (agent-a,"),
            "{text}"
        );
        assert!(text.contains("… (reason cut: "), "{text}");
        assert!(text.contains(" of 10000 characters)\n"), "{text}");
        assert!(text.contains("é—xé—x"), "{text}");
        assert_eq!(rendered.cut, [true]);
        assert_eq!(
            shown(&planned, &rendered.cut),
            vec![(&claims[0], Given::Fitted)]
        );
    }

    #[test]
    fn many_current_claims_whose_metadata_alone_is_over_the_cap_are_summarised() {
        // Given: 120 claims here with empty reasons (the review's second case
        // rendered 24,151), their branch names more than one byte a letter.
        let claims = (0..120)
            .map(|n| claim(&format!("feat/hére-{n}-{}", "b".repeat(60)), "agent-a", ""))
            .collect::<Vec<_>>();

        // When: their first notice is written.
        let (planned, rendered) = sol_case(&claims, true);

        // Then: within the cap; the claims that fit each on their line, the
        // rest counted on one bounded line; those recorded as fitted, the
        // others whole.
        let text = &rendered.text;
        assert!(
            text.chars().count() <= NOTICE_CHARS,
            "{}",
            text.chars().count()
        );
        let given = text
            .lines()
            .filter(|line| line.starts_with("This workspace's branch is claimed: "))
            .count();
        assert!(given > 0 && given < 120, "{given}");
        let folded = 120 - given;
        assert!(
            text.contains(&format!("… and {folded} more claims here, cut to fit: ")),
            "{text}"
        );
        assert!(text.contains("`knives start <branch>`"), "{text}");
        let records = shown(&planned, &rendered.cut);
        let fitted = records
            .iter()
            .filter(|(_, given)| *given == Given::Fitted)
            .count();
        let whole = records
            .iter()
            .filter(|(_, given)| *given == Given::Whole)
            .count();
        assert_eq!((whole, fitted), (given, folded));
    }

    #[test]
    fn a_registry_key_of_10000_characters_is_cut_in_the_envelope_and_the_count() {
        // Given: a registry key of 10,000 characters, which the registry
        // takes (the review's case rendered 10,376), and more claims than fit.
        let key = "k".repeat(10_000);
        let claims = (0..300)
            .map(|n| claim(&format!("feat/other-{n}"), "agent-a", "review"))
            .collect::<Vec<_>>();
        let beta = UpstreamName::new("beta");
        let roster = roster(&claims, &beta);
        let planned = plan(&roster, "d1", &named("main"), &holding(None, false)).expect("a notice");

        // When: its notice is written.
        let text = format_notice(
            &GuidanceRoot {
                name: key,
                root: PathBuf::from("/w/main"),
            },
            &planned,
            &Sightings {
                seen: &Seen::default(),
                now: at("2026-01-03T00:00:00Z"),
            },
            "0123",
        )
        .text;

        // Then: within the cap; the key cut to KEY_CHARS in the envelope and
        // in the count of rows left out, each marked.
        assert!(
            text.chars().count() <= NOTICE_CHARS,
            "{}",
            text.chars().count()
        );
        let cut = "k".repeat(KEY_CHARS);
        assert!(
            text.contains(&format!(
                "<knives-notice-0123 repo=\"{cut}...\" digest=\"d1\">"
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("`knives status {cut}… --no-landed`")),
            "{text}"
        );
    }

    #[test]
    fn every_step_fits_a_notice_within_the_floor() {
        // Given: everything at once: a 10,000-character key, a 20,000-character
        // workspace path, 60 claims here with long multi-byte reasons, and 300
        // claims elsewhere.
        let reason = "é—x".repeat(400);
        let mut claims = (0..60)
            .map(|n| claim(&format!("feat/hére-{n}"), "agent-a", &reason))
            .collect::<Vec<_>>();
        claims.extend((0..300).map(|n| claim(&format!("feat/other-{n}"), "agent-b", "review")));
        let beta = UpstreamName::new("beta");
        let branches = (0..60)
            .map(|n| format!("feat/hére-{n}"))
            .collect::<Vec<_>>();
        let workspace = Workspace {
            name: Some("feat-here"),
            branches: &branches,
        };
        let roster = roster(&claims, &beta);
        let planned = plan(&roster, "d1", &workspace, &holding(None, false)).expect("a notice");
        let root = format!("/{}", "p".repeat(20_000));

        // When: it is written within the floor.
        let rendered = format_notice_within(
            &GuidanceRoot {
                name: "k".repeat(10_000),
                root: PathBuf::from(&root),
            },
            &planned,
            &Sightings {
                seen: &Seen::default(),
                now: at("2026-01-03T00:00:00Z"),
            },
            Bounds {
                nonce: "0123",
                cap: NOTICE_FLOOR_CHARS,
            },
        );

        // Then: within it, every claim here cut or folded, never whole.
        assert!(
            rendered.text.chars().count() <= NOTICE_FLOOR_CHARS,
            "{}",
            rendered.text.chars().count()
        );
        assert!(rendered.cut.iter().all(|cut| *cut), "{:?}", rendered.cut);
    }

    #[test]
    fn a_registry_key_of_10000_characters_is_cut_in_the_refusal_envelope() {
        // When: the refusal is written under a key of 10,000 characters.
        let text = super::format_refusal(&"k".repeat(10_000), "the claims file is unreadable");

        // Then: its envelope names the key cut to KEY_CHARS and marked.
        assert!(
            text.contains(&format!(" repo=\"{}...\">", "k".repeat(KEY_CHARS))),
            "{}",
            &text[..300]
        );
        assert!(text.chars().count() < 500, "{}", text.chars().count());
    }
}
