use crate::commands::status::Report;
use crate::config::RepoEntry;
use crate::detect::{BookmarkTips, Finding, Subject};
use crate::ids::{BookmarkRef, CommitId, ReleaseScheme, is_our_release};
use crate::jj::Repo;
use crate::release_model::{
    BranchSuccessions, carried_from_tips, double_cut_findings, release_order, trunk_positions,
};

/// Which releases are worth checking for stale parents.
///
/// Not all of them. A fork accumulates every dated release it ever cut, and
/// those are frozen history: reporting stale parents on a release from ten days
/// ago is noise that buries the one finding that matters. Scanning a real
/// repository unfiltered produced twenty releases and forty-nine findings.
///
/// The rule: every local release bookmark, because those are the ones we can
/// re-cut, plus the newest remote one, because that is what a consumer is
/// plausibly pinning. Dated names sort correctly as strings. `@git` refs are
/// excluded outright: they are jj's internal git-tracking view, not a remote.
/// The count of what was skipped is reported rather than silently dropped.
/// Under `Fixed` this is instead exactly the local branch and its publish-remote counterpart: there is no accumulated history to skip, so nothing is superseded.
pub(super) fn releases_to_scan(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> (Vec<(BookmarkRef, CommitId)>, usize) {
    match scheme {
        ReleaseScheme::Dated => {
            let all: Vec<(&BookmarkRef, &CommitId)> = tips
                .iter()
                .filter(|(reference, _)| is_our_release(reference, scheme, publish_remote))
                .collect();

            let newest = |local: bool| {
                all.iter()
                    .filter(|(reference, _)| reference.is_local() == local)
                    .max_by_key(|(reference, _)| release_order(reference.branch().as_str()))
                    .map(|(reference, _)| (*reference).clone())
            };
            // Only the newest cut on each side. Every local release a fork ever cut used to
            // be scanned, and their parents have all moved on by definition, so the report
            // filled with stale-parent findings for releases nothing pins: 47 of 89 in a real
            // repository, nearly all against cuts a fortnight old. The remedy attached to a stale
            // parent is to re-cut the release onto current tips, which is right for the release in
            // use and wrong for frozen history, where the answer is to forget it.
            let newest_local = newest(true);
            let newest_remote = newest(false);

            let chosen: Vec<(BookmarkRef, CommitId)> = all
                .iter()
                .filter(|(reference, _)| {
                    newest_local.as_ref() == Some(*reference)
                        || newest_remote.as_ref() == Some(*reference)
                })
                .map(|(reference, commit)| ((*reference).clone(), (*commit).clone()))
                .collect();

            let skipped = all.len() - chosen.len();
            (chosen, skipped)
        }
        ReleaseScheme::Fixed(branch) => {
            // Fixed releases advance in place, so only their local and published positions matter.
            let references = [
                BookmarkRef::Local(branch.clone()),
                BookmarkRef::Remote {
                    branch: branch.clone(),
                    remote: crate::ids::RemoteName::new(publish_remote),
                },
            ];
            let chosen = references
                .into_iter()
                .filter_map(|reference| {
                    tips.get(&reference)
                        .cloned()
                        .map(|commit| (reference, commit))
                })
                .collect();
            (chosen, 0)
        }
    }
}

struct ReleaseScan<'a> {
    tips: &'a BookmarkTips,
    scheme: &'a ReleaseScheme,
    publish_remote: &'a str,
    trunk: &'a str,
    /// Every known trunk position: what a branch's own changes are measured
    /// past when a stale parent is matched to its branch.
    trunks: &'a [CommitId],
}

/// Which releases were scanned, what was found, and how many were skipped.
///
/// Extracted from `gather` because that function had grown past what one
/// reviewer can hold at once, not to be reused.
fn scan_releases(
    repo: &Repo,
    input: &ReleaseScan<'_>,
) -> anyhow::Result<(Vec<String>, Vec<Finding>, usize)> {
    let (releases, skipped) = releases_to_scan(input.tips, input.scheme, input.publish_remote);
    let mut names = Vec::new();
    let mut findings = Vec::new();
    // Say where the branch went, not just that nothing points at the parent.
    // `parents_of` only reports bookmarks pointing AT a parent, so the pure
    // detector can never produce the "feat/x is now <id>" payload. Matched by
    // succession — ancestry or change id — so a member rebased onto the newer
    // trunk is named as itself, with the verb that moves the member.
    let branches = carried_from_tips(input.tips, input.trunk, input.scheme);
    let successions = BranchSuccessions::of(repo, input.trunks, &branches)?;
    for (release, commit) in &releases {
        names.push(release.to_string());
        // Resolve by commit id, never by the bookmark's display form. A remote
        // bookmark rendered `name@remote` is not reliably resolvable as a
        // revset, and the tip map already carries the commit.
        let mut stale =
            crate::detect::stale_parents(&repo.parents_of(commit.as_str())?, input.tips);
        for finding in &mut stale {
            let Subject::Commit(parent) = finding.subject.clone() else {
                continue;
            };
            let moved = successions.successors_of(&parent)?;
            if moved.is_empty() {
                continue;
            }
            let where_now = moved
                .iter()
                .map(|(branch, tip)| {
                    format!(
                        "{branch} is now {}; `knives release advance {branch}` moves the member",
                        super::short(tip.as_str())
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            finding.detail = format!(
                "parent {} is no longer the tip of its branch ({where_now})",
                super::short(parent.as_str())
            );
        }
        findings.extend(stale);
    }
    Ok((names, findings, skipped))
}

#[derive(Clone, Copy)]
pub(super) struct ReleaseInput<'a> {
    pub(super) repo: &'a Repo,
    pub(super) tips: &'a BookmarkTips,
    pub(super) entry: &'a RepoEntry,
}

/// Fold the release scan into a report.
///
/// Extracted from `gather` for the same reason `scan_releases` was: that function
/// sits within a few lines of the file's hundred-line limit, and the breadcrumb
/// adds to it.
pub(super) fn add_releases(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    input: ReleaseInput<'_>,
) -> anyhow::Result<()> {
    let ReleaseInput { repo, tips, entry } = input;
    let scheme = entry.release_scheme();
    report.newest_release =
        crate::release_model::newest_release(tips, &scheme, entry.publish_remote())
            .map(|(reference, _)| reference.to_string());
    let trunks = trunk_positions(repo, entry)?;
    let (names, release_findings, skipped) = scan_releases(
        repo,
        &ReleaseScan {
            tips,
            scheme: &scheme,
            publish_remote: entry.publish_remote(),
            trunk: entry.trunk(),
            trunks: &trunks,
        },
    )?;
    report.releases = names;
    findings.extend(release_findings);
    let (double_cut_findings, double_cut_notes) =
        double_cut_findings(&entry.path, tips, &scheme, entry.publish_remote())?;
    findings.extend(double_cut_findings);
    report.notes.extend(double_cut_notes);
    if skipped > 0 {
        report
            .notes
            .push(format!("{skipped} superseded release(s) not scanned"));
    }
    Ok(())
}
