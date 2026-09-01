use super::{
    BTreeMap, BTreeSet, BookmarkRef, BookmarkTips, BranchName, BranchTarget, CommitId, Finding,
    FindingKind, Forge, JjError, LandedVerdict, Options, OriginRelation, Repo, RepoEntry, RepoName,
    Role, Store, Subject, classify_landed, divergent_changes, double_checkout, index_pulls,
    probe_landed, short,
};

use super::rows::pull_summary_for;
/// Inputs for a landed probe.
#[derive(Clone, Copy)]
pub(super) struct LandedInput<'a, 'forge> {
    path: &'a std::path::Path,
    branch: &'a BranchName,
    tips: (&'a CommitId, Option<&'a CommitId>),
    options: &'a Options<'forge>,
    upstream_trunk: &'a str,
}

/// Whether the branch has landed upstream, or that the question cannot be answered.
///
/// Judge only what the pull request actually contains. The probe replays the local
/// bookmark, so when local and origin disagree it answers about content nobody has
/// pushed, and stale content replays clean and reads as landed. Refusing to judge is
/// cheap; the `landed` advice is to delete the branch and its release parent.
pub(super) fn landed_verdict(input: LandedInput<'_, '_>) -> Result<Option<LandedVerdict>, JjError> {
    let LandedInput {
        path,
        branch,
        tips,
        options,
        upstream_trunk,
    } = input;
    let (tip, origin_tip) = tips;
    if !options.probe {
        return Ok(None);
    }
    if origin_tip.is_some_and(|origin| origin != tip) {
        return Ok(Some(LandedVerdict::Unjudged));
    }
    Ok(Some(classify_landed(probe_landed(
        path,
        branch,
        upstream_trunk,
    )?)))
}

/// The branch data needed by landed probes. Pull request discovery deliberately
/// stays out so the probes can overlap the forge phase.
pub(super) struct ProbeInput {
    pub(super) branch: BranchName,
    pub(super) tip: CommitId,
    /// `None` when the branch has no origin counterpart to compare against.
    pub(super) origin_tip: Option<CommitId>,
}

pub(super) struct ProbeResult {
    verdict: Result<Option<LandedVerdict>, JjError>,
    landed: Option<(String, LandedVerdict)>,
}

pub(super) struct ProbePhase {
    pub(super) verdicts: Vec<Result<Option<LandedVerdict>, JjError>>,
    pub(super) landed: BTreeMap<String, LandedVerdict>,
    pub(super) duration: std::time::Duration,
}

impl ProbePhase {
    pub(super) fn panicked(inputs: &[ProbeInput]) -> Self {
        Self {
            verdicts: inputs
                .iter()
                .map(|input| {
                    Err(JjError::ProbePanic {
                        branch: input.branch.to_string(),
                    })
                })
                .collect(),
            landed: BTreeMap::new(),
            duration: std::time::Duration::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ProbeContext<'a, 'forge, 'opened_ref, 'opened> {
    path: &'a std::path::Path,
    options: &'a Options<'forge>,
    upstream_trunk: &'a str,
    opened: Option<&'opened_ref crate::snapshot::Opened<'opened>>,
    trunk_commit: Option<&'a CommitId>,
}

pub(super) fn probe_one(input: &ProbeInput, context: &ProbeContext<'_, '_, '_, '_>) -> ProbeResult {
    let ProbeContext {
        path,
        options,
        upstream_trunk,
        opened,
        trunk_commit,
    } = *context;
    let can_cache = options.probe
        && input
            .origin_tip
            .as_ref()
            .is_none_or(|origin_tip| origin_tip == &input.tip);
    let key = can_cache
        .then(|| trunk_commit.map(|trunk| crate::forge_cache::landed_key(&input.tip, trunk)))
        .flatten();
    if let Some((key, verdict)) = key.as_ref().and_then(|key| {
        opened
            .and_then(|opened| opened.landed_cached(key))
            .map(|verdict| (key.clone(), verdict))
    }) {
        return ProbeResult {
            verdict: Ok(Some(verdict)),
            landed: Some((key, verdict)),
        };
    }

    let verdict = landed_verdict(LandedInput {
        path,
        branch: &input.branch,
        tips: (&input.tip, input.origin_tip.as_ref()),
        options,
        upstream_trunk,
    });
    let landed = match (&key, &verdict) {
        (Some(key), Ok(Some(verdict))) if *verdict != LandedVerdict::Unjudged => {
            Some((key.clone(), *verdict))
        }
        _ => None,
    };
    ProbeResult { verdict, landed }
}

/// Landed verdicts for every branch, probed concurrently in branch order, with
/// successful replay answers merged into the cache section.
pub(super) fn probe_phase(
    context: ProbeContext<'_, '_, '_, '_>,
    inputs: &[ProbeInput],
) -> ProbePhase {
    let ProbeContext { options, .. } = context;
    let started = std::time::Instant::now();
    if !options.probe || inputs.is_empty() {
        return ProbePhase {
            verdicts: inputs.iter().map(|_| Ok(None)).collect(),
            landed: BTreeMap::new(),
            duration: started.elapsed(),
        };
    }

    let workers = options.workers.clamp(1, inputs.len());
    let chunk = inputs.len().div_ceil(workers);
    let results = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for slice in inputs.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|input| probe_one(input, &context))
                        .collect::<Vec<_>>()
                }),
            ));
        }
        handles
            .into_iter()
            .flat_map(|(slice, handle)| {
                handle.join().unwrap_or_else(|_| {
                    slice
                        .iter()
                        .map(|input| ProbeResult {
                            verdict: Err(JjError::ProbePanic {
                                branch: input.branch.to_string(),
                            }),
                            landed: None,
                        })
                        .collect()
                })
            })
            .collect::<Vec<_>>()
    });

    let mut landed = BTreeMap::new();
    let verdicts = results
        .into_iter()
        .map(|result| {
            if let Some((key, verdict)) = result.landed {
                let _ = landed.insert(key, verdict);
            }
            result.verdict
        })
        .collect();
    ProbePhase {
        verdicts,
        landed,
        duration: started.elapsed(),
    }
}
/// Divergent bookmarks, named individually.
///
/// A divergent branch's bookmark is conflicted, so it has no single tip and never
/// appears in the branch list. Naming it here is the difference between "some change
/// is divergent" and "this branch of yours is".
pub(super) fn conflicted_bookmark_findings(repo: &Repo) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for (reference, commits) in repo.conflicted_bookmarks()? {
        let shown: Vec<String> = commits.iter().map(|c| short(c.as_str())).collect();
        findings.push(Finding::new(
            FindingKind::Divergence,
            Subject::Bookmark(reference.clone()),
            format!(
                "bookmark {reference} points at {} commits ({}), so it has no single tip",
                commits.len(),
                shown.join(", ")
            ),
        ));
    }
    Ok(findings)
}
/// Every number declared before discovery: stated pull requests and same-repository
/// dependencies for both maintained and divergent branches.
pub(super) fn declared_numbers(
    repo: &RepoName,
    branches: &[BranchName],
    store: &Store,
) -> Vec<u64> {
    let mut numbers = BTreeSet::new();
    for branch in branches {
        let target = BranchTarget::new(repo.clone(), branch.clone());
        if let Some(number) = store.tracked_pull(&target) {
            let _ = numbers.insert(number);
        }
        numbers.extend(
            store
                .dependencies(&target)
                .into_iter()
                .filter(|requirement| requirement.repo == *repo)
                .map(|requirement| requirement.number),
        );
    }
    numbers.into_iter().collect()
}

pub(super) struct ForgePhase<'snapshot> {
    pub(super) snapshot: Option<crate::snapshot::CompletedSnapshot<'snapshot>>,
    pub(super) duration: std::time::Duration,
    pub(super) problems: Vec<String>,
}

fn select_status_numbers(
    discovery: &crate::snapshot::Discovery<'_>,
    context: &(&[BranchName], &[u64]),
) -> Vec<u64> {
    let (branches, extra_numbers) = *context;
    let discovery_index = index_pulls(&discovery.ours());
    let mut surfaced: Vec<u64> = branches
        .iter()
        .filter_map(|branch| pull_summary_for(branch, &discovery_index.by_branch))
        .map(|pull| pull.number)
        .collect();
    surfaced.extend_from_slice(extra_numbers);
    surfaced
}

pub(super) fn forge_phase<'snapshot>(
    opened: Option<&'snapshot crate::snapshot::Opened<'snapshot>>,
    branches: &[BranchName],
    extra_numbers: &[u64],
    started: std::time::Instant,
) -> ForgePhase<'snapshot> {
    let Some(opened) = opened else {
        return ForgePhase {
            snapshot: None,
            duration: started.elapsed(),
            problems: Vec::new(),
        };
    };
    match opened.complete_with(&(branches, extra_numbers), select_status_numbers) {
        Ok(snapshot) => {
            let problems = branches
                .iter()
                .filter_map(|branch| {
                    let summary = pull_summary_for(branch, &snapshot.index().by_branch)?;
                    snapshot.fact(summary.number).is_none().then(|| {
                        format!(
                            "pull request #{} for {branch} unavailable: the forge did not report it",
                            summary.number
                        )
                    })
                })
                .collect();
            ForgePhase {
                snapshot: Some(snapshot),
                duration: started.elapsed(),
                problems,
            }
        }
        Err(error) => ForgePhase {
            snapshot: None,
            duration: started.elapsed(),
            problems: vec![format!("pull request state unavailable: {error}")],
        },
    }
}
/// Every maintained branch paired with the data the landed probes need.
///
/// Pull request discovery is deliberately absent: a probe never reads it, and
/// separating these inputs lets the probe phase overlap forge discovery.
pub(super) fn probe_inputs(
    branches: Vec<(BranchName, CommitId)>,
    tips: &BookmarkTips,
) -> Vec<ProbeInput> {
    branches
        .into_iter()
        .map(|(branch, tip)| {
            let origin_tip = tips
                .get(&BookmarkRef::Remote {
                    branch: branch.clone(),
                    remote: crate::ids::RemoteName::new("origin"),
                })
                .cloned();
            ProbeInput {
                branch,
                tip,
                origin_tip,
            }
        })
        .collect()
}
/// How local relates to origin when that relation can be determined.
///
/// Two ancestry queries, not one: `is_ancestor(origin, tip)` returning false covers both
/// "origin is ahead" and "the histories forked", and reporting a fork as `(behind)` tells a
/// reader to trust origin over their own unpushed work. The second query separates them.
///
/// A failure comes back as an error rather than a relation, because a relation this could not
/// establish is not a fact about history — the same reason `LandedVerdict::Unjudged` exists.
pub fn relation_to_origin(
    repo: &Repo,
    tip: &CommitId,
    origin_tip: Option<&CommitId>,
) -> Result<Option<OriginRelation>, JjError> {
    match origin_tip {
        Some(origin) if origin != tip => {
            if repo.is_ancestor(origin, tip)? {
                Ok(Some(OriginRelation::Ahead))
            } else if repo.is_ancestor(tip, origin)? {
                Ok(Some(OriginRelation::Behind))
            } else {
                Ok(Some(OriginRelation::Diverged))
            }
        }
        Some(_) | None => Ok(None),
    }
}

pub(super) struct OriginPhase {
    pub(super) relations: Vec<Result<Option<OriginRelation>, String>>,
}

/// Relations to origin, queried concurrently with one jj-lib handle per worker.
///
/// Loaded repository handles are not assumed `Sync`. Each worker opens its own
/// handle, handles its chunk in order, and the join order restores branch order
/// before a result reaches the report.
pub(super) fn origin_phase(
    path: &std::path::Path,
    inputs: &[ProbeInput],
    workers: usize,
) -> OriginPhase {
    if inputs.is_empty() {
        return OriginPhase {
            relations: Vec::new(),
        };
    }
    let workers = workers.clamp(1, inputs.len());
    let chunk = inputs.len().div_ceil(workers);
    let relations = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for slice in inputs.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || match Repo::open(path) {
                    Ok(repo) => slice
                        .iter()
                        .map(|input| {
                            relation_to_origin(&repo, &input.tip, input.origin_tip.as_ref())
                                .map_err(|error| error.to_string())
                        })
                        .collect::<Vec<Result<Option<OriginRelation>, String>>>(),
                    Err(error) => {
                        let error = error.to_string();
                        slice
                            .iter()
                            .map(|_| Err(error.clone()))
                            .collect::<Vec<Result<Option<OriginRelation>, String>>>()
                    }
                }),
            ));
        }
        handles
            .into_iter()
            .flat_map(|(slice, handle)| {
                handle.join().unwrap_or_else(|_| {
                    slice
                        .iter()
                        .map(|_| Err("origin relation task panicked".to_owned()))
                        .collect()
                })
            })
            .collect()
    });
    OriginPhase { relations }
}

/// Repository-wide health facts prepared on an independent jj-lib handle.
///
/// The status rows use their own handle below. Keeping this result separate
/// lets its expensive divergent-change scan overlap forge discovery while the
/// report still prefixes its findings and problems exactly as before.
pub(super) struct RepositoryHealth {
    pub(super) findings: Vec<Finding>,
    pub(super) problems: Vec<String>,
    pub(super) health: std::time::Duration,
    pub(super) divergent_changes: std::time::Duration,
}

pub(super) fn repository_health(
    path: &std::path::Path,
    tips: &BookmarkTips,
    publish_remote: &str,
) -> anyhow::Result<RepositoryHealth> {
    let health_phase = std::time::Instant::now();
    let repo = Repo::open(path)?;
    let mut findings = Vec::new();
    let mut problems = Vec::new();
    if let Some(stale) = repo.stale_working_copy(path) {
        problems.push(stale);
    }
    findings.extend(double_checkout(&repo.workspaces()?));
    let ignored: std::collections::BTreeSet<crate::ids::BookmarkRef> =
        crate::commands::release::superseded_dated_releases(tips, publish_remote)
            .into_iter()
            .map(|(reference, _)| reference)
            .collect();
    let phase = std::time::Instant::now();
    let changes = repo.divergent_changes(&ignored)?;
    let divergent_duration = phase.elapsed();
    findings.extend(divergent_changes(&changes));
    findings.extend(conflicted_bookmark_findings(&repo)?);
    let health = health_phase.elapsed();
    Ok(RepositoryHealth {
        findings,
        problems,
        health,
        divergent_changes: divergent_duration,
    })
}
/// Resolve the single forge identity and cache read both concurrent phases share.
pub(super) fn open_forge_snapshot<'a>(
    forge: Option<&'a dyn Forge>,
    entry: &'a RepoEntry,
    cache_root: Option<&'a std::path::Path>,
) -> Result<Option<crate::snapshot::Opened<'a>>, crate::forge::ForgeError> {
    let Some(forge) = forge else {
        return Ok(None);
    };
    crate::snapshot::open(crate::snapshot::SnapshotConfig {
        forge,
        path: &entry.path,
        remotes: [entry.remote(Role::Origin), entry.remote(Role::Release)],
        cache_root,
    })
    .map(Some)
}

/// The completed concurrent phases, retained until report folding persists the cache.
pub(super) struct StatusPhases<'snapshot> {
    pub(super) forge: ForgePhase<'snapshot>,
    pub(super) probe: ProbePhase,
}

/// Inputs to the concurrent forge-discovery and landed-probe phases.
#[derive(Clone, Copy)]
pub(super) struct StatusPhaseInput<'a, 'forge, 'snapshot> {
    pub(super) entry: &'a RepoEntry,
    pub(super) options: &'a Options<'forge>,
    pub(super) probe_inputs: &'a [ProbeInput],
    pub(super) opened: Option<&'snapshot crate::snapshot::Opened<'snapshot>>,
    pub(super) trunk_commit: Option<&'a CommitId>,
    pub(super) branches: &'a [BranchName],
    pub(super) declared: &'a [u64],
    pub(super) forge_started: std::time::Instant,
}

/// Run independent forge discovery and landed probes without sharing report mutation.
pub(super) fn run_status_phases<'snapshot>(
    input: StatusPhaseInput<'_, '_, 'snapshot>,
) -> StatusPhases<'snapshot> {
    let StatusPhaseInput {
        entry,
        options,
        probe_inputs,
        opened,
        trunk_commit,
        branches,
        declared,
        forge_started,
    } = input;
    let upstream_trunk = entry.upstream_trunk();
    let context = ProbeContext {
        path: &entry.path,
        options,
        upstream_trunk: &upstream_trunk,
        opened,
        trunk_commit,
    };
    let (forge, probe) = std::thread::scope(|scope| {
        let probes = scope.spawn(|| probe_phase(context, probe_inputs));
        let forge = forge_phase(opened, branches, declared, forge_started);
        let probes = probes
            .join()
            .unwrap_or_else(|_| ProbePhase::panicked(probe_inputs));
        (forge, probes)
    });
    StatusPhases { forge, probe }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::super::Report;
    use super::super::rows::{RowInput, branch_rows};
    use super::super::unjudged_note;
    use super::*;
    use crate::forge::PullRequest;
    use crate::forge::fake::FakeForge;
    use std::collections::BTreeMap;
    use std::path::Path;
    fn select_numbers(_: &crate::snapshot::Discovery<'_>, numbers: &[u64]) -> Vec<u64> {
        numbers.to_vec()
    }

    #[test]
    fn status_uses_a_newly_open_primary_from_the_completed_snapshot() {
        // A warm cache can still attach a branch to a closed primary when a new
        // open pull request for that branch appeared after the cache watermark.
        let cache = tempfile::tempdir().expect("cache directory");
        let branch = BranchName::new("feat/alpha");
        let closed = PullRequest {
            number: 7,
            state: "CLOSED".to_owned(),
            head_ref_name: branch.to_string(),
            updated_at: "2026-08-01T00:00:00Z".to_owned(),
            ..PullRequest::default()
        };
        let seeded = FakeForge {
            pull_requests: BTreeMap::from([(branch.clone(), closed)]),
            ..FakeForge::default()
        };
        let seeded_opened = crate::snapshot::open(crate::snapshot::SnapshotConfig {
            forge: &seeded,
            path: Path::new("/fake"),
            remotes: ["origin", "release"],
            cache_root: Some(cache.path()),
        })
        .expect("open seed cache");
        seeded_opened
            .complete_with(&[7_u64][..], select_numbers)
            .expect("fetch closed pull request")
            .persist(None)
            .expect("persist closed pull request");

        let opened = FakeForge {
            pull_requests: BTreeMap::from([(
                branch.clone(),
                PullRequest {
                    number: 8,
                    state: "OPEN".to_owned(),
                    head_ref_name: branch.to_string(),
                    updated_at: "2026-08-02T00:00:00Z".to_owned(),
                    ..PullRequest::default()
                },
            )]),
            ..FakeForge::default()
        };
        let live_opened = crate::snapshot::open(crate::snapshot::SnapshotConfig {
            forge: &opened,
            path: Path::new("/fake"),
            remotes: ["origin", "release"],
            cache_root: Some(cache.path()),
        })
        .expect("open warm cache");

        let phase = forge_phase(
            Some(&live_opened),
            std::slice::from_ref(&branch),
            &[],
            std::time::Instant::now(),
        );

        let snapshot = phase.snapshot.as_ref().expect("the live batch completed");
        assert_eq!(
            snapshot.index().by_branch[&branch].number,
            8,
            "the current open pull request is primary"
        );
        let store = Store::open(cache.path().join("state.json")).expect("open state");
        let repo = RepoName::new("test-repo");
        let mut report = Report::default();
        branch_rows(
            RowInput {
                name: &repo,
                store: &store,
                probe_inputs: vec![ProbeInput {
                    branch,
                    tip: CommitId::new("test-commit"),
                    origin_tip: Some(CommitId::new("origin-commit")),
                }],
                verdicts: vec![Ok::<Option<LandedVerdict>, JjError>(None)],
                origin_relations: vec![Ok::<Option<OriginRelation>, String>(None)],
                index: snapshot.index(),
                snapshot: phase.snapshot.as_ref(),
                notches: &[],
                expected_base: "main",
            },
            &mut report,
            &mut Vec::new(),
        )
        .expect("assemble status report");

        assert_eq!(
            report.branches[0].pr.as_ref().map(|pull| pull.number),
            Some(8),
            "the report shows the current open pull request this run"
        );
    }

    #[test]
    fn a_branch_behind_origin_is_not_judged_against_the_trunk() {
        // The probe replays the local bookmark. When local is behind origin it replays
        // stale content, which comes back clean and reads as already in the trunk.
        // Observed against a real repository: two branches with open pull requests were
        // called landed while nothing of theirs was upstream.
        let note = unjudged_note(&["feat/alpha".to_owned(), "feat/beta".to_owned()]);
        let note = note.expect("two unjudgeable branches must be reported");
        assert!(note.contains("2 branches"), "was: {note}");
        assert!(note.contains("feat/alpha") && note.contains("feat/beta"));
        assert!(
            unjudged_note(&[]).is_none(),
            "nothing to say when all were judged"
        );
    }
}
