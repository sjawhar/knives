//! `knives release`: cut or repair a dated release.
//!
//! Everything here is a check, never a prompt. A CLI in a non-interactive agent
//! session has nobody to ask, so it decides from evidence and says what it
//! decided. Cutting is opt-in: planning is the default because this is the only
//! command that writes to a remote.

use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::commands::status::UPSTREAM_TRUNK;
use crate::config::{RepoEntry, Role};
use crate::detect::{Finding, stale_parents};
use crate::ids::{BookmarkRef, CommitId, RepoName};
use crate::jj::Repo;
use crate::pins::{PIN_FILES, Pin, PinKind, scan};

/// What repairing this release would actually reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairEffect {
    /// At least one consumer follows the branch, so repairing in place reaches
    /// them and a new dated name is not required. A needless dated name burns
    /// the name and forces a re-pin nobody wanted.
    RepairInPlace,
    /// Every pin is frozen, so the next cut has to take a new dated suffix.
    NewDatedName,
    /// Nothing pins it, so either is safe.
    Unpinned,
}

pub fn repair_effect(pins: &[Pin]) -> RepairEffect {
    if pins.is_empty() {
        return RepairEffect::Unpinned;
    }
    if pins.iter().any(|pin| pin.kind == PinKind::Follows) {
        return RepairEffect::RepairInPlace;
    }
    RepairEffect::NewDatedName
}

/// Scan a consumer checkout for pins of one repo's releases.
///
/// Scoped by `slug`, the repository's name as it appears in a dependency line. These
/// forks cut releases on one dated scheme, so `release/2026-07-28` exists in several of
/// them at once; an unscoped scan attributed a sibling's pin to this repo, which reads
/// as "pinned at the newest cut" when it is not pinned here at all. `None` keeps every
/// pin, for a caller that genuinely wants the whole file.
pub fn scan_consumer_for(consumer: &Path, slug: Option<&str>) -> Vec<Pin> {
    let mut pins = Vec::new();
    for name in PIN_FILES {
        let path = consumer.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            pins.extend(
                scan(name, &text)
                    .into_iter()
                    .filter(|pin| slug.is_none_or(|slug| pin.source.contains(slug))),
            );
        }
    }
    pins
}

/// The repository's name as it appears in a dependency line, e.g. `sandbox-runner`.
pub fn repo_slug(entry: &RepoEntry) -> Option<String> {
    let last = entry.remote(Role::Origin).rsplit('/').next()?;
    let trimmed = last.trim_end_matches(".git");
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[derive(Debug, Default)]
pub struct Plan {
    pub repo: String,
    pub release: Option<String>,
    pub parents: Vec<(CommitId, Vec<String>)>,
    pub stale: Vec<Finding>,
    pub pins: Vec<Pin>,
    /// Informational: something worth saying that is not a failure.
    pub notes: Vec<String>,
    /// Could not answer. These, and only these, make the command exit non-zero
    /// for incompleteness. Keying on every note instead would make a routine
    /// remark like "14 superseded releases not scanned" look like a failure.
    pub problems: Vec<String>,
}

impl Plan {}

/// Everything we carry: the current tip of each of our branches.
///
/// A fresh cut is a flat merge of the upstream trunk and exactly these. Explicit commit
/// ids are read here, once, so a branch moving mid-cut cannot change what gets merged.
pub fn carried_branches(repo: &Repo) -> anyhow::Result<Vec<(String, CommitId)>> {
    let tips = repo.bookmark_tips()?;
    Ok(tips
        .iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch)
                if !branch.as_str().starts_with("release/") && branch.as_str() != "main" =>
            {
                Some((branch.to_string(), commit.clone()))
            }
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        })
        .collect())
}

/// Whether the release in hand already contains the upstream trunk.
///
/// Stated as a fact, not a instruction: a release that does not contain the current trunk
/// is a normal thing to have, and whether to move it is a judgment. It matters when a
/// pull request has merged upstream, because until the release contains the commit that
/// merge landed in, dropping the local branch removes the change from the release too.
/// `knives release rebase` is the operation; this only says where things stand.
pub fn trunk_lag(repo: &Repo, release: Option<&str>) -> Option<String> {
    let trunk = repo.resolve_commit(UPSTREAM_TRUNK).ok()?;
    let release = release?;
    let parents = repo.parents_of(release).ok()?;
    if parents.iter().any(|parent| parent.commit == trunk) {
        return None;
    }
    Some(format!(
        "{release} does not contain the upstream trunk ({})",
        &trunk.as_str()[..12.min(trunk.as_str().len())]
    ))
}

pub fn plan(name: &RepoName, entry: &RepoEntry, consumers: &[PathBuf]) -> anyhow::Result<Plan> {
    let mut plan = Plan {
        repo: name.to_string(),
        ..Plan::default()
    };
    let repo = Repo::open(&entry.path)?;
    let tips = repo.bookmark_tips()?;

    // The newest release we cut. Historical ones are frozen and not our concern.
    let newest = tips
        .iter()
        .filter(|(reference, _)| reference.branch().as_str().starts_with("release/"))
        .filter(|(reference, _)| match reference {
            BookmarkRef::Local(_) => true,
            BookmarkRef::Remote { remote, .. } => matches!(remote.as_str(), "origin" | "release"),
        })
        // The same ordering `status` uses. These two commands answering "which
        // is the current release?" differently was a real divergence.
        .max_by_key(|(reference, _)| {
            (
                crate::commands::status::release_order(reference.branch().as_str()),
                // On a tie prefer the local ref, deterministically. `max_by_key`
                // otherwise returns whichever came last in iteration order.
                u8::from(reference.is_local()),
            )
        });

    let Some((reference, commit)) = newest else {
        plan.notes
            .push("no dated release found; the first cut has nothing to repair".to_owned());
        return Ok(plan);
    };
    plan.release = Some(reference.to_string());

    let parents = repo.parents_of(commit.as_str())?;
    plan.stale = stale_parents(&parents, &tips);
    plan.parents = parents
        .into_iter()
        .map(|parent| {
            let names = parent.bookmarks.iter().map(ToString::to_string).collect();
            (parent.commit, names)
        })
        .collect();

    if consumers.is_empty() {
        // The command's central question. Unanswered is not success.
        plan.problems.push(
            "no consumers recorded, so pinned-ness is unknown; add `consumers = [...]` to \
             the registry entry, or pass --consumer"
                .to_owned(),
        );
    }
    // Every consumer, not one: they can sit on different releases, so a plan that saw only
    // the first would call a release unpinned while something else was frozen on it.
    let slug = repo_slug(entry);
    for consumer in consumers {
        plan.pins
            .extend(scan_consumer_for(consumer, slug.as_deref()));
    }
    Ok(plan)
}

pub fn render(plan: &Plan) -> String {
    let mut lines: Vec<String> = plan
        .problems
        .iter()
        .map(|problem| format!("!! {problem}"))
        .chain(plan.notes.iter().map(|note| format!("! {note}")))
        .collect();
    let Some(release) = &plan.release else {
        return lines.join("\n");
    };

    lines.push(format!("{}: {release}", plan.repo));
    lines.push(format!("  {} parent(s), flat", plan.parents.len()));
    for (commit, names) in &plan.parents {
        let short: String = commit.as_str().chars().take(12).collect();
        let held = if names.is_empty() {
            "no bookmark".to_owned()
        } else {
            names.join(", ")
        };
        lines.push(format!("    {short}  {held}"));
    }

    if plan.stale.is_empty() {
        lines.push("  every parent is still its branch tip".to_owned());
    } else {
        lines.push(format!("  {} stale parent(s):", plan.stale.len()));
        for finding in &plan.stale {
            lines.push(format!("    {}", finding.detail));
        }
    }

    lines.push("  pinned by:".to_owned());
    lines.push(crate::pins::render(&plan.pins));
    lines.push(match repair_effect(&plan.pins) {
        RepairEffect::RepairInPlace => {
            "  at least one consumer follows the branch: repair in place, no new dated name"
                .to_owned()
        }
        RepairEffect::NewDatedName => {
            "  every pin is frozen: the next cut needs a new dated suffix".to_owned()
        }
        RepairEffect::Unpinned => "  nothing pins this release: either is safe".to_owned(),
    });
    lines.push(
        "  planning by default. `knives release cut <name>` cuts a flat release from \
           the branches stated as members, or every branch when none is stated, and \
           verifies the parent count. Nothing here ever pushes."
            .to_owned(),
    );
    lines.join("\n")
}

/// Findings mean act; notes mean we could not answer. A command that reports a
/// problem in its text and still exits zero lets a CI gate go green on a broken
/// forge login or an unopenable repository.
pub const fn exit_for(plan: &Plan) -> Exit {
    if !plan.problems.is_empty() {
        return Exit::Incomplete;
    }
    if plan.stale.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn pin(kind: PinKind) -> Pin {
        Pin {
            file: "pyproject.toml".to_owned(),
            line: 1,
            reference: "release/2026-07-28".to_owned(),
            kind,
            source: String::new(),
        }
    }

    #[test]
    fn one_following_consumer_means_repair_in_place() {
        // A needless dated name burns the name and forces a re-pin nobody wanted.
        let pins = vec![pin(PinKind::Frozen), pin(PinKind::Follows)];
        assert_eq!(repair_effect(&pins), RepairEffect::RepairInPlace);
    }

    #[test]
    fn all_frozen_means_a_new_dated_name() {
        assert_eq!(
            repair_effect(&[pin(PinKind::Frozen)]),
            RepairEffect::NewDatedName
        );
    }

    #[test]
    fn nothing_pinning_it_leaves_the_choice_open() {
        assert_eq!(repair_effect(&[]), RepairEffect::Unpinned);
    }
}

/// A cut that has been checked but not yet made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    pub name: String,
    pub parents: Vec<CommitId>,
    /// Which pull ref each parent came from. Records provenance and pins
    /// nothing: a jj octopus's parents are already specific commits.
    pub provenance: Vec<(CommitId, String)>,
}

impl Cut {
    /// The message a cut carries, provenance included.
    pub fn message(&self) -> String {
        let mut lines = vec![format!("release: {}", self.name), String::new()];
        for (commit, source) in &self.provenance {
            lines.push(format!("parent {} from {source}", commit.as_str()));
        }
        lines.join("\n")
    }
}

/// Workspaces belonging to branches this cut has dropped.
///
/// They are cheap to create, which is why they accumulate; nothing else reaps
/// them.
pub fn workspaces_to_clean(workspaces: &[String], carried: &[String]) -> Vec<String> {
    let kept: Vec<String> = carried.iter().map(|b| b.replace('/', "-")).collect();
    workspaces
        .iter()
        .filter(|name| name.as_str() != "default" && !kept.contains(name))
        .cloned()
        .collect()
}

/// Make the cut, after checking it.
///
/// Refuses if the merge did not come out with exactly the parents asked for.
/// That check runs BEFORE anything could be pushed, because the failure it
/// catches is silent: a cut that dropped a parent looks exactly like one that
/// did not, until work goes missing downstream.
///
/// This never pushes. Publishing a release is a separate, deliberate act.
pub fn cut(repo: &Path, request: &Cut) -> anyhow::Result<CommitId> {
    let created = crate::jj::create_merge(repo, &request.parents, &request.message())?;
    let actual = Repo::open(repo)?.parents_of(created.as_str())?;
    anyhow::ensure!(
        actual.len() == request.parents.len(),
        "cut {} came out with {} parents, expected {}; refusing to name it",
        request.name,
        actual.len(),
        request.parents.len()
    );
    crate::jj::set_bookmark(repo, &request.name, created.as_str())?;
    Ok(created)
}

#[cfg(test)]
mod cut_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn sample() -> Cut {
        Cut {
            name: "release/2026-07-30".to_owned(),
            parents: vec![CommitId::new("aaaa"), CommitId::new("bbbb")],
            provenance: vec![
                (CommitId::new("aaaa"), "pull/10/head".to_owned()),
                (CommitId::new("bbbb"), "feat/beta".to_owned()),
            ],
        }
    }

    #[test]
    fn the_message_records_where_every_parent_came_from() {
        // So a later sync knows what to check, including for a parent that was
        // never our branch.
        let message = sample().message();
        assert!(message.contains("release/2026-07-30"));
        assert!(message.contains("parent aaaa from pull/10/head"));
        assert!(message.contains("parent bbbb from feat/beta"));
    }

    #[test]
    fn workspaces_for_dropped_branches_are_identified() {
        let workspaces = vec![
            "default".to_owned(),
            "feat-alpha".to_owned(),
            "feat-beta".to_owned(),
        ];
        let carried = vec!["feat/beta".to_owned()];
        assert_eq!(
            workspaces_to_clean(&workspaces, &carried),
            vec!["feat-alpha".to_owned()]
        );
    }

    #[test]
    fn the_default_workspace_is_never_reaped() {
        // Reaping it would delete the checkout the operator is standing in.
        let workspaces = vec!["default".to_owned()];
        assert!(workspaces_to_clean(&workspaces, &[]).is_empty());
    }
}

/// Whether a cut kept at least as many tests as a single contributing branch.
///
/// A total below one branch's own count means the merge dropped that branch's
/// tests, which is silent: everything still compiles and the suite still passes,
/// just with less in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestCountCheck {
    NotConfigured,
    Kept { merged: u32, branch: u32 },
    Dropped { merged: u32, branch: u32 },
}

impl TestCountCheck {
    pub const fn compare(merged: u32, branch: u32) -> Self {
        if merged < branch {
            Self::Dropped { merged, branch }
        } else {
            Self::Kept { merged, branch }
        }
    }

    pub fn render(self) -> String {
        match self {
            Self::NotConfigured => {
                "  test count: not configured for this repo, so not checked".to_owned()
            }
            Self::Kept { merged, branch } => {
                format!("  test count: {merged} merged against {branch} on one branch, kept")
            }
            Self::Dropped { merged, branch } => format!(
                "  test count: {merged} merged is BELOW {branch} on a single branch, \
                 so the cut dropped that branch's tests"
            ),
        }
    }
}

/// Extract a count from a test command's output: the last number it prints.
pub fn parse_test_count(output: &str) -> Option<u32> {
    output
        .split(|c: char| !c.is_ascii_digit())
        .rfind(|piece| !piece.is_empty())
        .and_then(|piece| piece.parse().ok())
}

#[cfg(test)]
mod test_count_tests {
    use super::*;

    #[test]
    fn a_merged_total_below_one_branch_is_a_dropped_suite() {
        // Silent failure: everything compiles, the suite passes, there is just
        // less of it.
        assert_eq!(
            TestCountCheck::compare(40, 55),
            TestCountCheck::Dropped {
                merged: 40,
                branch: 55
            }
        );
        assert!(TestCountCheck::compare(40, 55).render().contains("BELOW"));
    }

    #[test]
    fn an_equal_or_greater_total_is_kept() {
        assert!(matches!(
            TestCountCheck::compare(55, 55),
            TestCountCheck::Kept { .. }
        ));
        assert!(matches!(
            TestCountCheck::compare(90, 55),
            TestCountCheck::Kept { .. }
        ));
    }

    #[test]
    fn an_unconfigured_check_never_renders_as_passed() {
        // Not looking must not read as looking and finding nothing wrong.
        let text = TestCountCheck::NotConfigured.render();
        assert!(text.contains("not checked"));
        assert!(!text.contains("kept"));
    }

    #[test]
    fn a_count_is_read_from_the_last_number_a_runner_prints() {
        assert_eq!(
            parse_test_count("test result: ok. 115 passed; 0 failed"),
            Some(0)
        );
        assert_eq!(parse_test_count("Ran 19 tests"), Some(19));
        assert_eq!(parse_test_count("no numbers here"), None);
    }
}

/// Did the cut keep at least as many tests as one contributing branch?
///
/// Runs the repo's configured test command at the cut and at one parent, in
/// throwaway workspaces. Absent configuration reports "not checked", never
/// "passed": counting tests has no portable form, so the command is per repo.
pub fn check_test_count(
    repo: &Path,
    entry: &RepoEntry,
    cut: &CommitId,
    parent: &CommitId,
) -> TestCountCheck {
    let Some(command) = entry.test_count_command.as_deref() else {
        return TestCountCheck::NotConfigured;
    };
    let merged = crate::jj::output_at_revision(repo, cut.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    let single = crate::jj::output_at_revision(repo, parent.as_str(), command)
        .ok()
        .and_then(|out| parse_test_count(&out));
    match (merged, single) {
        (Some(merged), Some(single)) => TestCountCheck::compare(merged, single),
        _ => TestCountCheck::NotConfigured,
    }
}

/// What a cut's conflicts mean, and what to do about them.
///
/// Reported, never auto-resolved. Independent branches that each append a config
/// key land in the same regions, so a real cut carries real conflicts: one
/// ten-parent cut had a four-sided conflict in one file and a three-sided one in
/// another. Resolving those correctly is a semantic judgement about the config,
/// which a tool cannot make. Saying exactly where they are, and what shape the
/// resolution takes, is the part a tool can do.
pub fn conflict_guidance(files: &[String]) -> String {
    if files.is_empty() {
        return "  no conflicts in this cut".to_owned();
    }
    let mut lines = vec![format!(
        "  {} conflicted file(s), which is expected:",
        files.len()
    )];
    lines.extend(files.iter().map(|file| format!("    {file}")));
    lines.push(
        "  resolve as a union: keep every branch's addition, keep any shared helper \
         defined exactly once, and land each config key in every loader"
            .to_owned(),
    );
    lines.push("  then re-check the parent count and the test count before publishing".to_owned());
    lines.join("\n")
}

#[cfg(test)]
mod consumer_scope_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    #[test]
    fn a_siblings_pin_does_not_answer_this_repos_question() {
        // These forks cut releases on one dated scheme, so the same release name exists
        // in several of them. Unscoped, a sibling's pin was attributed to this repo,
        // which reads as "pinned at the newest cut" when this repo is not pinned at all.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("uv.lock"),
            "source = { git = \"https://forge.invalid/org/sandbox-runner.git?rev=release/2026-07-20\" }\n\
             source = { git = \"https://forge.invalid/org/sandbox-tools.git?rev=release/2026-07-28.2\" }\n",
        )
        .unwrap();

        let ours = scan_consumer_for(dir.path(), Some("sandbox-runner"));
        assert_eq!(ours.len(), 1, "only our own pin: {ours:?}");
        assert_eq!(ours[0].reference, "release/2026-07-20");

        let unscoped = scan_consumer_for(dir.path(), None);
        assert_eq!(unscoped.len(), 2, "without a slug, every pin is kept");
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    fn a_clean_cut_says_so_rather_than_staying_silent() {
        assert!(conflict_guidance(&[]).contains("no conflicts"));
    }

    #[test]
    fn conflicts_are_named_with_the_shape_of_their_resolution() {
        // Resolving them correctly is a semantic judgement about the config,
        // which a tool cannot make. Saying where they are, and that the shape is
        // a union with one shared helper and the key in every loader, is the
        // part a tool can do.
        let text = conflict_guidance(&["infra/lib/config.py".to_owned()]);
        assert!(text.contains("infra/lib/config.py"));
        assert!(text.contains("union"));
        assert!(text.contains("every loader"));
    }
}
