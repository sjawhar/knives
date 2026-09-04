//! `knives register`: print a paste-ready registry snippet.
//!
//! Registration is a trust grant, so nothing here writes the registry: a human
//! pastes the entry. A checkout whose `upstream` the registry already lists is
//! named rather than re-described, because the entry is the identity and one
//! upstream cannot be two entries. A pasted entry whose name another entry
//! already holds is rejected by TOML at load, with the line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::bind::{self, Unbound, remote_host, repository_name, url_owner};
use crate::cli::Exit;
use crate::config::{Registry, RepoEntry, default_config_path};
use crate::hook::resolve::guidance_name;
use crate::ids::RepoName;

/// What `register` decided about a directory, so the caller renders rather
/// than re-deriving.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "decided once per invocation; the snippet's entry is the payload"
)]
pub enum Outcome {
    /// The checkout's `upstream` is already an entry's.
    AlreadyRegistered { name: RepoName },
    Snippet {
        name: String,
        entry: RepoEntry,
        warnings: Vec<String>,
    },
    /// Not a repository, a git clone with no jj store (the hook binds those;
    /// fork verbs do not), or a checkout missing a role: the refusal line.
    Refused(String),
}

/// Decide for the checkout `path` is inside.
///
/// `path` may be any directory inside the checkout: [`bind::checkout_root`]
/// finds the root, following a workspace's pointer to its checkout. Only
/// remotes already named for a role are adopted. Guessing which arbitrary
/// remote is the upstream is a coin flip, and a wrong upstream makes every
/// landed check answer about the wrong repository.
///
/// # Errors
///
/// Returns an error when the checkout's remotes cannot be read — including a
/// workspace whose checkout is gone, which jj reports in its own words.
pub fn outcome_for(path: &Path, registry: &Registry) -> anyhow::Result<Outcome> {
    let Some(root) = bind::checkout_root(path) else {
        return Ok(Outcome::Refused(format!(
            "{} is not a repository",
            path.display()
        )));
    };
    if !root.join(".jj").is_dir() {
        return Ok(Outcome::Refused(
            Unbound::GitOnly { root }.message(registry),
        ));
    }
    Ok(decide(&root, &bind::remotes(&root)?, registry))
}

fn decide(root: &Path, remotes: &BTreeMap<String, String>, registry: &Registry) -> Outcome {
    let upstream = remotes.get("upstream");
    let origin = remotes.get("origin");
    let (Some(upstream), Some(origin)) = (upstream, origin) else {
        let found: Vec<&str> = remotes.keys().map(String::as_str).collect();
        let absent: Vec<&str> = [("upstream", upstream), ("origin", origin)]
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(role, _)| *role)
            .collect();
        return Outcome::Refused(format!(
            "{} has remotes [{}] but no remote named {}; rename a remote to its role, or add one",
            root.display(),
            found.join(", "),
            absent.join(" or ")
        ));
    };
    if let Some((name, _)) = bind::entry_for(registry, upstream) {
        return Outcome::AlreadyRegistered { name };
    }
    Outcome::Snippet {
        name: guidance_name(root),
        entry: RepoEntry {
            release: remotes.get("release").cloned(),
            ..RepoEntry::new(upstream, origin)
        },
        warnings: miswiring_warnings(remotes),
    }
}

/// An untracked remote that looks like another fork of upstream: the wiring
/// that produced months of misleading unpushed findings on six real forks,
/// with `origin` pointing at an org copy while the personal fork sat as an ad
/// hoc remote.
fn miswiring_warnings(remotes: &BTreeMap<String, String>) -> Vec<String> {
    let (Some(upstream), Some(origin)) = (remotes.get("upstream"), remotes.get("origin")) else {
        return Vec::new();
    };
    let (Some(upstream_name), Some(upstream_owner), Some(origin_owner)) = (
        repository_name(upstream),
        url_owner(upstream),
        url_owner(origin),
    ) else {
        return Vec::new();
    };
    let upstream_host = remote_host(upstream);

    remotes
        .iter()
        .filter_map(|(name, url)| {
            if matches!(name.as_str(), "upstream" | "origin" | "release") {
                return None;
            }
            let (Some(name_of_remote), Some(owner)) = (repository_name(url), url_owner(url))
            else {
                return None;
            };
            if !name_of_remote.eq_ignore_ascii_case(upstream_name)
                || owner.eq_ignore_ascii_case(upstream_owner)
                || owner.eq_ignore_ascii_case(origin_owner)
            {
                return None;
            }
            if let (Some(host), Some(upstream_host)) = (remote_host(url), upstream_host)
                && !host.eq_ignore_ascii_case(upstream_host)
            {
                return None;
            }
            Some(format!(
                "origin is {origin}; untracked remote {name} looks like another fork of upstream ({url}). knives treats origin as YOUR fork — the one your branches push to and your PR heads live on. If {name} is that fork, rename remotes so it is origin."
            ))
        })
        .collect()
}

/// Serializes one repository entry as paste-ready registry TOML.
///
/// # Errors
///
/// Returns an error when a field cannot be represented in TOML.
pub fn snippet(name: &str, entry: &RepoEntry) -> Result<String, toml::ser::Error> {
    let registry = Registry {
        repos: BTreeMap::from([(name.to_owned(), entry.clone())]),
        ..Registry::default()
    };
    toml::to_string_pretty(&registry)
}

/// Prints a registry snippet, or the name a registered checkout already has,
/// without changing the registry.
///
/// # Errors
///
/// Returns errors from reading the checkout's remotes or serializing the entry.
pub fn run(target: Option<PathBuf>, registry: &Registry) -> anyhow::Result<Exit> {
    let path = match target {
        Some(given) => given,
        None => std::env::current_dir()?,
    };
    match outcome_for(&path, registry)? {
        Outcome::AlreadyRegistered { name } => {
            println!("already registered as {name}");
            Ok(Exit::Ok)
        }
        Outcome::Snippet {
            name,
            entry,
            warnings,
        } => {
            for warning in &warnings {
                eprintln!("  warning: {warning}");
            }
            print!("{}", snippet(&name, &entry)?);
            eprintln!(
                "paste this into {} to register; hooks reload the registry on every event, so it takes effect on the next tool call\n  convention: origin = your fork (push target, PR heads); upstream = the maintainer's repo (fetch only)",
                default_config_path().display()
            );
            Ok(Exit::Ok)
        }
        Outcome::Refused(line) => {
            eprintln!("{line}");
            Ok(Exit::Usage)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{Outcome, decide, outcome_for, snippet};
    use crate::config::{Registry, RepoEntry};
    use crate::ids::RepoName;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;

    fn remotes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn registry_with(name: &str, upstream: &str) -> Registry {
        Registry {
            repos: BTreeMap::from([(
                name.to_owned(),
                RepoEntry::new(upstream, "https://forge.invalid/ours/tool"),
            )]),
            ..Registry::default()
        }
    }

    fn snippet_warnings(outcome: Outcome) -> Vec<String> {
        let Outcome::Snippet { warnings, .. } = outcome else {
            panic!("expected a snippet, was {outcome:?}")
        };
        warnings
    }

    #[test]
    fn only_remotes_named_for_a_role_are_adopted_and_the_entry_has_no_path() {
        // Given: a repo with an extra remote that is nobody's role
        let found = remotes(&[
            ("origin", "o"),
            ("upstream", "u"),
            ("someone-elses-fork", "x"),
        ]);
        // When: register decides
        let outcome = decide(Path::new("/tmp/a-repo"), &found, &Registry::default());
        // Then: the stray remote is ignored rather than guessed at
        let Outcome::Snippet {
            name,
            entry,
            warnings,
        } = outcome
        else {
            panic!("expected a snippet")
        };
        assert_eq!(name, "a-repo");
        assert_eq!(entry.upstream, "u");
        assert_eq!(entry.origin, "o");
        assert_eq!(entry.release, None);
        assert!(warnings.is_empty(), "was: {warnings:?}");
        let text = snippet(&name, &entry).expect("snippet serializes");
        assert!(!text.contains("path"), "was: {text}");
    }

    #[test]
    fn a_split_release_remote_is_adopted_when_present() {
        let found = remotes(&[("origin", "o"), ("upstream", "u"), ("release", "r")]);
        let Outcome::Snippet {
            entry, warnings, ..
        } = decide(Path::new("/tmp/a"), &found, &Registry::default())
        else {
            panic!("expected a snippet")
        };
        assert_eq!(entry.release.as_deref(), Some("r"));
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_checkout_whose_upstream_is_an_entry_is_already_registered_whatever_the_spelling() {
        let registry = registry_with("tool", "https://forge.invalid/maintainer/tool");
        let found = remotes(&[
            ("origin", "https://forge.invalid/someone/tool.git"),
            ("upstream", "git@forge.invalid:maintainer/tool.git"),
        ]);
        assert_eq!(
            decide(Path::new("/tmp/elsewhere"), &found, &registry),
            Outcome::AlreadyRegistered {
                name: RepoName::new("tool")
            }
        );
    }

    #[test]
    fn an_untracked_second_fork_of_upstream_is_flagged_as_a_possible_miswiring() {
        // Given: origin pointing at an org copy while a personal fork of the same
        // repository sits as an ad hoc remote — the exact wiring that produced
        // months of misleading unpushed findings on six real forks
        let found = remotes(&[
            ("origin", "https://forge.invalid/org-copy/tool.git"),
            ("upstream", "https://forge.invalid/maintainer/tool.git"),
            ("mine", "https://forge.invalid/someone/tool.git"),
        ]);
        let warnings = snippet_warnings(decide(
            Path::new("/tmp/tool/default"),
            &found,
            &Registry::default(),
        ));
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
        assert!(warnings[0].contains("mine"), "was: {warnings:?}");
    }

    #[test]
    fn a_remote_for_a_different_repository_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/someone/tool.git"),
            ("upstream", "https://forge.invalid/maintainer/tool.git"),
            ("other", "https://forge.invalid/someone/unrelated.git"),
        ]);
        let warnings = snippet_warnings(decide(Path::new("/tmp/t"), &found, &Registry::default()));
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_case_variant_of_upstream_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("acme", "https://forge.invalid/acme/work.git"),
        ]);
        let warnings = snippet_warnings(decide(Path::new("/tmp/t"), &found, &Registry::default()));
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_case_variant_of_upstream_slug_warns_for_a_third_owner() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("third", "https://forge.invalid/someone/WORK.git"),
        ]);
        let warnings = snippet_warnings(decide(Path::new("/tmp/t"), &found, &Registry::default()));
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
    }

    #[test]
    fn an_owner_matching_origin_or_upstream_is_not_a_miswiring() {
        for (name, url) in [
            ("same-as-origin", "https://forge.invalid/ours/work.git"),
            ("same-as-upstream", "https://forge.invalid/ACME/work.git"),
            ("release", "https://forge.invalid/someone/work.git"),
        ] {
            let found = remotes(&[
                ("origin", "https://forge.invalid/ours/work.git"),
                ("upstream", "https://forge.invalid/ACME/work.git"),
                (name, url),
            ]);
            let warnings =
                snippet_warnings(decide(Path::new("/tmp/t"), &found, &Registry::default()));
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
        }
    }

    #[test]
    fn a_cross_forge_remote_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://github.example/ours/work.git"),
            ("upstream", "https://github.example/ACME/work.git"),
            ("other-forge", "https://gitlab.example/someone/work.git"),
        ]);
        let warnings = snippet_warnings(decide(Path::new("/tmp/t"), &found, &Registry::default()));
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn an_unparseable_host_keeps_a_possible_miswiring_warning() {
        let found = remotes(&[
            ("origin", "https:///ours/work.git"),
            ("upstream", "https:///ACME/work.git"),
            ("third", "https://forge.invalid/someone/work.git"),
        ]);
        let warnings = snippet_warnings(decide(Path::new("/tmp/t"), &found, &Registry::default()));
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
    }

    #[test]
    fn every_refusal_names_the_directory_and_what_is_missing() {
        assert_eq!(
            decide(
                Path::new("/tmp/a-repo"),
                &remotes(&[("origin", "o")]),
                &Registry::default(),
            ),
            Outcome::Refused(
                "/tmp/a-repo has remotes [origin] but no remote named upstream; rename a remote \
                 to its role, or add one"
                    .to_owned()
            )
        );
        let nowhere = tempfile::tempdir().expect("directory");
        assert_eq!(
            outcome_for(nowhere.path(), &Registry::default()).expect("decided"),
            Outcome::Refused(format!("{} is not a repository", nowhere.path().display()))
        );
        let clone = tempfile::tempdir().expect("directory");
        std::fs::create_dir(clone.path().join(".git")).expect(".git");
        assert_eq!(
            outcome_for(clone.path(), &Registry::default()).expect("decided"),
            Outcome::Refused(format!(
                "{} is a git clone, not a jj checkout; fork commands need jj",
                clone.path().canonicalize().expect("canonical").display()
            ))
        );
    }

    #[test]
    fn the_snippet_is_valid_toml_that_round_trips_into_a_registry_entry() {
        // The whole command is "print what a human would paste"; a snippet that
        // does not parse back into the same entry is worse than no command.
        let entry = RepoEntry {
            base: Some("integration".to_owned()),
            release: Some("https://forge.invalid/someone/tool-releases.git".to_owned()),
            release_branch: Some("release".to_owned()),
            test_count_command: Some("cargo test -- --list | wc -l".to_owned()),
            consumers: vec!["acme/consumer".to_owned()],
            workspaces: Some(std::path::PathBuf::from("~/.worktrees/tool")),
            ..RepoEntry::new(
                "https://forge.invalid/maintainer/tool.git",
                "https://forge.invalid/someone/tool.git",
            )
        };
        let text = snippet("tool", &entry).expect("snippet serializes");
        assert!(text.starts_with("[repos.tool]"), "was: {text}");
        let parsed: Registry = toml::from_str(&text).expect("snippet parses");
        assert_eq!(parsed.repos["tool"], entry);
    }

    #[cfg(unix)]
    #[test]
    fn the_snippet_returns_an_error_for_a_non_utf8_workspaces_directory() {
        let entry = RepoEntry {
            workspaces: Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(
                vec![0xff],
            ))),
            ..RepoEntry::new(
                "https://forge.invalid/maintainer/tool.git",
                "https://forge.invalid/someone/tool.git",
            )
        };

        assert!(snippet("tool", &entry).is_err());
    }
}
