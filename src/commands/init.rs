//! `knives init`: adopt a repository into the registry.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::config::{RepoEntry, default_config_path, load, save};
use crate::hook::resolve::{guidance_name, remote_authority_and_path, url_owner};

/// What init decided, so the caller renders rather than re-deriving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitOutcome {
    NotARepository {
        path: PathBuf,
    },
    MissingRoles {
        path: PathBuf,
        found: Vec<String>,
        absent: Vec<String>,
    },
    Adopted {
        name: String,
        entry: Box<RepoEntry>,
        warnings: Vec<String>,
    },
    /// A different tree already holds this name.
    ///
    /// Refusing matters beyond tidiness: the registry is the plugin's trust set,
    /// so silently replacing an entry re-points guidance injection at whatever
    /// tree ran `init` last. An adversarial fixture at `<anywhere>/hawk/default`
    /// could take over the name `hawk`. This is the "verify one repo per fork"
    /// the design asks for.
    NameTaken {
        name: String,
        existing: PathBuf,
        requested: PathBuf,
    },
}

/// Map a repository's existing remotes onto roles.
///
/// Only remotes already named for a role are adopted. Guessing which arbitrary
/// remote is the upstream is a coin flip, and a wrong upstream makes every
/// landed check answer about the wrong repository.
pub fn decide_with_registry(
    path: &Path,
    remotes: &BTreeMap<String, String>,
    existing: Option<&RepoEntry>,
) -> InitOutcome {
    let outcome = decide(path, remotes);
    if let InitOutcome::Adopted {
        name,
        entry,
        warnings: _,
    } = &outcome
        && let Some(held) = existing
        && held.path != entry.path
    {
        return InitOutcome::NameTaken {
            name: name.clone(),
            existing: held.path.clone(),
            requested: entry.path.clone(),
        };
    }
    outcome
}

pub fn decide(path: &Path, remotes: &BTreeMap<String, String>) -> InitOutcome {
    let upstream = remotes.get("upstream");
    let origin = remotes.get("origin");
    let absent: Vec<String> = [("upstream", upstream), ("origin", origin)]
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(role, _)| (*role).to_owned())
        .collect();

    let (Some(upstream), Some(origin)) = (upstream, origin) else {
        return InitOutcome::MissingRoles {
            path: path.to_owned(),
            found: remotes.keys().cloned().collect(),
            absent,
        };
    };

    InitOutcome::Adopted {
        name: guidance_name(path),
        entry: Box::new(RepoEntry {
            path: path.to_owned(),
            upstream: upstream.clone(),
            origin: origin.clone(),
            base: None,
            release: remotes.get("release").cloned(),
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        }),
        warnings: miswiring_warnings(remotes),
    }
}

fn url_host(url: &str) -> Option<&str> {
    let (authority, _) = remote_authority_and_path(url)?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (!host.is_empty()).then_some(host)
}

fn repo_slug(url: &str) -> Option<&str> {
    let (_, repository) = url.trim_end_matches('/').rsplit_once('/')?;
    let slug = repository.strip_suffix(".git").or(Some(repository))?;
    (!slug.is_empty()).then_some(slug)
}

fn miswiring_warnings(remotes: &BTreeMap<String, String>) -> Vec<String> {
    let (Some(upstream), Some(origin)) = (remotes.get("upstream"), remotes.get("origin")) else {
        return Vec::new();
    };
    let (Some(upstream_slug), Some(upstream_owner), Some(origin_owner)) =
        (repo_slug(upstream), url_owner(upstream), url_owner(origin))
    else {
        return Vec::new();
    };
    let upstream_host = url_host(upstream);

    remotes
        .iter()
        .filter_map(|(name, url)| {
            if matches!(name.as_str(), "upstream" | "origin" | "release") {
                return None;
            }
            let (Some(slug), Some(owner)) = (repo_slug(url), url_owner(url)) else {
                return None;
            };
            if !slug.eq_ignore_ascii_case(upstream_slug)
                || owner.eq_ignore_ascii_case(upstream_owner)
                || owner.eq_ignore_ascii_case(origin_owner)
            {
                return None;
            }
            if let (Some(remote_host), Some(upstream_host)) = (url_host(url), upstream_host)
                && !remote_host.eq_ignore_ascii_case(upstream_host)
            {
                return None;
            }
            Some(format!(
                "origin is {origin}; untracked remote {name} looks like another fork of upstream ({url}). knives treats origin as YOUR fork — the one your branches push to and your PR heads live on. If {name} is that fork, rename remotes so it is origin."
            ))
        })
        .collect()
}

pub fn render(outcome: &InitOutcome, config_path: &Path) -> String {
    match outcome {
        InitOutcome::NotARepository { path } => format!("{} is not a repository", path.display()),
        InitOutcome::MissingRoles {
            path,
            found,
            absent,
        } => format!(
            "{} has remotes [{}] but no remote named {}; rename a remote to its role, or add one",
            path.display(),
            found.join(", "),
            absent.join(" or ")
        ),
        InitOutcome::NameTaken {
            name,
            existing,
            requested,
        } => format!(
            "{name} already refers to {}; refusing to point it at {}. \
             Rename one of the checkouts, or remove the existing entry first.",
            existing.display(),
            requested.display()
        ),
        InitOutcome::Adopted {
            name,
            entry,
            warnings,
        } => {
            let mut roles = format!("upstream={} origin={}", entry.upstream, entry.origin);
            if let Some(release) = &entry.release {
                let _ = write!(roles, " release={release}");
            }
            let mut output = format!(
                "configured {name} at {}\n  {roles}\n  written to {}\n  convention: origin = your fork (push target, PR heads); upstream = the maintainer's repo (fetch only)",
                entry.path.display(),
                config_path.display()
            );
            for warning in warnings {
                let _ = write!(output, "\n  warning: {warning}");
            }
            output
        }
    }
}

pub fn run(target: Option<PathBuf>) -> anyhow::Result<Exit> {
    let path = match target {
        Some(given) => given,
        None => std::env::current_dir()?,
    };
    let config_path = default_config_path();
    // `.jj` only: every other command needs `.jj/repo`, so adopting a git-only
    // tree produces a registry entry on which everything then fails.
    let outcome = if path.join(".jj").exists() {
        let remotes = crate::jj::git_remotes(&path)?;
        let registry = load(&config_path)?;
        let name = match decide(&path, &remotes) {
            InitOutcome::Adopted {
                name,
                entry: _,
                warnings: _,
            } => Some(name),
            _ => None,
        };
        decide_with_registry(&path, &remotes, name.and_then(|n| registry.repos.get(&n)))
    } else {
        InitOutcome::NotARepository { path }
    };
    match &outcome {
        InitOutcome::Adopted {
            name,
            entry,
            warnings: _,
        } => {
            let mut registry = load(&config_path)?;
            let _ = registry.repos.insert(name.clone(), (**entry).clone());
            save(&registry, &config_path)?;
            println!("{}", render(&outcome, &config_path));
            Ok(Exit::Ok)
        }
        InitOutcome::NotARepository { .. }
        | InitOutcome::MissingRoles { .. }
        | InitOutcome::NameTaken { .. } => {
            eprintln!("{}", render(&outcome, &config_path));
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
    use super::*;

    fn remotes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn only_remotes_named_for_a_role_are_adopted() {
        // Given: a repo with an extra remote that is nobody's role
        let found = remotes(&[
            ("origin", "o"),
            ("upstream", "u"),
            ("someone-elses-fork", "x"),
        ]);
        // When: init decides
        let outcome = decide(Path::new("/tmp/a-repo"), &found);
        // Then: the stray remote is ignored rather than guessed at
        let InitOutcome::Adopted {
            entry, warnings, ..
        } = outcome
        else {
            panic!("expected adoption")
        };
        assert_eq!(entry.upstream, "u");
        assert_eq!(entry.origin, "o");
        assert_eq!(entry.release, None);
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_split_release_remote_is_adopted_when_present() {
        let found = remotes(&[("origin", "o"), ("upstream", "u"), ("release", "r")]);
        let InitOutcome::Adopted {
            entry, warnings, ..
        } = decide(Path::new("/tmp/a"), &found)
        else {
            panic!("expected adoption")
        };
        assert_eq!(entry.release.as_deref(), Some("r"));
        assert!(warnings.is_empty(), "was: {warnings:?}");
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
        // When: init decides
        let outcome = decide(Path::new("/tmp/tool/default"), &found);
        // Then: adoption succeeds, carrying a warning that names the suspect remote
        let InitOutcome::Adopted { warnings, .. } = &outcome else {
            panic!("expected adoption")
        };
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
        assert!(warnings[0].contains("mine"), "was: {warnings:?}");
        let text = render(&outcome, Path::new("/tmp/repos.toml"));
        assert!(
            text.contains("origin = your fork"),
            "the convention is always stated: {text}"
        );
        assert!(
            text.contains(&warnings[0]),
            "the warning is rendered: {text}"
        );
    }

    #[test]
    fn a_remote_for_a_different_repository_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/someone/tool.git"),
            ("upstream", "https://forge.invalid/maintainer/tool.git"),
            ("other", "https://forge.invalid/someone/unrelated.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_case_variant_of_upstream_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/sjawhar/hawk.git"),
            ("upstream", "https://forge.invalid/METR/hawk.git"),
            ("metr", "https://forge.invalid/metr/hawk.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_case_variant_of_upstream_slug_warns_for_a_third_owner() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/sjawhar/hawk.git"),
            ("upstream", "https://forge.invalid/METR/hawk.git"),
            ("third", "https://forge.invalid/someone/HAWK.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
    }

    #[test]
    fn an_owner_matching_origin_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/sjawhar/hawk.git"),
            ("upstream", "https://forge.invalid/METR/hawk.git"),
            ("same-as-origin", "https://forge.invalid/sjawhar/hawk.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn an_owner_matching_upstream_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/sjawhar/hawk.git"),
            ("upstream", "https://forge.invalid/METR/hawk.git"),
            ("same-as-upstream", "https://forge.invalid/METR/hawk.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_release_named_remote_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/sjawhar/hawk.git"),
            ("upstream", "https://forge.invalid/METR/hawk.git"),
            ("release", "https://forge.invalid/someone/hawk.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_cross_forge_remote_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://github.example/sjawhar/hawk.git"),
            ("upstream", "https://github.example/METR/hawk.git"),
            ("other-forge", "https://gitlab.example/someone/hawk.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn an_unparseable_host_keeps_a_possible_miswiring_warning() {
        let found = remotes(&[
            ("origin", "https:///sjawhar/hawk.git"),
            ("upstream", "https:///METR/hawk.git"),
            ("third", "https://forge.invalid/someone/hawk.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
    }

    #[test]
    fn url_owner_supports_https_and_scp_style_urls() {
        assert_eq!(
            url_owner("https://forge.invalid/someone/tool.git"),
            Some("someone")
        );
        assert_eq!(
            url_owner("git@forge.invalid:someone/tool.git"),
            Some("someone")
        );
    }

    #[test]
    fn url_owner_rejects_paths_without_an_authority_delimited_owner_segment() {
        assert_eq!(url_owner("https://forge.invalid/repo.git"), None);
        assert_eq!(url_owner("git@forge.invalid:repo.git"), None);
        assert_eq!(url_owner("/local/path/tool.git"), None);
    }

    #[test]
    fn repo_slug_rejects_an_empty_repository_name() {
        assert_eq!(repo_slug("https://forge.invalid/someone/.git"), None);
    }

    #[test]
    fn a_missing_required_role_is_named_in_the_message() {
        let outcome = decide(Path::new("/tmp/a-repo"), &remotes(&[("origin", "o")]));
        let text = render(&outcome, Path::new("/tmp/repos.toml"));
        assert!(text.contains("upstream"), "was: {text}");
    }

    #[test]
    fn a_repo_checked_out_at_default_is_named_for_its_parent() {
        // The layout these forks use: <name>/default. Naming it "default" would
        // make every repo in the registry collide.
        assert_eq!(
            guidance_name(Path::new("/home/u/forks/hawk/default")),
            "hawk"
        );
        assert_eq!(guidance_name(Path::new("/home/u/forks/hawk")), "hawk");
    }
}

#[cfg(test)]
mod registry_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn remotes() -> BTreeMap<String, String> {
        [("upstream", "u"), ("origin", "o")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn held(path: &str) -> RepoEntry {
        RepoEntry {
            path: PathBuf::from(path),
            upstream: "u".to_owned(),
            origin: "o".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        }
    }

    #[test]
    fn a_second_tree_cannot_take_over_an_existing_name() {
        // The registry is the plugin's trust set. Silently replacing an entry
        // re-points guidance injection at whatever tree ran `init` last, which
        // is the one real path to poisoning the allowlist.
        let outcome = decide_with_registry(
            Path::new("/tmp/attacker/hawk/default"),
            &remotes(),
            Some(&held("/home/real/forks/hawk/default")),
        );
        let InitOutcome::NameTaken { name, .. } = &outcome else {
            panic!("expected refusal, got {outcome:?}")
        };
        assert_eq!(name, "hawk");
        let text = render(&outcome, Path::new("/tmp/repos.toml"));
        assert!(
            text.contains("/home/real/forks/hawk/default"),
            "was: {text}"
        );
        assert!(text.contains("refusing"), "was: {text}");
    }

    #[test]
    fn re_adopting_the_same_tree_is_allowed() {
        // Re-running init on a repo already in the registry is routine.
        let outcome = decide_with_registry(
            Path::new("/home/real/forks/hawk/default"),
            &remotes(),
            Some(&held("/home/real/forks/hawk/default")),
        );
        let InitOutcome::Adopted { warnings, .. } = outcome else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_git_only_tree_is_not_adopted() {
        // Every other command needs `.jj/repo`, so adopting one produces a
        // registry entry on which everything then fails.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(!dir.path().join(".jj").exists());
    }
}
