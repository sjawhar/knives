//! The claims notice: who holds which branch of a managed fork, told once per
//! session and afterwards only as it changes.
//!
//! The session keeps the roster it was told ([`HeldRoster`]). The first notice
//! in a session lists every claim with its reason cut to one line; a later one
//! lists only the claims taken, changed or released since; and the claim on
//! the branch of the workspace a call touched is given whole, once. A roster
//! the session already holds is said again only as one line, the first time a
//! call touches another workspace of the fork.

use std::{collections::BTreeMap, path::Path};

use crate::commands::claim::{owner_kind_label, render_claim_line};
use crate::commands::wip::workspace_for;
use crate::hook::guidance::{FNV_OFFSET_BASIS, envelope_nonce, fnv1a, safe_attribute};
use crate::hook::state::HeldRoster;
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

/// The key a session remembers a claim given whole by: its fork, its branch
/// and its [`claim_fingerprint`], so a changed claim is given whole again.
pub fn whole_key(claim: &Claim) -> String {
    format!(
        "{}\x1f{}\x1f{}",
        claim.repo,
        claim.branch,
        claim_fingerprint(claim)
    )
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
    /// The claims on the branch of the workspace the call touched, each with
    /// whether to give it whole: the session has not had it whole yet.
    pub here: Vec<(&'a Claim, bool)>,
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
    /// Whether the session had a claim given whole ([`whole_key`]).
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

/// The notice for a call that touched the workspace directory `workspace` of
/// a fork whose [`roster`] has the digest `digest`; `None`: nothing to tell.
///
/// `workspace` is the directory's name, `None` when it has none. The claims
/// "here" are those on the branch whose workspace the directory is: a
/// workspace's directory is named for its branch ([`workspace_for`]), the
/// convention sightings key on ([`crate::seen`]).
pub fn plan<'a, F: Fn(&Claim) -> bool>(
    roster: &[&'a Claim],
    digest: &str,
    workspace: Option<&str>,
    holding: &Holding<'_, F>,
) -> Option<NoticePlan<'a>> {
    let here = roster
        .iter()
        .copied()
        .filter(|claim| workspace.is_some_and(|name| workspace_for(&claim.branch) == name))
        .map(|claim| (claim, !(holding.had_whole)(claim)))
        .collect::<Vec<_>>();
    let elsewhere = |claim: &&Claim| !here.iter().any(|(held, _)| std::ptr::eq(*held, *claim));
    let Some(held) = holding.roster else {
        let others = roster.iter().copied().filter(elsewhere).collect();
        return Some(NoticePlan {
            digest: digest.to_owned(),
            form: Form::Roster(others),
            here,
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
    })
}

/// The lines of the notice `plan` describes, for the workspace `root`.
fn notice_lines(root: &Path, plan: &NoticePlan<'_>, sightings: &Sightings<'_>) -> Vec<String> {
    let digest = &plan.digest;
    let line = |claim: &Claim, whole: bool| claim_line(claim, whole, sightings);
    let here = plan.here.iter().map(|(claim, whole)| {
        format!(
            "This workspace's branch is claimed: {}",
            line(claim, *whole)
        )
    });
    let mut lines = Vec::new();
    match &plan.form {
        Form::Roster(others) => {
            lines.push(format!(
                "{} is a fork managed by knives, and another agent may be working in it.",
                root.display()
            ));
            lines.extend(here);
            lines.push(match (others.len(), plan.here.is_empty()) {
                (0, true) => "No branch is claimed here right now.".to_owned(),
                (0, false) => "No other branch is claimed here right now.".to_owned(),
                (count, true) => {
                    format!("Branches claimed here ({count}), each reason cut to one line:")
                }
                (count, false) => {
                    format!("Other branches claimed here ({count}), each reason cut to one line:")
                }
            });
            lines.extend(
                others
                    .iter()
                    .map(|claim| format!("- {}", line(claim, false))),
            );
            lines.extend(USAGE.iter().map(|line| (*line).to_owned()));
        }
        Form::Changes {
            since,
            claimed,
            changed,
            released,
        } => {
            lines.push(format!(
                "The claims on this fork changed since the notice with digest {since}; only the \
                 changes follow."
            ));
            lines.extend(
                claimed
                    .iter()
                    .map(|claim| format!("- newly claimed: {}", line(claim, false))),
            );
            lines.extend(
                changed
                    .iter()
                    .map(|claim| format!("- claim changed: {}", line(claim, false))),
            );
            lines.extend(
                released.iter().map(|branch| {
                    format!("- released: {}", crate::ledger::inline_human_text(branch))
                }),
            );
            // A claim here the session had whole and that did not change is
            // no change.
            lines.extend(
                here.zip(&plan.here)
                    .filter(|(_, (_, whole))| *whole)
                    .map(|(line, _)| line),
            );
        }
        Form::Unchanged => {
            lines.push(format!(
                "{}: the claims on this fork are unchanged since the notice with digest {digest}.",
                root.display()
            ));
            if plan.here.is_empty() {
                lines.push("No claimed branch has its workspace here.".to_owned());
            }
            lines.extend(here);
        }
    }
    lines
}

/// How to work in a managed fork, said with the full roster.
const USAGE: [&str; 4] = [
    "Use knives rather than jj or git directly here: `knives status` for the state of",
    "every branch, `knives start <branch>` to take a branch and get your own workspace,",
    "`knives finish <branch>` as soon as your active work there stops — the branch,",
    "its bookmark, and any open pull request all survive the release.",
];

/// The notice `plan` describes for the workspace `root` of the fork the
/// registry names `repo_name`.
pub fn format_notice(
    repo_name: &str,
    root: &Path,
    plan: &NoticePlan<'_>,
    sightings: &Sightings<'_>,
) -> String {
    let nonce = envelope_nonce();
    [
        String::new(),
        String::new(),
        format!(
            "<knives-notice-{nonce} repo=\"{}\" digest=\"{}\">",
            safe_attribute(repo_name),
            plan.digest
        ),
    ]
    .into_iter()
    .chain(notice_lines(root, plan, sightings))
    .chain([format!("</knives-notice-{nonce}>")])
    .collect::<Vec<_>>()
    .join("\n")
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
            safe_attribute(repo_name)
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
    use std::path::Path;

    use super::{
        Form, Holding, NoticePlan, REASON_CHARS, Sightings, claim_fingerprint, claim_line,
        format_notice, held, one_line, plan, roster, roster_digest, whole_key,
    };
    use crate::hook::state::HeldRoster;
    use crate::ids::UpstreamName;
    use crate::seen::Seen;
    use crate::store::{Claim, OwnerKind};

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
        format_notice(
            "beta",
            Path::new(root),
            planned,
            &Sightings {
                seen: &Seen::default(),
                now: at("2026-01-03T00:00:00Z"),
            },
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
            plan(&roster, &digest, Some("main"), &holding(None, false)).expect("a notice");

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
            plan(&roster, "d1", Some("feat-here"), &holding(None, false)).expect("a notice");

        // Then: the touched branch's claim is whole, the other cut.
        assert_eq!(
            planned,
            NoticePlan {
                digest: "d1".to_owned(),
                form: Form::Roster(vec![&claims[1]]),
                here: vec![(&claims[0], true)],
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
            Some("main"),
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
            Some("main"),
            &holding(Some(&held_roster), false),
        )
        .expect("a line");
        assert_eq!(first.form, Form::Unchanged);
        let text = text("/w/main", &first);
        assert!(
            text.contains(&format!(
                "/w/main: the claims on this fork are unchanged since the notice with digest \
                 {digest}.\nNo claimed branch has its workspace here.\n"
            )),
            "{text}"
        );
        assert!(!text.contains("feat/a"), "{text}");

        assert_eq!(
            plan(
                &roster,
                &digest,
                Some("main"),
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
        let key = whole_key(&claims[0]);
        let holding = Holding {
            roster: Some(&held_roster),
            noticed: true,
            had_whole: |claim: &Claim| whole_key(claim) == key,
        };

        // A workspace of the branch, once the session had its claim whole and
        // had this digest here: nothing.
        assert_eq!(
            plan(&roster_before, &digest, Some("feat-a"), &holding),
            None
        );

        // The claim changed: whole again, as the change.
        let changed = [claim("feat/a", "agent-a", "porting, round two")];
        let roster_after = roster(&changed, &beta);
        let digest_after = roster_digest(&beta, &roster_after);
        let planned =
            plan(&roster_after, &digest_after, Some("feat-a"), &holding).expect("the change");
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
}
