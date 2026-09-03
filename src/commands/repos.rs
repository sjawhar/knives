//! `knives repos`: what am I maintaining.
//!
//! Deliberately separate from `knives wip`, which answers what is being worked on
//! right now. Conflating the two was an earlier mistake in this design.
// allow: SIZE_OK: 1032 lines - release state, pin lag, gather and render for one report, with the pin-lag scenarios beside the private functions they exercise.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::cli::Exit;
use crate::config::{Registry, RepoEntry, Role, default_config_path, load};
use crate::consumer_pins::{ConsumerHeadMemo, ConsumerPinSource, scan_consumer_slug_with_heads};
use crate::ids::{BookmarkRef, BranchName, ReleaseScheme, RemoteName, is_our_release};
use crate::jj::Repo;
use crate::release_model::{release_order, repo_slug};

/// Cached release lookup for one repo, computed once and shared across every
/// consumer's pin-lag comparison for that repo.
#[derive(Debug)]
pub struct ReleaseState {
    pub newest: Option<String>,
    pub repo: Option<Repo>,
}

/// Selects the release reference that represents a repository's newest publishable state.
///
/// Fixed releases use only their local bookmark and publish-remote counterpart because
/// treating origin and release as interchangeable reported a non-publish position as newest.
fn newest_release(tips: &crate::detect::BookmarkTips, entry: &RepoEntry) -> Option<String> {
    let scheme = entry.release_scheme();
    match &scheme {
        ReleaseScheme::Dated => tips
            .keys()
            .filter(|reference| is_our_release(reference, &scheme, entry.publish_remote()))
            .max_by_key(|reference| release_order(reference.branch().as_str()))
            .map(ToString::to_string),
        ReleaseScheme::Fixed(branch) => {
            let local = BookmarkRef::Local(branch.clone());
            let published = BookmarkRef::Remote {
                branch: branch.clone(),
                remote: RemoteName::new(entry.publish_remote()),
            };
            if tips.contains_key(&local) {
                Some(local.to_string())
            } else {
                tips.contains_key(&published).then(|| published.to_string())
            }
        }
    }
}

fn release_state(registry: &Registry) -> BTreeMap<String, ReleaseState> {
    registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let repo = Repo::open(&entry.path).ok();
            let newest = repo
                .as_ref()
                .and_then(|repo| repo.bookmark_tips().ok())
                .and_then(|tips| newest_release(&tips, entry));
            (name.clone(), ReleaseState { newest, repo })
        })
        .collect()
}

/// How far behind a consumer's pin is from the newest release we cut.
///
/// The single most useful thing a sweep of these forks found was that the consumer
/// pinned releases older than our own cuts, across three repositories at once. Nobody
/// runs a command to answer a question they have not thought of, so this is reported
/// beside the release state rather than hidden behind a flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinLag {
    pub lag: Option<String>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

fn consumer_label(consumer: &str) -> String {
    consumer.to_owned()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the current release state and shared consumer-scan collaborators have independent owners and lifetimes"
)]
pub fn pin_lag(
    entry: &RepoEntry,
    newest: Option<&String>,
    repo: Option<&Repo>,
    forge: &dyn ConsumerPinSource,
    cache_root: Option<&Path>,
    heads: &ConsumerHeadMemo,
) -> PinLag {
    let scheme = entry.release_scheme();
    match &scheme {
        ReleaseScheme::Dated => dated_pin_lag(entry, newest, &scheme, forge, cache_root, heads),
        ReleaseScheme::Fixed(fixed) => {
            fixed_pin_lag(entry, repo, fixed, &scheme, forge, cache_root, heads)
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "dated pin lag needs a release comparison target and each shared scan collaborator independently"
)]
fn dated_pin_lag(
    entry: &RepoEntry,
    newest: Option<&String>,
    scheme: &ReleaseScheme,
    forge: &dyn ConsumerPinSource,
    cache_root: Option<&Path>,
    heads: &ConsumerHeadMemo,
) -> PinLag {
    if entry.consumers.is_empty() {
        return PinLag::default();
    }
    let Some(newest) = newest else {
        return PinLag::default();
    };
    // The newest release arrives qualified with the remote it was seen on, while a pin
    // names only the branch. Comparing those forms directly called every repo behind,
    // including ones pinned exactly at the newest cut.
    let newest_branch = newest.split('@').next().unwrap_or(newest);
    let slug = repo_slug(entry);
    // Reported per consumer, because they can sit on different releases: one consumer
    // being current says nothing about another, and collapsing them into a single verdict
    // hid exactly that.
    let mut behind = Vec::new();
    let mut notes = Vec::new();
    let mut problems = Vec::new();
    for consumer in &entry.consumers {
        let mut scan = scan_consumer_slug_with_heads(
            forge,
            cache_root,
            &entry.path,
            consumer,
            slug.as_deref(),
            scheme,
            heads,
        );
        notes.extend(std::mem::take(&mut scan.notes));
        if !scan.problems.is_empty() {
            problems.extend(scan.problems);
            continue;
        }
        // The listing answers "how far behind our releases"; a pin at a consumer's own
        // tag is off that axis and reads here exactly as pinning no release.
        scan.pins.retain(|pin| pin.on_scheme);
        let label = consumer_label(consumer);
        if scan.pins.is_empty() {
            behind.push(format!("{label} pins no release of this repo"));
            continue;
        }
        if scan.pins.iter().any(|pin| pin.reference == newest_branch) {
            continue;
        }
        let mut names: Vec<&str> = scan.pins.iter().map(|pin| pin.reference.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        behind.push(format!("{label} pins {}", names.join(", ")));
    }
    PinLag {
        lag: (!behind.is_empty())
            .then(|| format!("newest is {newest_branch}; {}", behind.join("; "))),
        notes,
        problems,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "fixed pin lag needs both fixed-release comparison inputs and each shared scan collaborator independently"
)]
fn fixed_pin_lag(
    entry: &RepoEntry,
    repo: Option<&Repo>,
    fixed: &BranchName,
    scheme: &ReleaseScheme,
    forge: &dyn ConsumerPinSource,
    cache_root: Option<&Path>,
    heads: &ConsumerHeadMemo,
) -> PinLag {
    let slug = repo_slug(entry);
    let local_tip = repo.and_then(|repo| {
        repo.bookmark_tips()
            .ok()
            .and_then(|tips| tips.get(&BookmarkRef::Local(fixed.clone())).cloned())
    });
    let mut behind = Vec::new();
    let mut notes = Vec::new();
    let mut problems = Vec::new();
    for consumer in &entry.consumers {
        let mut scan = scan_consumer_slug_with_heads(
            forge,
            cache_root,
            &entry.path,
            consumer,
            slug.as_deref(),
            scheme,
            heads,
        );
        notes.extend(std::mem::take(&mut scan.notes));
        if !scan.problems.is_empty() {
            problems.extend(scan.problems);
            continue;
        }
        scan.pins.retain(|pin| pin.on_scheme);
        let label = consumer_label(consumer);
        if scan.pins.is_empty() {
            behind.push(format!("{label} pins no release of this repo"));
            continue;
        }
        for pin in scan.pins {
            let Some(locked) = pin.locked else {
                continue;
            };
            let Some(repo) = repo else {
                notes.push(format!(
                    "could not compare {locked} with {fixed}: repository unavailable"
                ));
                continue;
            };
            let Some(tip) = local_tip.as_ref() else {
                notes.push(format!(
                    "could not compare {locked} with {fixed}: local branch unavailable"
                ));
                continue;
            };
            let Ok(locked_commit) = repo.resolve_commit(&locked) else {
                notes.push(format!(
                    "could not compare {locked} with {fixed}: commit unavailable"
                ));
                continue;
            };
            match repo.is_ancestor(&locked_commit, tip) {
                Ok(true) if locked_commit != *tip => {
                    behind.push(format!("{label} pins {fixed} at {locked}"));
                }
                Ok(_) => {}
                Err(_) => notes.push(format!("could not compare {locked} with {fixed}")),
            }
        }
    }
    PinLag {
        lag: (!behind.is_empty()).then(|| behind.join("; ")),
        notes,
        problems,
    }
}

/// What `knives repos` reports: per-repo release/consumer state, trusted mounts,
/// and any registry-level notes (where to add entries, or that pin state lives
/// in consumers).
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repos: Vec<RepoRow>,
    pub trusted: Vec<TrustedRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub config_path: String,
}

/// One maintained repository: its release position and how far its consumers lag it.
#[derive(Debug, serde::Serialize)]
pub struct RepoRow {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// A trusted-but-unmaintained mount: instructions are read, nothing here is
/// pinned or released.
#[derive(Debug, serde::Serialize)]
pub struct TrustedRow {
    pub name: String,
    pub path: String,
}

/// What one listing reads: the registry, each repository's release state, and
/// the consumer-scan collaborators every row shares.
pub struct GatherInput<'a> {
    pub registry: &'a Registry,
    pub releases: &'a BTreeMap<String, ReleaseState>,
    pub config_path: &'a Path,
    pub forge: &'a dyn ConsumerPinSource,
    pub cache_root: Option<&'a Path>,
    pub heads: &'a ConsumerHeadMemo,
}

impl std::fmt::Debug for GatherInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatherInput")
            .field("config_path", &self.config_path)
            .field("repos", &self.registry.repos.len())
            .finish_non_exhaustive()
    }
}

/// Collects release state and consumer pin lag for every maintained and
/// trusted registry entry.
pub fn gather(input: &GatherInput<'_>) -> Report {
    let GatherInput {
        registry,
        releases,
        config_path,
        forge,
        cache_root,
        heads,
    } = *input;
    let repos = registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let state = releases.get(name);
            let newest = state.and_then(|state| state.newest.as_ref());
            let pin_lag = pin_lag(
                entry,
                newest,
                state.and_then(|state| state.repo.as_ref()),
                forge,
                cache_root,
                heads,
            );
            RepoRow {
                name: name.clone(),
                path: entry.path.display().to_string(),
                release_remote: entry
                    .has_split_release()
                    .then(|| entry.remote(Role::Release).to_owned()),
                newest_release: newest.cloned(),
                behind: pin_lag.lag,
                notes: pin_lag.notes,
                problems: pin_lag.problems,
            }
        })
        .collect::<Vec<_>>();
    let trusted = registry
        .trusted
        .iter()
        .map(|(name, entry)| TrustedRow {
            name: name.clone(),
            path: entry.path.display().to_string(),
        })
        .collect::<Vec<_>>();

    // Saying where to add entries beats printing nothing and exiting zero; a
    // trusted-only registry is a real configuration, not an empty one, so that
    // note only fires when both sections are empty.
    let mut notes = Vec::new();
    if repos.is_empty() {
        if trusted.is_empty() {
            notes.push(format!(
                "no repos configured; add entries to {}",
                config_path.display()
            ));
        }
    } else {
        notes.push(
            "pin state lives in consumers: record them as `consumers = [...]` in the registry"
                .to_owned(),
        );
    }

    Report {
        repos,
        trusted,
        notes,
        config_path: config_path.display().to_string(),
    }
}

/// Trusted-but-unmaintained entries, listed apart from the forks.
///
/// Shown because a registry entry nothing ever prints is one nobody can debug:
/// these change what guidance an agent receives, so they have to be visible. Kept
/// under their own heading because they are not answers to "what am I
/// maintaining" — no fork command touches them.
fn trusted_lines(trusted: &[TrustedRow]) -> Vec<String> {
    if trusted.is_empty() {
        return Vec::new();
    }
    let width = trusted
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec!["trusted (instructions read, not maintained):".to_owned()];
    lines.extend(
        trusted
            .iter()
            .map(|entry| format!("  {:<width$}  {}", entry.name, entry.path)),
    );
    lines
}

pub fn render(report: &Report) -> String {
    if report.repos.is_empty() {
        if report.trusted.is_empty() {
            return report.notes.first().cloned().unwrap_or_default();
        }
        return trusted_lines(&report.trusted).join("\n");
    }

    let width = report
        .repos
        .iter()
        .map(|repo| repo.name.len())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for repo in &report.repos {
        let mut line = format!("{:<width$}  {}", repo.name, repo.path);
        if let Some(release_remote) = &repo.release_remote {
            let _ = write!(line, "  release-remote={release_remote}");
        }
        match &repo.newest_release {
            Some(newest) => {
                let _ = write!(line, "  newest={newest}");
            }
            None => line.push_str("  newest=none"),
        }
        if let Some(behind) = &repo.behind {
            let _ = write!(line, "  BEHIND: {behind}");
        }
        lines.push(line);
        lines.extend(repo.notes.iter().map(|note| format!("  ! {note}")));
        lines.extend(repo.problems.iter().map(|problem| format!("  ? {problem}")));
    }
    lines.extend(trusted_lines(&report.trusted));
    lines.extend(report.notes.iter().cloned());
    lines.join("\n")
}

/// A repository whose pin state could not be compared leaves the listing
/// incomplete: the command's central question went unanswered there.
pub fn exit_for(report: &Report) -> Exit {
    if report.repos.iter().any(|repo| !repo.problems.is_empty()) {
        Exit::Incomplete
    } else {
        Exit::Ok
    }
}

pub fn run(output: crate::cli::Output) -> anyhow::Result<Exit> {
    let path = default_config_path();
    let registry = load(&path)?;
    let releases = release_state(&registry);
    let forge = crate::forge::github::CliForge;
    let cache_root = crate::forge_cache::cache_root();
    let heads = ConsumerHeadMemo::default();
    let report = gather(&GatherInput {
        registry: &registry,
        releases: &releases,
        config_path: &path,
        forge: &forge,
        cache_root: cache_root.as_deref(),
        heads: &heads,
    });
    if let Some(payload) = crate::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", render(&report));
    }
    Ok(exit_for(&report))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use std::path::PathBuf;

    use super::*;
    use crate::config::{RepoEntry, TrustedEntry};
    use crate::consumer_pins::ConsumerHeadMemo;
    use crate::forge::{ConsumerHead, fake::FakeForge};
    use crate::ids::{BookmarkRef, BranchName, CommitId, RemoteName};
    fn entry(release: Option<&str>) -> RepoEntry {
        RepoEntry {
            path: PathBuf::from("/tmp/a-repo"),
            upstream: "u".to_owned(),
            origin: "o".to_owned(),
            base: None,
            release: release.map(ToOwned::to_owned),
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        }
    }

    fn registry(names: &[(&str, Option<&str>)]) -> Registry {
        Registry {
            repos: names
                .iter()
                .map(|(name, release)| ((*name).to_owned(), entry(*release)))
                .collect(),
            ..Registry::default()
        }
    }

    #[test]
    fn a_fixed_release_uses_its_publish_remote_when_origin_also_has_the_branch() {
        // Given: a split-release repo whose fixed branch exists at both origin and release.
        let mut entry = entry(Some("https://forge.invalid/release/repo"));
        entry.release_branch = Some("integration".to_owned());
        let tips = BTreeMap::from([
            (
                BookmarkRef::Remote {
                    branch: BranchName::new("integration"),
                    remote: RemoteName::new("origin"),
                },
                CommitId::new("origin-position"),
            ),
            (
                BookmarkRef::Remote {
                    branch: BranchName::new("integration"),
                    remote: RemoteName::new("release"),
                },
                CommitId::new("publish-position"),
            ),
        ]);

        // When: repos selects the fixed release position.
        let newest = newest_release(&tips, &entry);

        // Then: only the publish remote is reported.
        assert_eq!(newest, Some("integration@release".to_owned()));
    }

    #[test]
    fn a_fixed_release_ignores_an_origin_only_branch() {
        // Given: a split-release repo whose fixed branch exists ONLY at origin,
        // which is not its publish remote.
        let mut entry = entry(Some("https://forge.invalid/release/repo"));
        entry.release_branch = Some("integration".to_owned());
        let tips = BTreeMap::from([(
            BookmarkRef::Remote {
                branch: BranchName::new("integration"),
                remote: RemoteName::new("origin"),
            },
            CommitId::new("origin-position"),
        )]);

        // When: repos selects the fixed release position.
        let newest = newest_release(&tips, &entry);

        // Then: origin is not the publish remote, so no release is cut here.
        assert_eq!(newest, None);
    }

    #[test]
    fn a_fixed_release_prefers_its_local_branch_over_its_publish_remote() {
        // Given: a fixed release branch has both local and publish-remote positions.
        let mut entry = entry(Some("https://forge.invalid/release/repo"));
        entry.release_branch = Some("integration".to_owned());
        let tips = BTreeMap::from([
            (
                BookmarkRef::Local(BranchName::new("integration")),
                CommitId::new("local-position"),
            ),
            (
                BookmarkRef::Remote {
                    branch: BranchName::new("integration"),
                    remote: RemoteName::new("release"),
                },
                CommitId::new("publish-position"),
            ),
        ]);

        // When: repos selects the fixed release position.
        let newest = newest_release(&tips, &entry);

        // Then: the local branch wins without applying a release ordering.
        assert_eq!(newest, Some("integration".to_owned()));
    }

    #[test]
    fn repos_are_listed_one_per_line_sorted_by_name() {
        // Given: two repos out of order
        let registry = registry(&[("beta", None), ("alpha", None)]);
        let releases = release_state(&registry);
        // When: gathered and rendered
        let report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &FakeForge::default(),
            cache_root: None,
            heads: &ConsumerHeadMemo::default(),
        });
        let out = render(&report);
        // Then: alphabetical, one per line, plus the trailing consumer note
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("alpha"));
        assert!(lines[1].starts_with("beta"));
    }

    #[test]
    fn each_consumer_is_reported_separately() {
        // Consumers can sit on different releases, so one being current says nothing about
        // another. Collapsing them into a single verdict hid exactly that.
        let dir = tempfile::tempdir().unwrap();
        let current = "acme/current";
        let behind = "acme/behind";
        let commit = "aaaaaaaaaaaaaaaa";
        let forge = FakeForge {
            heads: BTreeMap::from([
                (
                    current.to_owned(),
                    ConsumerHead {
                        branch: "main".to_owned(),
                        commit: commit.to_owned(),
                    },
                ),
                (
                    behind.to_owned(),
                    ConsumerHead {
                        branch: "main".to_owned(),
                        commit: commit.to_owned(),
                    },
                ),
            ]),
            files: BTreeMap::from([
                (
                    (current.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                    "git = \"https://forge.invalid/o/sandbox-runner.git?rev=release/2026-07-28\"\n"
                        .to_owned(),
                ),
                (
                    (behind.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                    "git = \"https://forge.invalid/o/sandbox-runner.git?rev=release/2026-07-20\"\n"
                        .to_owned(),
                ),
            ]),
            ..FakeForge::default()
        };
        let heads = ConsumerHeadMemo::default();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: vec![current.to_owned(), behind.to_owned()],
            workspaces: None,
        };

        let lag = pin_lag(
            &entry,
            Some(&"release/2026-07-28@origin".to_owned()),
            None,
            &forge,
            None,
            &heads,
        )
        .lag
        .expect("one consumer is behind");

        assert!(
            lag.contains("acme/behind pins release/2026-07-20"),
            "was: {lag}"
        );
        assert!(
            !lag.contains("acme/current"),
            "the current consumer must not be reported as behind: {lag}"
        );
    }

    #[test]
    fn a_fixed_branch_pin_without_a_lock_is_current() {
        let dir = tempfile::tempdir().unwrap();
        let consumer = "acme/consumer";
        let commit = "aaaaaaaaaaaaaaaa";
        let forge = FakeForge {
            heads: BTreeMap::from([(
                consumer.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                "git = \"https://forge.invalid/o/sandbox-runner.git?branch=integration\"\n"
                    .to_owned(),
            )]),
            ..FakeForge::default()
        };
        let heads = ConsumerHeadMemo::default();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: Some("integration".to_owned()),
            test_count_command: None,
            consumers: vec![consumer.to_owned()],
            workspaces: None,
        };

        let pin_lag = pin_lag(&entry, None, None, &forge, None, &heads);

        assert_eq!(pin_lag.lag, None);
        assert!(
            pin_lag.problems.is_empty(),
            "problems: {:?}",
            pin_lag.problems
        );
    }

    #[test]
    fn a_fixed_locked_pin_without_a_repo_reports_a_comparison_note() {
        let dir = tempfile::tempdir().unwrap();
        let consumer = "acme/consumer";
        let commit = "aaaaaaaaaaaaaaaa";
        let forge = FakeForge {
            heads: BTreeMap::from([(
                consumer.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                "git = \"https://forge.invalid/o/sandbox-runner.git?branch=integration#548aaafb\"\n"
                    .to_owned(),
            )]),
            ..FakeForge::default()
        };
        let heads = ConsumerHeadMemo::default();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: Some("integration".to_owned()),
            test_count_command: None,
            consumers: vec![consumer.to_owned()],
            workspaces: None,
        };

        let pin_lag = pin_lag(&entry, None, None, &forge, None, &heads);

        assert_eq!(pin_lag.lag, None);
        assert!(
            pin_lag
                .notes
                .iter()
                .any(|note| note.contains("could not compare"))
        );
    }

    #[test]
    fn fixed_pin_comparison_notes_are_rendered_below_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let consumer = "acme/consumer";
        let commit = "aaaaaaaaaaaaaaaa";
        let forge = FakeForge {
            heads: BTreeMap::from([(
                consumer.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                "git = \"https://forge.invalid/o/sandbox-runner.git?branch=integration#548aaafb\"\n"
                    .to_owned(),
            )]),
            ..FakeForge::default()
        };
        let heads = ConsumerHeadMemo::default();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: Some("integration".to_owned()),
            test_count_command: None,
            consumers: vec![consumer.to_owned()],
            workspaces: None,
        };
        let registry = Registry {
            repos: BTreeMap::from([("sandbox-runner".to_owned(), entry)]),
            ..Registry::default()
        };
        let releases = BTreeMap::from([(
            "sandbox-runner".to_owned(),
            ReleaseState {
                newest: None,
                repo: None,
            },
        )]);

        let report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &forge,
            cache_root: None,
            heads: &heads,
        });
        let rendered = render(&report);

        assert_eq!(exit_for(&report), Exit::Ok);
        assert!(
            rendered.contains("\n  ! could not compare"),
            "was: {rendered}"
        );
    }

    #[test]
    fn a_cached_forge_failure_marks_the_repository_listing_incomplete() {
        let cache = tempfile::tempdir().expect("create consumer cache");
        let consumer = "acme/consumer";
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut entry = entry(None);
        entry.origin = "https://forge.invalid/o/tool.git".to_owned();
        entry.consumers = vec![consumer.to_owned()];
        let registry = Registry {
            repos: BTreeMap::from([("tool".to_owned(), entry)]),
            ..Registry::default()
        };
        let releases = BTreeMap::from([(
            "tool".to_owned(),
            ReleaseState {
                newest: Some("release/2026-08-05@origin".to_owned()),
                repo: None,
            },
        )]);
        let priming_forge = FakeForge {
            heads: BTreeMap::from([(
                consumer.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                "tool = { git = \"https://forge.invalid/o/tool.git?rev=release%2F2026-08-05#112233445566\" }\n"
                    .to_owned(),
            )]),
            ..FakeForge::default()
        };
        let priming_heads = ConsumerHeadMemo::default();
        let priming_report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &priming_forge,
            cache_root: Some(cache.path()),
            heads: &priming_heads,
        });
        assert_eq!(exit_for(&priming_report), Exit::Ok);

        let unavailable_forge = FakeForge {
            fail_consumer_head: true,
            ..FakeForge::default()
        };
        let unavailable_heads = ConsumerHeadMemo::default();
        let report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &unavailable_forge,
            cache_root: Some(cache.path()),
            heads: &unavailable_heads,
        });
        let rendered = render(&report);

        assert!(rendered.contains("? acme/consumer: forge unreachable:"));
        assert!(rendered.contains("fake consumer head failed"));
        assert!(rendered.contains(
            "! acme/consumer: forge unreachable; pins answered from cache at aaaaaaaaaaaa"
        ));
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn a_split_release_remote_is_shown() {
        let registry = registry(&[("split", Some("r"))]);
        let releases = release_state(&registry);
        let report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &FakeForge::default(),
            cache_root: None,
            heads: &ConsumerHeadMemo::default(),
        });
        assert!(render(&report).contains("release-remote=r"));
    }

    #[test]
    fn the_release_column_is_omitted_when_it_defaults_to_origin() {
        let registry = registry(&[("simple", None)]);
        let releases = release_state(&registry);
        let report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &FakeForge::default(),
            cache_root: None,
            heads: &ConsumerHeadMemo::default(),
        });
        assert!(!render(&report).contains("release-remote="));
    }

    #[test]
    fn an_empty_registry_reports_where_to_add_entries() {
        // Printing nothing would be indistinguishable from a broken command.
        let report = gather(&GatherInput {
            registry: &Registry::default(),
            releases: &BTreeMap::new(),
            config_path: Path::new("/tmp/somewhere/repos.toml"),
            forge: &FakeForge::default(),
            cache_root: None,
            heads: &ConsumerHeadMemo::default(),
        });
        let out = render(&report);
        assert!(out.contains("no repos configured"));
        assert!(out.contains("/tmp/somewhere/repos.toml"));
    }

    #[test]
    fn an_empty_registry_gathers_a_note_pointing_at_the_config_path() {
        // The Report model carries the same note independent of rendering, so
        // TOON/JSON consumers see it too.
        let report = gather(&GatherInput {
            registry: &Registry::default(),
            releases: &BTreeMap::new(),
            config_path: Path::new("/tmp/somewhere/repos.toml"),
            forge: &FakeForge::default(),
            cache_root: None,
            heads: &ConsumerHeadMemo::default(),
        });
        assert!(report.repos.is_empty());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no repos configured")
                    && note.contains("/tmp/somewhere/repos.toml"))
        );
    }

    #[test]
    fn a_gathered_report_serializes_release_state_and_trusted_entries() {
        let dir = tempfile::tempdir().unwrap();
        let consumer = "acme/behind";
        let commit = "aaaaaaaaaaaaaaaa";
        let forge = FakeForge {
            heads: BTreeMap::from([(
                consumer.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                "git = \"https://forge.invalid/o/sandbox-runner.git?rev=release/2026-07-20\"\n"
                    .to_owned(),
            )]),
            ..FakeForge::default()
        };
        let heads = ConsumerHeadMemo::default();
        let mut sandbox = entry(None);
        sandbox.path = dir.path().join("repo");
        sandbox.origin = "https://forge.invalid/o/sandbox-runner".to_owned();
        sandbox.consumers = vec![consumer.to_owned()];
        let registry = Registry {
            repos: BTreeMap::from([("sandbox-runner".to_owned(), sandbox)]),
            trusted: BTreeMap::from([(
                "legacy".to_owned(),
                TrustedEntry {
                    path: PathBuf::from("/tmp/legacy"),
                },
            )]),
            ..Registry::default()
        };
        let releases = BTreeMap::from([(
            "sandbox-runner".to_owned(),
            ReleaseState {
                newest: Some("release/2026-07-28@origin".to_owned()),
                repo: None,
            },
        )]);

        let report = gather(&GatherInput {
            registry: &registry,
            releases: &releases,
            config_path: Path::new("/tmp/repos.toml"),
            forge: &forge,
            cache_root: None,
            heads: &heads,
        });
        let json = serde_json::to_value(&report).expect("serialize report");

        assert_eq!(
            json["repos"][0]["newest_release"],
            "release/2026-07-28@origin"
        );
        assert!(
            json["repos"][0]["behind"]
                .as_str()
                .is_some_and(|behind| behind.contains("acme/behind pins release/2026-07-20")),
            "was: {json:#?}"
        );
        assert_eq!(json["trusted"][0]["name"], "legacy");
    }
}
