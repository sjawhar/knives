//! `knives register`: print a paste-ready registry snippet.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::cli::Exit;
use crate::commands::init::{self, InitOutcome};
use crate::config::{ConfigError, Registry, RepoEntry, default_config_path};

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

/// Prints a registry snippet without changing the registry.
///
/// # Errors
///
/// Returns errors from discovering the repository's remotes or serializing its
/// entry.
pub fn run(target: Option<PathBuf>) -> anyhow::Result<Exit> {
    let path = match target {
        Some(given) => given,
        None => std::env::current_dir()?,
    };
    let config_path = default_config_path();
    let outcome = if path.join(".jj").exists() {
        init::decide(&path, &crate::jj::git_remotes(&path)?)
    } else {
        InitOutcome::NotARepository { path }
    };

    match &outcome {
        InitOutcome::Adopted {
            name,
            entry,
            warnings,
        } => {
            for warning in warnings {
                eprintln!("  warning: {warning}");
            }
            print!(
                "{}",
                snippet(name, entry).map_err(|source| ConfigError::Serialise { source })?
            );
            eprintln!(
                "paste this into {} to register; replace any existing [repos.{name}] entry rather than appending a duplicate; hooks reload the registry on every event, so it takes effect on the next tool call",
                config_path.display()
            );
            Ok(Exit::Ok)
        }
        InitOutcome::NotARepository { .. } | InitOutcome::MissingRoles { .. } => {
            eprintln!("{}", init::render(&outcome, &config_path));
            Ok(Exit::Usage)
        }
        InitOutcome::NameTaken { .. } => Err(anyhow::anyhow!(
            "init::decide returned a registry-collision outcome without reading a registry"
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::snippet;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;

    #[test]
    fn the_snippet_is_valid_toml_that_round_trips_into_a_registry_entry() {
        // The whole command is "print what a human would paste"; a snippet that
        // does not parse back into the same entry is worse than no command.
        let entry = crate::config::RepoEntry {
            path: std::path::PathBuf::from("/home/someone/forks/tool/default"),
            upstream: "https://forge.invalid/maintainer/tool.git".to_owned(),
            origin: "https://forge.invalid/someone/tool.git".to_owned(),
            base: Some("integration".to_owned()),
            release: Some("https://forge.invalid/someone/tool-releases.git".to_owned()),
            release_branch: Some("release".to_owned()),
            test_count_command: Some("cargo test -- --list | wc -l".to_owned()),
            consumers: vec![std::path::PathBuf::from("/home/someone/consumers/tool")],
        };
        let text = snippet("tool", &entry).expect("snippet serializes");
        assert!(text.starts_with("[repos.tool]"), "was: {text}");
        let parsed: crate::config::Registry = toml::from_str(&text).expect("snippet parses");
        assert_eq!(parsed.repos["tool"], entry);
    }

    #[cfg(unix)]
    #[test]
    fn the_snippet_returns_an_error_for_a_non_utf8_path() {
        let entry = crate::config::RepoEntry {
            path: std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![0xff])),
            upstream: "https://forge.invalid/maintainer/tool.git".to_owned(),
            origin: "https://forge.invalid/someone/tool.git".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        };

        assert!(snippet("tool", &entry).is_err());
    }
}
