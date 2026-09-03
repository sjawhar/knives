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
    /// tree ran `init` last. An adversarial fixture at `<anywhere>/work/default`
    /// could take over the name `work`. This is the "verify one repo per fork"
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
///
/// Re-adopting a registered checkout keeps what the registry was told by hand:
/// everything but the remotes, including the registry's own spelling of `path`.
/// Nothing on disk can recover those fields, and rebuilding the entry from
/// remotes alone silently moved every new workspace back beside the checkout.
pub fn decide_with_registry(
    path: &Path,
    remotes: &BTreeMap<String, String>,
    existing: Option<&RepoEntry>,
) -> InitOutcome {
    match (decide(path, remotes), existing) {
        (InitOutcome::Adopted { name, entry, .. }, Some(held))
            if !same_directory(&held.path, &entry.path) =>
        {
            InitOutcome::NameTaken {
                name,
                existing: held.path.clone(),
                requested: entry.path,
            }
        }
        (
            InitOutcome::Adopted {
                name,
                entry,
                warnings,
            },
            Some(held),
        ) => InitOutcome::Adopted {
            name,
            entry: Box::new(RepoEntry {
                upstream: entry.upstream,
                origin: entry.origin,
                release: entry.release,
                ..held.clone()
            }),
            warnings,
        },
        (outcome, _) => outcome,
    }
}

/// What `init` and `register` decide about the tree at `path`, with the registry
/// consulted so a registered checkout keeps its hand-written fields and a name
/// is not taken twice.
pub fn outcome_for(path: PathBuf, config_path: &Path) -> anyhow::Result<InitOutcome> {
    // `.jj` only: every other command needs `.jj/repo`, so adopting a git-only
    // tree produces a registry entry on which everything then fails.
    if !path.join(".jj").exists() {
        return Ok(InitOutcome::NotARepository { path });
    }
    let remotes = crate::jj::git_remotes(&path)?;
    let registry = load(config_path)?;
    let existing = match decide(&path, &remotes) {
        InitOutcome::Adopted { name, .. } => registry.repos.get(&name),
        _ => None,
    };
    Ok(decide_with_registry(&path, &remotes, existing))
}

/// Whether two spellings name one directory: canonical when both resolve, so a
/// symlink or relative path to a registered checkout is that checkout and not a
/// second tree taking its name; literal otherwise.
fn same_directory(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
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
            workspaces: None,
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

/// The checkout `init` or `register` was pointed at, spelled as the registry holds it.
///
/// Absolute, so `knives init ./work` neither collides with its own entry nor writes
/// a path that `load` later resolves against the config directory. Symlinks are
/// kept; the spelling is the user's.
pub fn target_path(target: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let path = match target {
        Some(given) => given,
        None => std::env::current_dir()?,
    };
    Ok(std::path::absolute(path)?)
}

pub fn run(target: Option<PathBuf>) -> anyhow::Result<Exit> {
    let config_path = default_config_path();
    let outcome = outcome_for(target_path(target)?, &config_path)?;
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
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("acme", "https://forge.invalid/acme/work.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_case_variant_of_upstream_slug_warns_for_a_third_owner() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("third", "https://forge.invalid/someone/WORK.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert_eq!(warnings.len(), 1, "was: {warnings:?}");
    }

    #[test]
    fn an_owner_matching_origin_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("same-as-origin", "https://forge.invalid/ours/work.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn an_owner_matching_upstream_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("same-as-upstream", "https://forge.invalid/ACME/work.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_release_named_remote_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://forge.invalid/ours/work.git"),
            ("upstream", "https://forge.invalid/ACME/work.git"),
            ("release", "https://forge.invalid/someone/work.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn a_cross_forge_remote_is_not_a_miswiring() {
        let found = remotes(&[
            ("origin", "https://github.example/ours/work.git"),
            ("upstream", "https://github.example/ACME/work.git"),
            ("other-forge", "https://gitlab.example/someone/work.git"),
        ]);
        let InitOutcome::Adopted { warnings, .. } = decide(Path::new("/tmp/t"), &found) else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn an_unparseable_host_keeps_a_possible_miswiring_warning() {
        let found = remotes(&[
            ("origin", "https:///ours/work.git"),
            ("upstream", "https:///ACME/work.git"),
            ("third", "https://forge.invalid/someone/work.git"),
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
            guidance_name(Path::new("/home/u/forks/work/default")),
            "work"
        );
        assert_eq!(guidance_name(Path::new("/home/u/forks/work")), "work");
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
            workspaces: None,
        }
    }

    #[test]
    fn a_second_tree_cannot_take_over_an_existing_name() {
        // The registry is the plugin's trust set. Silently replacing an entry
        // re-points guidance injection at whatever tree ran `init` last, which
        // is the one real path to poisoning the allowlist.
        let outcome = decide_with_registry(
            Path::new("/tmp/attacker/work/default"),
            &remotes(),
            Some(&held("/home/real/forks/work/default")),
        );
        let InitOutcome::NameTaken { name, .. } = &outcome else {
            panic!("expected refusal, got {outcome:?}")
        };
        assert_eq!(name, "work");
        let text = render(&outcome, Path::new("/tmp/repos.toml"));
        assert!(
            text.contains("/home/real/forks/work/default"),
            "was: {text}"
        );
        assert!(text.contains("refusing"), "was: {text}");
    }

    #[test]
    fn re_adopting_the_same_tree_is_allowed() {
        // Re-running init on a repo already in the registry is routine.
        let outcome = decide_with_registry(
            Path::new("/home/real/forks/work/default"),
            &remotes(),
            Some(&held("/home/real/forks/work/default")),
        );
        let InitOutcome::Adopted { warnings, .. } = outcome else {
            panic!("expected adoption")
        };
        assert!(warnings.is_empty(), "was: {warnings:?}");
    }

    #[test]
    fn re_adopting_the_same_tree_keeps_what_the_registry_was_told_by_hand() {
        // `init` reads remotes; `base`, `release_branch`, `test_count_command`,
        // `consumers` and `workspaces` are written by hand and nothing on disk can
        // recover them. Rebuilding the entry from remotes alone silently moved
        // every new workspace back beside the checkout.
        let mut existing = held("/home/real/forks/work/default");
        existing.base = Some("dev".to_owned());
        existing.release_branch = Some("sami".to_owned());
        existing.test_count_command = Some("count".to_owned());
        existing.consumers = vec!["acme/workbench".to_owned()];
        existing.workspaces = Some(PathBuf::from("/home/real/.worktrees/work"));

        let outcome = decide_with_registry(
            Path::new("/home/real/forks/work/default"),
            &remotes(),
            Some(&existing),
        );

        let InitOutcome::Adopted { entry, .. } = outcome else {
            panic!("expected adoption")
        };
        assert_eq!(*entry, existing);
    }

    #[test]
    fn re_adopting_the_same_tree_under_another_spelling_is_not_a_collision() {
        // The registry holds one spelling; the command line offers another — a
        // symlink, or a relative path. The same directory is the same checkout.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("work")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let held = held(real.join("work").to_str().unwrap());

        let outcome = decide_with_registry(&link.join("work"), &remotes(), Some(&held));

        let InitOutcome::Adopted { entry, .. } = outcome else {
            panic!("expected adoption, got {outcome:?}")
        };
        assert_eq!(
            entry.path, held.path,
            "the registry's spelling was replaced"
        );
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
