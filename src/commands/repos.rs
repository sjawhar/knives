//! `knives repos`: what am I maintaining.
//!
//! Deliberately separate from `knives wip`, which answers what is being worked on
//! right now. Conflating the two was an earlier mistake in this design.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::cli::Exit;
use crate::commands::status::release_order;
use crate::config::{Registry, RepoEntry, Role, default_config_path, load};
use crate::ids::{BookmarkRef, ReleaseScheme, RemoteName, is_our_release};
use crate::jj::Repo;

struct ReleaseState {
    newest: Option<String>,
    repo: Option<Repo>,
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
            .filter(|reference| is_our_release(reference, &scheme))
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
}

fn consumer_label(consumer: &Path) -> String {
    let label = consumer.file_name().map_or_else(
        || consumer.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    consumer
        .parent()
        .and_then(|parent| parent.file_name())
        .map_or_else(
            || label.clone(),
            |parent| format!("{}/{}", parent.to_string_lossy(), label),
        )
}

pub fn pin_lag(entry: &RepoEntry, newest: Option<&String>, repo: Option<&Repo>) -> PinLag {
    let scheme = entry.release_scheme();
    match &scheme {
        ReleaseScheme::Dated => {
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
            let slug = crate::commands::release::repo_slug(entry);
            // Reported per consumer, because they can sit on different releases: one consumer
            // being current says nothing about another, and collapsing them into a single verdict
            // hid exactly that.
            let mut behind = Vec::new();
            let mut notes = Vec::new();
            for consumer in &entry.consumers {
                let (pins, consumer_notes) =
                    crate::commands::release::scan_consumer_for(consumer, slug.as_deref(), &scheme);
                notes.extend(consumer_notes);
                let label = consumer_label(consumer);
                if pins.is_empty() {
                    behind.push(format!("{label} pins no release of this repo"));
                    continue;
                }
                if pins.iter().any(|pin| pin.reference == newest_branch) {
                    continue;
                }
                let mut names: Vec<&str> = pins.iter().map(|pin| pin.reference.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                behind.push(format!("{label} pins {}", names.join(", ")));
            }
            PinLag {
                lag: (!behind.is_empty())
                    .then(|| format!("newest is {newest_branch}; {}", behind.join("; "))),
                notes,
            }
        }
        ReleaseScheme::Fixed(fixed) => {
            let slug = crate::commands::release::repo_slug(entry);
            let local_tip = repo.and_then(|repo| {
                repo.bookmark_tips()
                    .ok()
                    .and_then(|tips| tips.get(&BookmarkRef::Local(fixed.clone())).cloned())
            });
            let mut behind = Vec::new();
            let mut notes = Vec::new();
            for consumer in &entry.consumers {
                let (pins, consumer_notes) =
                    crate::commands::release::scan_consumer_for(consumer, slug.as_deref(), &scheme);
                notes.extend(consumer_notes);
                let label = consumer_label(consumer);
                if pins.is_empty() {
                    behind.push(format!("{label} pins no release of this repo"));
                    continue;
                }
                for pin in pins {
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
            }
        }
    }
}

fn render_with_releases(
    registry: &Registry,
    releases: &BTreeMap<String, ReleaseState>,
    config_path: &Path,
) -> String {
    if registry.is_empty() {
        return render(registry, config_path);
    }
    let width = registry.repos.keys().map(String::len).max().unwrap_or(0);
    let mut lines: Vec<String> = registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let mut line = format!("{name:<width$}  {}", entry.path.display());
            if entry.has_split_release() {
                let _ = write!(line, "  release-remote={}", entry.remote(Role::Release));
            }
            let state = releases.get(name);
            let newest = state.and_then(|state| state.newest.as_ref());
            match newest {
                Some(newest) => {
                    let _ = write!(line, "  newest={newest}");
                }
                None => line.push_str("  newest=none"),
            }
            let pin_lag = pin_lag(entry, newest, state.and_then(|state| state.repo.as_ref()));
            if let Some(lag) = pin_lag.lag {
                let _ = write!(line, "  BEHIND: {lag}");
            }
            std::iter::once(line)
                .chain(pin_lag.notes.into_iter().map(|note| format!("  ! {note}")))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    lines.extend(trusted_lines(registry));
    lines.push(
        "pin state lives in consumers: record them as `consumers = [...]` in the registry"
            .to_owned(),
    );
    lines.join("\n")
}

/// Trusted-but-unmaintained entries, listed apart from the forks.
///
/// Shown because a registry entry nothing ever prints is one nobody can debug:
/// these change what guidance an agent receives, so they have to be visible. Kept
/// under their own heading because they are not answers to "what am I
/// maintaining" — no fork command touches them.
fn trusted_lines(registry: &Registry) -> Vec<String> {
    if registry.trusted.is_empty() {
        return Vec::new();
    }
    let width = registry.trusted.keys().map(String::len).max().unwrap_or(0);
    let mut lines = vec!["trusted (instructions read, not maintained):".to_owned()];
    lines.extend(
        registry
            .trusted
            .iter()
            .map(|(name, entry)| format!("  {name:<width$}  {}", entry.path.display())),
    );
    lines
}

pub fn render(registry: &Registry, config_path: &Path) -> String {
    if registry.is_empty() {
        // A trusted-only registry is a real configuration, not an empty one, so it
        // must not be reported as "nothing configured".
        let trusted = trusted_lines(registry);
        if !trusted.is_empty() {
            return trusted.join("\n");
        }
        // Saying where to put them beats printing nothing and exiting zero.
        return format!(
            "no repos configured; add entries to {}",
            config_path.display()
        );
    }

    let width = registry.repos.keys().map(String::len).max().unwrap_or(0);
    registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let mut line = format!("{name:<width$}  {}", entry.path.display());
            if entry.has_split_release() {
                // Only shown when releases genuinely live elsewhere, so the
                // common case stays quiet.
                let _ = write!(line, "  release={}", entry.remote(Role::Release));
            }
            line
        })
        .chain(trusted_lines(registry))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn run() -> anyhow::Result<Exit> {
    let path = default_config_path();
    let registry = load(&path)?;
    let releases = release_state(&registry);
    println!("{}", render_with_releases(&registry, &releases, &path));
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use std::path::PathBuf;

    use super::*;
    use crate::config::RepoEntry;
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
        // When: rendered
        let out = render(&registry, Path::new("/tmp/repos.toml"));
        // Then: alphabetical, one per line
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("alpha"));
        assert!(lines[1].starts_with("beta"));
    }

    #[test]
    fn each_consumer_is_reported_separately() {
        // Consumers can sit on different releases, so one being current says nothing about
        // another. Collapsing them into a single verdict hid exactly that.
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current/default");
        let behind = dir.path().join("behind/default");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&behind).unwrap();
        std::fs::write(
            current.join("uv.lock"),
            "git = \"https://forge.invalid/o/sandbox-runner.git?rev=release/2026-07-28\"\n",
        )
        .unwrap();
        std::fs::write(
            behind.join("uv.lock"),
            "git = \"https://forge.invalid/o/sandbox-runner.git?rev=release/2026-07-20\"\n",
        )
        .unwrap();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: vec![current, behind],
        };

        let lag = pin_lag(&entry, Some(&"release/2026-07-28@origin".to_owned()), None)
            .lag
            .expect("one consumer is behind");

        assert!(
            lag.contains("behind/default pins release/2026-07-20"),
            "was: {lag}"
        );
        assert!(
            !lag.contains("current/default"),
            "the current consumer must not be reported as behind: {lag}"
        );
    }

    #[test]
    fn a_fixed_branch_pin_without_a_lock_is_current() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("uv.lock"),
            "git = \"https://forge.invalid/o/sandbox-runner.git?branch=integration\"\n",
        )
        .unwrap();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: Some("integration".to_owned()),
            test_count_command: None,
            consumers: vec![dir.path().to_owned()],
        };

        let pin_lag = pin_lag(&entry, None, None);

        assert_eq!(pin_lag.lag, None);
        assert!(
            pin_lag
                .notes
                .iter()
                .any(|note| note.contains("pins read from the working copy")),
            "notes: {:?}",
            pin_lag.notes
        );
    }

    #[test]
    fn a_fixed_locked_pin_without_a_repo_reports_a_comparison_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("uv.lock"),
            "git = \"https://forge.invalid/o/sandbox-runner.git?branch=integration#548aaafb\"\n",
        )
        .unwrap();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: Some("integration".to_owned()),
            test_count_command: None,
            consumers: vec![dir.path().to_owned()],
        };

        let pin_lag = pin_lag(&entry, None, None);

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
        std::fs::write(
            dir.path().join("uv.lock"),
            "git = \"https://forge.invalid/o/sandbox-runner.git?branch=integration#548aaafb\"\n",
        )
        .unwrap();
        let entry = RepoEntry {
            path: dir.path().join("repo"),
            upstream: "https://forge.invalid/up/sandbox-runner".to_owned(),
            origin: "https://forge.invalid/o/sandbox-runner".to_owned(),
            base: None,
            release: None,
            release_branch: Some("integration".to_owned()),
            test_count_command: None,
            consumers: vec![dir.path().to_owned()],
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

        let rendered = render_with_releases(&registry, &releases, Path::new("/tmp/repos.toml"));

        assert!(
            rendered.contains("\n  ! could not compare"),
            "was: {rendered}"
        );
    }

    #[test]
    fn a_split_release_remote_is_shown() {
        let registry = registry(&[("split", Some("r"))]);
        assert!(render(&registry, Path::new("/tmp/repos.toml")).contains("release=r"));
    }

    #[test]
    fn the_release_column_is_omitted_when_it_defaults_to_origin() {
        let registry = registry(&[("simple", None)]);
        assert!(!render(&registry, Path::new("/tmp/repos.toml")).contains("release="));
    }

    #[test]
    fn an_empty_registry_reports_where_to_add_entries() {
        // Printing nothing would be indistinguishable from a broken command.
        let out = render(&Registry::default(), Path::new("/tmp/somewhere/repos.toml"));
        assert!(out.contains("no repos configured"));
        assert!(out.contains("/tmp/somewhere/repos.toml"));
    }
}
