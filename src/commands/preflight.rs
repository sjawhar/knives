//! `knives preflight`: the facts you need before contributing upstream.
//!
//! The programmatic half only. Anything of the form "have you read the
//! contributing guide, and does this change comply" is judgment, which a CLI
//! cannot evaluate and must not pretend to. That half is the skill's job, and
//! it consumes this output.

use std::collections::BTreeSet;
use std::path::Path;

use crate::cli::Exit;
use crate::config::RepoEntry;
use crate::detect::{Finding, divergent_changes, stale_parents};
use crate::forge::{Forge, PullSummary};
use crate::ids::{BookmarkRef, RepoName, is_release_name};
use crate::jj::Repo;
use crate::snapshot::{self, SnapshotConfig};
use crate::store::Store;

/// Files a project uses to state how to contribute.
pub const CONVENTION_FILES: &[&str] = &[
    "AGENTS.md",
    "CONTRIBUTING.md",
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Convention {
    Absent {
        file: String,
    },
    Unchanged {
        file: String,
    },
    /// Present and different from the last time we looked. The agent has to
    /// read it again, and saying so is the whole value of storing a digest.
    Changed {
        file: String,
    },
    FirstSeen {
        file: String,
    },
}

impl Convention {
    pub fn render(&self) -> String {
        match self {
            Self::Absent { file } => format!("  {file}: absent"),
            Self::Unchanged { file } => format!("  {file}: present, unchanged since last seen"),
            Self::Changed { file } => {
                format!("  {file}: present, CHANGED since last seen, read it again")
            }
            Self::FirstSeen { file } => format!("  {file}: present, not seen before, read it"),
        }
    }
}

/// FNV-1a, chosen because it is specified.
///
/// `DefaultHasher` is explicitly unspecified across Rust releases, and this
/// value is persisted, so using it meant a toolchain upgrade re-reported every
/// convention file as changed.
pub fn digest(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Any numeric cap the project's own policy states.
///
/// Parsed rather than assumed. Reporting "unknown" is correct when the project
/// does not say; inventing a number would be worse than silence, because an
/// agent would treat it as policy.
pub fn stated_pull_request_cap(policy: &str) -> Option<u32> {
    // Strict on purpose. The skill instructs an agent not to open a pull request
    // once the cap is reached, so a number invented from unrelated prose blocks
    // legitimate contributions. The number must be the token immediately after a
    // limit phrase, in a sentence that names pull requests as a word.
    const LIMITS: &[&str] = &["at most", "no more than", "limit of", "maximum of", "up to"];
    let lowered = policy.to_lowercase();
    for sentence in lowered.split(['.', '\n']) {
        let names_prs = sentence.contains("pull request")
            || sentence
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word == "pr" || word == "prs");
        if !names_prs {
            continue;
        }
        for phrase in LIMITS {
            let Some(rest) = sentence.split_once(phrase).map(|(_, tail)| tail) else {
                continue;
            };
            let Some(token) = rest.split_whitespace().next() else {
                continue;
            };
            if let Ok(value) = token
                .trim_matches(|c: char| !c.is_ascii_digit())
                .parse::<u32>()
                && value > 0
                && value < 100
            {
                return Some(value);
            }
        }
    }
    None
}

fn owned_open_pull_request_count(pull_requests: &[PullSummary], ours: &BTreeSet<String>) -> usize {
    pull_requests
        .iter()
        .filter(|pull_request| {
            ours.contains(pull_request.head_ref_name.as_str()) && pull_request.is_open()
        })
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchState {
    pub branch: String,
    pub claimed_by: Option<String>,
    pub stale_release_parent: bool,
    pub divergent: bool,
    pub landed: Option<bool>,
}

impl BranchState {
    pub fn render(&self) -> String {
        let mut bits = vec![format!("  {}", self.branch)];
        bits.push(self.claimed_by.as_ref().map_or_else(
            || "unclaimed".to_owned(),
            |owner| format!("claimed by {owner}"),
        ));
        if self.stale_release_parent {
            bits.push("a release still pins an older commit of it".to_owned());
        }
        if self.divergent {
            bits.push("divergent".to_owned());
        }
        bits.push(match self.landed {
            Some(true) => "landed upstream".to_owned(),
            Some(false) => "not landed".to_owned(),
            None => "landed: not probed".to_owned(),
        });
        bits.join("; ")
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub repo: String,
    pub conventions: Vec<Convention>,
    pub open_pull_requests: Option<usize>,
    pub stated_cap: Option<u32>,
    pub claimed_branches: Vec<String>,
    /// Per-branch state the spec asks for. `landed` is absent rather than false
    /// when the probe was not run: the probe mutates, so it is opt-in, and
    /// "not checked" must never render as "not landed".
    pub branch_state: Vec<BranchState>,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
}

pub fn gather(
    name: &RepoName,
    entry: &RepoEntry,
    store: &mut Store,
    forge: &dyn Forge,
    cache: Option<&std::path::Path>,
) -> Report {
    let mut report = Report {
        repo: name.to_string(),
        ..Report::default()
    };

    for file in CONVENTION_FILES {
        let path = entry.path.join(file);
        let Ok(content) = std::fs::read_to_string(&path) else {
            report.conventions.push(Convention::Absent {
                file: (*file).to_owned(),
            });
            continue;
        };
        let current = digest(&content);
        let state = match store.convention_digest(name, file) {
            None => Convention::FirstSeen {
                file: (*file).to_owned(),
            },
            Some(seen) if seen == current => Convention::Unchanged {
                file: (*file).to_owned(),
            },
            Some(_) => Convention::Changed {
                file: (*file).to_owned(),
            },
        };
        report.conventions.push(state);
        store.record_convention_digest(name, file, &current);

        if file.eq_ignore_ascii_case("CONTRIBUTING.md") {
            report.stated_cap = stated_pull_request_cap(&content);
        }
    }

    match snapshot::open(SnapshotConfig {
        forge,
        path: &entry.path,
        remotes: [
            entry.remote(crate::config::Role::Origin),
            entry.remote(crate::config::Role::Release),
        ],
        cache_root: cache,
    }) {
        Ok(opened) => match opened
            .discover()
            .and_then(|discovery| discovery.complete(&[]))
        {
            // The cap counts every open pull request on a local branch. Unlike
            // the snapshot's ours() helper, this deliberately does not filter
            // by head repository owner: that would silently shrink today's
            // count.
            Ok(snapshot) => {
                if let Err(error) = snapshot.persist(None) {
                    report.notes.push(format!("forge cache not saved: {error}"));
                }
                match Repo::open(&entry.path).and_then(|repo| repo.bookmark_tips()) {
                    Ok(tips) => {
                        let ours: BTreeSet<String> = tips
                            .keys()
                            .filter_map(|reference| match reference {
                                BookmarkRef::Local(branch) => Some(branch.to_string()),
                                BookmarkRef::Remote { .. } => None,
                            })
                            .collect();
                        report.open_pull_requests =
                            Some(owned_open_pull_request_count(snapshot.rows(), &ours));
                    }
                    Err(error) => report
                        .notes
                        .push(format!("could not read our branches: {error}")),
                }
            }
            Err(error) => report
                .notes
                .push(format!("open pull request count unavailable: {error}")),
        },
        Err(error) => report
            .notes
            .push(format!("open pull request count unavailable: {error}")),
    }

    let claims = store.claims(Some(name));
    report.claimed_branches = claims.iter().map(|claim| claim.branch.clone()).collect();

    // The spec asks for claimed, stale, landed, or divergent. Everything but
    // landed is answerable read-only, so it is answered here; landed needs the
    // mutating probe and is reported as "not probed" rather than guessed.
    match branch_states_with_findings(entry, &claims) {
        Ok((states, findings)) => {
            report.branch_state = states;
            report.findings.extend(findings);
        }
        Err(error) => report
            .notes
            .push(format!("branch state unavailable: {error}")),
    }
    report
}

/// Per-branch state, read-only. Public so an integration test can pin the
/// change-id versus commit-id comparison, which was wrong once already.
pub fn branch_states(
    entry: &RepoEntry,
    claims: &[&crate::store::Claim],
) -> anyhow::Result<Vec<BranchState>> {
    branch_states_with_findings(entry, claims).map(|(states, _)| states)
}

fn branch_states_with_findings(
    entry: &RepoEntry,
    claims: &[&crate::store::Claim],
) -> anyhow::Result<(Vec<BranchState>, Vec<Finding>)> {
    let repo = Repo::open(&entry.path)?;
    let tips = repo.bookmark_tips()?;
    let scheme = entry.release_scheme();
    let ignored: BTreeSet<crate::ids::BookmarkRef> =
        crate::commands::release::superseded_dated_releases(&tips)
            .into_iter()
            .map(|(reference, _)| reference)
            .collect();
    let divergent: std::collections::BTreeSet<String> =
        divergent_changes(&repo.divergent_changes(&ignored)?)
            .into_iter()
            .map(|finding| finding.subject.to_string())
            .collect();

    // Which branches a release still pins an older commit of.
    let mut stale_branches = std::collections::BTreeSet::new();
    for (reference, commit) in &tips {
        if !is_release_name(reference.branch(), &scheme) {
            continue;
        }
        for finding in stale_parents(&repo.parents_of(commit.as_str())?, &tips) {
            if let crate::detect::Subject::Commit(commit) = finding.subject {
                stale_branches.insert(commit.to_string());
            }
        }
    }

    let mut states: Vec<BranchState> = Vec::new();

    // Divergent branches first: their bookmarks are conflicted, so they are
    // absent from the tip map below and would otherwise never be reported.
    for (reference, _) in repo.conflicted_bookmarks()? {
        if is_release_name(reference.branch(), &scheme)
            || reference.branch().as_str() == entry.trunk()
        {
            continue;
        }
        states.push(BranchState {
            branch: reference.branch().to_string(),
            claimed_by: claims
                .iter()
                .find(|claim| claim.branch == reference.branch().as_str())
                .map(|claim| claim.owner.clone()),
            stale_release_parent: false,
            divergent: true,
            landed: None,
        });
    }

    states.extend(tips.iter().filter_map(|(reference, commit)| {
        match reference {
            BookmarkRef::Local(branch)
                if !is_release_name(branch, &scheme) && branch.as_str() != entry.trunk() =>
            {
                Some(BranchState {
                    branch: branch.to_string(),
                    claimed_by: claims
                        .iter()
                        .find(|claim| claim.branch == branch.as_str())
                        .map(|claim| claim.owner.clone()),
                    stale_release_parent: stale_branches.contains(commit.as_str()),
                    divergent: divergent.contains(commit.as_str()),
                    landed: None,
                })
            }
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        }
    }));
    let mut findings = Vec::new();
    if let (Some((_, release)), Ok(trunk_tip)) = (
        crate::commands::release::newest_release(&tips, &scheme, entry.publish_remote()),
        repo.resolve_commit(&entry.upstream_trunk()),
    ) && let Some(base) = crate::commands::release::shared_base(&repo, &release, &trunk_tip)?
    {
        let members = crate::commands::release::carried_from_tips(&tips, entry.trunk(), &scheme);
        findings.extend(crate::commands::release::mixed_base_findings(
            &entry.path,
            &members,
            &base,
            &trunk_tip,
        )?);
    }
    Ok((states, findings))
}

pub fn render(report: &Report) -> String {
    let mut lines: Vec<String> = report
        .notes
        .iter()
        .map(|note| format!("! {note}"))
        .collect();
    lines.push(format!("{}: convention files", report.repo));
    lines.extend(report.conventions.iter().map(Convention::render));

    lines.push(match (report.open_pull_requests, report.stated_cap) {
        (Some(count), Some(cap)) => {
            format!("  open pull requests: {count} of a stated cap of {cap}")
        }
        (Some(count), None) => {
            format!("  open pull requests (ours): {count}; the project states no cap")
        }
        (None, _) => "  open pull requests (ours): unknown".to_owned(),
    });

    if report.claimed_branches.is_empty() {
        lines.push("  claimed branches: none".to_owned());
    } else {
        lines.push(format!(
            "  claimed branches: {}",
            report.claimed_branches.join(", ")
        ));
    }
    if report.branch_state.is_empty() {
        lines.push("  branches: none".to_owned());
    } else {
        lines.push("  branch state:".to_owned());
        lines.extend(report.branch_state.iter().map(BranchState::render));
    }
    for finding in &report.findings {
        lines.push(format!("  !! {}", finding.detail));
    }
    lines.push("  judgment is not this command's job: run the pre-PR skill next".to_owned());
    lines.join("\n")
}

pub const fn exit_for(report: &Report) -> Exit {
    let notes = if report.notes.is_empty() {
        Exit::Ok
    } else {
        Exit::Incomplete
    };
    let findings = if report.findings.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    };
    notes.worst(findings)
}

/// Convenience for callers that only have a path.
pub fn has_convention(root: &Path, file: &str) -> bool {
    root.join(file).exists()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    #[test]
    fn only_open_pulls_on_our_branches_count_toward_a_cap() {
        // Given: our branches with both an open and a merged pull request, plus an
        // open pull request on a branch we do not carry.
        let pull_requests = vec![
            crate::forge::PullSummary {
                number: 1,
                state: "OPEN".to_owned(),
                review_decision: String::new(),
                head_ref_name: "feat/open".to_owned(),
                head_ref_oid: "aa".to_owned(),
                updated_at: "2026-08-01T00:00:00Z".to_owned(),
                is_draft: false,
                url: String::new(),
                head_repository_owner: None,
                base_ref_name: "main".to_owned(),
                merge_commit: None,
            },
            crate::forge::PullSummary {
                number: 2,
                state: "MERGED".to_owned(),
                review_decision: String::new(),
                head_ref_name: "feat/merged".to_owned(),
                head_ref_oid: "bb".to_owned(),
                updated_at: "2026-08-01T00:00:00Z".to_owned(),
                is_draft: false,
                url: String::new(),
                head_repository_owner: None,
                base_ref_name: "main".to_owned(),
                merge_commit: None,
            },
            crate::forge::PullSummary {
                number: 3,
                state: "OPEN".to_owned(),
                review_decision: String::new(),
                head_ref_name: "outside/open".to_owned(),
                head_ref_oid: "cc".to_owned(),
                updated_at: "2026-08-01T00:00:00Z".to_owned(),
                is_draft: false,
                url: String::new(),
                head_repository_owner: None,
                base_ref_name: "main".to_owned(),
                merge_commit: None,
            },
        ];
        let ours = BTreeSet::from(["feat/open".to_owned(), "feat/merged".to_owned()]);

        // When: the cap count is derived from all snapshot rows.
        let count = owned_open_pull_request_count(&pull_requests, &ours);

        // Then: neither merged history nor an open pull request on a branch we
        // do not carry consumes a slot.
        assert_eq!(count, 1);
    }

    #[test]
    fn an_open_pull_from_another_owner_still_counts_when_its_branch_is_ours() {
        // The cap counts submissions by branch name, exactly as today's unfiltered
        // list did: an owner filter here would silently shrink the count.
        let pull = crate::forge::PullSummary {
            number: 7,
            state: "OPEN".to_owned(),
            review_decision: String::new(),
            head_ref_name: "feat/ours".to_owned(),
            head_ref_oid: "aa".to_owned(),
            updated_at: "2026-08-01T00:00:00Z".to_owned(),
            is_draft: false,
            url: String::new(),
            head_repository_owner: Some(crate::forge::Account {
                login: "someone-else".to_owned(),
            }),
            base_ref_name: "main".to_owned(),
            merge_commit: None,
        };
        let ours = BTreeSet::from(["feat/ours".to_owned()]);

        assert_eq!(owned_open_pull_request_count(&[pull], &ours), 1);
    }

    #[test]
    fn a_changed_convention_file_is_distinguished_from_an_unchanged_one() {
        assert_ne!(digest("first"), digest("second"));
        assert_eq!(digest("same"), digest("same"));
    }

    #[test]
    fn a_stated_cap_is_read_from_the_policy_text() {
        let policy = "Contributors may have at most 4 open pull requests at a time.";
        assert_eq!(stated_pull_request_cap(policy), Some(4));
    }

    #[test]
    fn no_stated_cap_reports_none_rather_than_a_guess() {
        // Inventing a number would be worse than silence: an agent would treat
        // it as policy and refuse legitimate work.
        let policy = "Please open a pull request against main and be nice.";
        assert_eq!(stated_pull_request_cap(policy), None);
    }

    #[test]
    fn a_line_about_pull_requests_without_a_limit_word_is_not_a_cap() {
        let policy = "Every pull request needs 2 reviewers.";
        assert_eq!(stated_pull_request_cap(policy), None);
    }

    #[test]
    fn the_render_says_judgment_is_elsewhere() {
        // The command reports facts. If it implied it had judged compliance,
        // an agent would skip the gate that actually does.
        let report = Report {
            repo: "a-repo".to_owned(),
            ..Report::default()
        };
        assert!(render(&report).contains("judgment is not this command's job"));
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    #[test]
    fn a_real_stated_cap_is_read() {
        assert_eq!(
            stated_pull_request_cap(
                "Contributors may have at most 4 open pull requests at a time."
            ),
            Some(4)
        );
    }

    #[test]
    fn prose_that_merely_contains_a_number_is_not_a_cap() {
        // Every one of these produced a fabricated cap in the previous parser.
        // The skill tells an agent not to open a pull request once the cap is
        // reached, so a wrong number blocks legitimate contributions.
        for text in [
            "Please limit each PR to one logical change; see section 2.",
            "We limit review scope; see our process for the 3 stages.",
            "Do not limit yourself: open a PR for anything, see PEP 8 and RFC 42.",
            "Every pull request needs 2 reviewers.",
            "Open a pull request against main and be nice.",
        ] {
            assert_eq!(
                stated_pull_request_cap(text),
                None,
                "fabricated a cap from: {text}"
            );
        }
    }

    #[test]
    fn the_digest_is_stable_across_runs_and_toolchains() {
        // Persisted, so an unspecified hash meant a toolchain upgrade reported
        // every convention file as changed.
        assert_eq!(digest("hello"), digest("hello"));
        assert_ne!(digest("hello"), digest("hello "));
        assert_eq!(digest("hello"), "a430d84680aabd0b");
    }
}
