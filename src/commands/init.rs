//! `knives init`: adopt a repository into the registry.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::config::{RepoEntry, default_config_path, load, save};

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
    if let InitOutcome::Adopted { name, entry } = &outcome
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
        name: repo_name(path),
        entry: Box::new(RepoEntry {
            path: path.to_owned(),
            upstream: upstream.clone(),
            origin: origin.clone(),
            base: None,
            release: remotes.get("release").cloned(),
            test_count_command: None,
            consumers: Vec::new(),
        }),
    }
}

/// A repository checked out at `<name>/default` is named for its parent, which
/// is the layout these forks actually use.
fn repo_name(path: &Path) -> String {
    let last = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    if last == "default" {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or(last)
            .to_owned();
    }
    last.to_owned()
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
        InitOutcome::Adopted { name, entry } => {
            let mut roles = format!("upstream={} origin={}", entry.upstream, entry.origin);
            if let Some(release) = &entry.release {
                let _ = write!(roles, " release={release}");
            }
            format!(
                "configured {name} at {}\n  {roles}\n  written to {}",
                entry.path.display(),
                config_path.display()
            )
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
            InitOutcome::Adopted { name, .. } => Some(name),
            _ => None,
        };
        decide_with_registry(&path, &remotes, name.and_then(|n| registry.repos.get(&n)))
    } else {
        InitOutcome::NotARepository { path }
    };
    match &outcome {
        InitOutcome::Adopted { name, entry } => {
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
        let InitOutcome::Adopted { entry, .. } = outcome else {
            panic!("expected adoption")
        };
        assert_eq!(entry.upstream, "u");
        assert_eq!(entry.origin, "o");
        assert_eq!(entry.release, None);
    }

    #[test]
    fn a_split_release_remote_is_adopted_when_present() {
        let found = remotes(&[("origin", "o"), ("upstream", "u"), ("release", "r")]);
        let InitOutcome::Adopted { entry, .. } = decide(Path::new("/tmp/a"), &found) else {
            panic!("expected adoption")
        };
        assert_eq!(entry.release.as_deref(), Some("r"));
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
        assert_eq!(repo_name(Path::new("/home/u/forks/hawk/default")), "hawk");
        assert_eq!(repo_name(Path::new("/home/u/forks/hawk")), "hawk");
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
        assert!(matches!(outcome, InitOutcome::Adopted { .. }));
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
