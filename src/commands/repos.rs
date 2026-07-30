//! `knives repos`: what am I maintaining.
//!
//! Deliberately separate from `knives wip`, which answers what is being worked on
//! right now. Conflating the two was an earlier mistake in this design.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::cli::Exit;
use crate::commands::status::{is_our_release, release_order};
use crate::config::{Registry, RepoEntry, Role, default_config_path, load};
use crate::jj::Repo;

/// The newest release each repo has cut, if any.
///
/// The design asks `repos` for pin and release state. Release state is
/// answerable here. Pin state is not: it lives in a consumer, which this command
/// has no handle on, so it is `knives release --consumer` that answers it and this
/// command says so rather than implying otherwise.
pub fn release_state(registry: &Registry) -> BTreeMap<String, Option<String>> {
    registry
        .repos
        .iter()
        .map(|(name, entry)| {
            let newest = Repo::open(&entry.path)
                .and_then(|repo| repo.bookmark_tips())
                .ok()
                .and_then(|tips| {
                    tips.keys()
                        .filter(|reference| is_our_release(reference))
                        .max_by_key(|reference| release_order(reference.branch().as_str()))
                        .map(ToString::to_string)
                });
            (name.clone(), newest)
        })
        .collect()
}

/// How far behind a consumer's pin is from the newest release we cut.
///
/// The single most useful thing a sweep of these forks found was that the consumer
/// pinned releases older than our own cuts, across three repositories at once. Nobody
/// runs a command to answer a question they have not thought of, so this is reported
/// beside the release state rather than hidden behind a flag.
fn pin_lag(entry: &RepoEntry, newest: Option<&String>) -> Option<String> {
    if entry.consumers.is_empty() {
        return None;
    }
    let newest = newest?;
    // The newest release arrives qualified with the remote it was seen on, while a pin
    // names only the branch. Comparing those forms directly called every repo behind,
    // including ones pinned exactly at the newest cut.
    let newest_branch = newest.split('@').next().unwrap_or(newest);
    let slug = crate::commands::release::repo_slug(entry);
    // Reported per consumer, because they can sit on different releases: one consumer
    // being current says nothing about another, and collapsing them into a single verdict
    // hid exactly that.
    let mut behind = Vec::new();
    for consumer in &entry.consumers {
        let pins = crate::commands::release::scan_consumer_for(consumer, slug.as_deref());
        let label = consumer.file_name().map_or_else(
            || consumer.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let parent = consumer
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().into_owned());
        let label = parent.map_or_else(|| label.clone(), |parent| format!("{parent}/{label}"));
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
    if behind.is_empty() {
        return None;
    }
    Some(format!("newest is {newest_branch}; {}", behind.join("; ")))
}

pub fn render_with_releases(
    registry: &Registry,
    releases: &BTreeMap<String, Option<String>>,
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
            let newest = releases.get(name).and_then(Option::as_ref);
            match newest {
                Some(newest) => {
                    let _ = write!(line, "  newest={newest}");
                }
                None => line.push_str("  newest=none"),
            }
            if let Some(lag) = pin_lag(entry, newest) {
                let _ = write!(line, "  BEHIND: {lag}");
            }
            line
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

    fn entry(release: Option<&str>) -> RepoEntry {
        RepoEntry {
            path: PathBuf::from("/tmp/a-repo"),
            upstream: "u".to_owned(),
            origin: "o".to_owned(),
            base: None,
            release: release.map(ToOwned::to_owned),
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
            test_count_command: None,
            consumers: vec![current, behind],
        };

        let lag = pin_lag(&entry, Some(&"release/2026-07-28@origin".to_owned()))
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
