//! The registry of managed repositories.
//!
//! Everything user-specific is configuration. No user, organisation, or
//! repository name appears in this crate; remotes are addressed by role.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ids::RepoName;

/// What a remote is for, rather than what it is called.
///
/// An enum, so an unknown role cannot be requested and no runtime lookup error
/// is needed. `Release` is optional and falls back to `Origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// The repository we contribute to. Fetch only, including pull refs.
    Upstream,
    /// Where our branches get pushed.
    Origin,
    /// Where dated releases get pushed. Defaults to [`Role::Origin`].
    Release,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Upstream => "upstream",
            Self::Origin => "origin",
            Self::Release => "release",
        };
        f.write_str(text)
    }
}

/// One repository as it appears on disk in the registry.
///
/// `upstream` and `origin` are required by the type, so a registry missing
/// either fails to parse rather than failing later at the first query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub path: PathBuf,
    pub upstream: String,
    pub origin: String,
    /// The branch upstream expects pull requests against. Defaults to `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// A command whose output ends in the number of tests, used to check that a
    /// release cut did not drop a branch's tests.
    ///
    /// Per repo because counting tests is repo-specific: there is no portable
    /// way to ask an arbitrary project how many tests it has. Absent means the
    /// check is reported as not configured, never as passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_count_command: Option<String>,
    /// Checkouts that pin this repo's releases, so pinned-versus-newest can be answered
    /// without being asked.
    ///
    /// A list, because a fork can be consumed by several things at once and they can sit
    /// on different releases — which is the case the pin logic already reasoned about
    /// while the registry could only record one of them. Recorded here rather than passed
    /// as a flag every time, because the fact worth knowing is that a consumer is behind,
    /// and nobody runs a command to discover a question they have not thought of. Empty
    /// is normal: not every fork is consumed by something we also check out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<PathBuf>,
}

impl RepoEntry {
    /// The repository's location, resolved the same way the plugin resolves it.
    ///
    /// The two sides used to disagree: the plugin expanded `~` and resolved a
    /// relative path against the config home, while this side used the value
    /// verbatim, i.e. relative to the process's current directory. A
    /// `path = "~/repos/x"` entry was therefore a working allowlist entry and a
    /// broken CLI entry, so the trust set and the tool covered different
    /// directories. One rule now, and it is the plugin's.
    pub fn resolved_path(&self, config_home: &Path) -> PathBuf {
        expand_registry_path(&self.path, config_home)
    }

    /// The remote for a role. Total: every role resolves.
    pub fn remote(&self, role: Role) -> &str {
        match role {
            Role::Upstream => &self.upstream,
            Role::Origin => &self.origin,
            Role::Release => self.release.as_deref().unwrap_or(&self.origin),
        }
    }

    /// The branch a pull request from this repo should target.
    ///
    /// Configurable because not every upstream calls its default branch `main`.
    pub fn default_base(&self) -> &str {
        self.base.as_deref().unwrap_or("main")
    }

    /// Whether releases live somewhere other than our branches.
    pub const fn has_split_release(&self) -> bool {
        self.release.is_some()
    }
}

/// A repository whose agent instructions we trust, but which we do not maintain.
///
/// Deliberately its own section rather than a fork entry with optional remotes.
/// `RepoEntry` requires `upstream` and `origin` so that a malformed fork entry
/// fails at parse time; relaxing them to fit a repository that has no upstream
/// would trade that for a failure at the first query instead. A company repo
/// with nothing to contribute upstream is a different kind of thing, so it gets
/// a different shape: a path, and nothing else to get wrong.
///
/// No fork command reads these. They exist so the plugin can surface a
/// repository's own instructions when an agent reads its files, which is
/// useful well beyond the set of repositories we maintain forks of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedEntry {
    pub path: PathBuf,
}

/// Whether a guidance root comes from a maintained fork or trusted instructions.
///
/// The distinction survives resolution because callers may surface contribution
/// guidance for either kind without treating a trusted repository as a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceRootKind {
    /// A maintained fork declared under `[repos.*]`.
    Managed,
    /// A repository whose instructions are trusted under `[trusted.*]`.
    Trusted,
}

/// A canonical repository root eligible to provide agent guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceRoot {
    pub name: String,
    pub root: PathBuf,
    pub kind: GuidanceRootKind,
}

impl TrustedEntry {
    /// Resolved exactly as `RepoEntry::resolved_path` resolves, and for the same
    /// reason: the CLI and the plugin must agree on which directory an entry names.
    pub fn resolved_path(&self, config_home: &Path) -> PathBuf {
        expand_registry_path(&self.path, config_home)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub repos: BTreeMap<String, RepoEntry>,
    /// Trusted but unmaintained. Present on this type, rather than ignored as an
    /// unknown section, because `save` rewrites the whole file: a section serde
    /// does not know about would be silently deleted the next time `init` runs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trusted: BTreeMap<String, TrustedEntry>,
}

impl Registry {
    pub fn get(&self, name: &RepoName) -> Option<&RepoEntry> {
        self.repos.get(name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    /// Return the existing registry entries that may provide guidance.
    ///
    /// Resolve and skip each entry independently: a moved checkout must not disable
    /// guidance for the remaining registered repositories.
    pub fn guidance_roots(&self) -> Vec<GuidanceRoot> {
        let managed = self.repos.iter().filter_map(|(name, entry)| {
            entry.path.canonicalize().ok().map(|root| GuidanceRoot {
                name: name.clone(),
                root,
                kind: GuidanceRootKind::Managed,
            })
        });
        let trusted = self.trusted.iter().filter_map(|(name, entry)| {
            entry.path.canonicalize().ok().map(|root| GuidanceRoot {
                name: name.clone(),
                root,
                kind: GuidanceRootKind::Trusted,
            })
        });
        managed.chain(trusted).collect()
    }

    /// The managed repo that contains `path`, if any.
    ///
    /// Longest root wins, so a repo checked out inside another repo resolves to the
    /// inner one. Containment is by path components, never by string prefix: a
    /// sibling directory named `<root>-2` shares the prefix and is a different
    /// repository, which is the same trap the plugin's `isInside` avoids.
    pub fn containing(&self, path: &Path) -> Option<(RepoName, &RepoEntry)> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        self.repos
            .iter()
            .filter(|(_, entry)| {
                let root = entry
                    .path
                    .canonicalize()
                    .unwrap_or_else(|_| entry.path.clone());
                canonical == root || canonical.strip_prefix(&root).is_ok()
            })
            .max_by_key(|(_, entry)| entry.path.components().count())
            .map(|(name, entry)| (RepoName::new(name.clone()), entry))
    }

    pub fn names(&self) -> impl Iterator<Item = RepoName> + '_ {
        self.repos.keys().map(|name| RepoName::new(name.clone()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a valid registry: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("serialising the registry: {source}")]
    Serialise {
        #[from]
        source: toml::ser::Error,
    },
}

/// Where the registry lives.
///
/// `KNIVES_CONFIG_HOME` wins over `XDG_CONFIG_HOME` so this tool can be pointed
/// elsewhere without moving every other tool's config too. Redirecting
/// `XDG_CONFIG_HOME` to isolate this tool also hides the forge CLI's
/// credentials, which turns a working setup into an authentication failure.
/// Expand `~` and resolve a relative registry path against the config home.
pub fn expand_registry_path(path: &Path, config_home: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if path.is_absolute() {
        return path.to_owned();
    }
    config_home.join(path)
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/"), PathBuf::from)
}

pub fn default_config_path() -> PathBuf {
    if let Ok(home) = std::env::var("KNIVES_CONFIG_HOME") {
        return PathBuf::from(home).join("repos.toml");
    }
    let base = std::env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            // Never the current directory: that silently relocates the trust
            // set, and the registry decides which repositories the plugin will
            // inject guidance from.
            std::env::var("HOME")
                .map_or_else(|_| PathBuf::from("/nonexistent"), PathBuf::from)
                .join(".config")
        },
        PathBuf::from,
    );
    base.join("knives").join("repos.toml")
}

/// Read the registry. A missing file is an empty registry, which the commands
/// surface explicitly rather than treating as "nothing to do".
pub fn load(path: &Path) -> Result<Registry, ConfigError> {
    if !path.exists() {
        return Ok(Registry::default());
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut registry: Registry = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    // Resolve once, here, so no caller can accidentally use the raw value and
    // end up pointed somewhere other than the plugin's allowlist.
    let home = path.parent().unwrap_or_else(|| Path::new(".")).to_owned();
    for entry in registry.repos.values_mut() {
        entry.path = entry.resolved_path(&home);
        // Consumer paths get the same treatment as the repo path. Leaving `~` unexpanded
        // made a recorded consumer read as pinning nothing, which is the same wrong answer
        // as having no consumer at all.
        entry.consumers = entry
            .consumers
            .iter()
            .map(|consumer| expand_registry_path(consumer, &home))
            .collect();
    }
    for entry in registry.trusted.values_mut() {
        entry.path = entry.resolved_path(&home);
    }
    Ok(registry)
}

pub fn save(registry: &Registry, path: &Path) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(registry)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| ConfigError::Write {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    const SAMPLE: &str = r#"
[repos.example]
path = "/tmp/example"
upstream = "https://example.invalid/upstream.git"
origin = "https://example.invalid/origin.git"

[repos.split]
path = "/tmp/split"
upstream = "https://example.invalid/upstream2.git"
origin = "https://example.invalid/branches.git"
release = "https://example.invalid/releases.git"
"#;

    fn write(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("repos.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_registry_parses_one_entry_per_repo() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load(&write(dir.path(), SAMPLE)).unwrap();
        assert_eq!(registry.repos.len(), 2);
        assert_eq!(
            registry.repos["example"].path,
            PathBuf::from("/tmp/example")
        );
    }

    #[test]
    fn the_release_role_falls_back_to_origin() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load(&write(dir.path(), SAMPLE)).unwrap();
        let entry = &registry.repos["example"];
        assert_eq!(entry.remote(Role::Release), entry.remote(Role::Origin));
        assert!(!entry.has_split_release());
    }

    #[test]
    fn the_expected_base_is_the_trunk_unless_stated() {
        // A pull request against our own fork's main never reaches the maintainer, so the
        // expected base has to be knowable. Configurable because not every upstream calls
        // its default branch main.
        let dir = tempfile::tempdir().unwrap();
        let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "main");

        let stated = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                      base = \"develop\"\n";
        let registry = load(&write(dir.path(), stated)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "develop");
    }

    #[test]
    fn a_split_release_remote_is_used_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load(&write(dir.path(), SAMPLE)).unwrap();
        let entry = &registry.repos["split"];
        assert_eq!(
            entry.remote(Role::Release),
            "https://example.invalid/releases.git"
        );
        assert!(entry.has_split_release());
    }

    #[test]
    fn a_registry_missing_a_required_role_fails_to_parse() {
        // Given: an entry with no upstream
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.broken]\npath = \"/tmp/b\"\norigin = \"o\"\n";
        // When: it is loaded
        let result = load(&write(dir.path(), text));
        // Then: it fails at parse time, naming the field, not later at query time
        let message = result.unwrap_err().to_string();
        assert!(message.contains("upstream"), "message was: {message}");
    }

    #[test]
    fn the_repo_containing_a_directory_is_found_and_a_sibling_is_not() {
        // Requiring the repo name on every command is absurd when you are standing in
        // the repository. Prefix matching would be worse than nothing: `<root>-2` is a
        // different repository that shares the prefix.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("managed");
        let sibling = dir.path().join("managed-2");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let text = format!(
            "[repos.managed]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n",
            root.display()
        );
        let registry = load(&write(dir.path(), &text)).unwrap();

        assert_eq!(
            registry.containing(&root.join("src")).map(|(n, _)| n),
            Some(RepoName::new("managed"))
        );
        assert_eq!(
            registry.containing(&root).map(|(n, _)| n),
            Some(RepoName::new("managed"))
        );
        assert!(
            registry.containing(&sibling).is_none(),
            "a sibling is not inside"
        );
        assert!(registry.containing(dir.path()).is_none());
    }

    #[test]
    fn consumer_paths_are_expanded_like_the_repo_path() {
        // An unexpanded `~` made a recorded consumer look like one pinning nothing,
        // which reads identically to having no consumer at all.
        unsafe { std::env::set_var("HOME", "/home/someone") };
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\npath = \"/tmp/demo\"\nupstream = \"u\"\norigin = \"o\"\n\
                    consumers = [\"~/one/default\", \"~/two/default\"]\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.repos["demo"].consumers,
            vec![
                PathBuf::from("/home/someone/one/default"),
                PathBuf::from("/home/someone/two/default")
            ],
            "a fork can be consumed by several things at once"
        );
    }

    #[test]
    fn a_trusted_entry_needs_only_a_path() {
        // Given: a repository we do not maintain and have no upstream for
        let dir = tempfile::tempdir().unwrap();
        let text = "[trusted.workbench]\npath = \"/tmp/workbench\"\n";
        // When: the registry is loaded
        let registry = load(&write(dir.path(), text)).unwrap();
        // Then: it parses, with no remotes demanded of it
        assert_eq!(
            registry.trusted["workbench"].path,
            PathBuf::from("/tmp/workbench")
        );
    }

    #[test]
    fn a_trusted_entry_is_invisible_to_fork_commands() {
        // The parse-time guarantee that a fork entry carries its remotes only holds
        // if trusted entries never reach the code that assumes it.
        let dir = tempfile::tempdir().unwrap();
        let text = "[trusted.workbench]\npath = \"/tmp/workbench\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert!(registry.get(&RepoName::new("workbench")).is_none());
        assert!(registry.repos.is_empty());
    }

    #[test]
    fn guidance_roots_preserve_managed_and_trusted_kinds() {
        // Given: one managed fork and one trusted repository that both exist.
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        let trusted = dir.path().join("trusted");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::create_dir_all(&trusted).unwrap();
        let text = format!(
            "[repos.managed]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [trusted.workbench]\npath = \"{}\"\n",
            managed.display(),
            trusted.display()
        );
        let registry = load(&write(dir.path(), &text)).unwrap();

        // When: their guidance roots are collected.
        let roots = registry.guidance_roots();

        // Then: the roots retain their distinct registry roles.
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|root| {
            root.name == "managed"
                && root.root == managed.canonicalize().unwrap()
                && root.kind == GuidanceRootKind::Managed
        }));
        assert!(roots.iter().any(|root| {
            root.name == "workbench"
                && root.root == trusted.canonicalize().unwrap()
                && root.kind == GuidanceRootKind::Trusted
        }));
    }

    #[test]
    fn guidance_roots_skip_an_unresolvable_entry_without_dropping_others() {
        // Given: one existing managed fork and one trusted checkout that was moved away.
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        let missing = dir.path().join("missing");
        let text = format!(
            "[repos.managed]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [trusted.moved]\npath = \"{}\"\n",
            managed.display(),
            missing.display()
        );
        let registry = load(&write(dir.path(), &text)).unwrap();

        // When: roots are resolved from the registry.
        let roots = registry.guidance_roots();

        // Then: the moved entry is skipped without disabling the existing one.
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "managed");
        assert_eq!(roots[0].kind, GuidanceRootKind::Managed);
    }

    #[test]
    fn a_trusted_tilde_path_resolves_like_a_fork_path() {
        unsafe { std::env::set_var("HOME", "/home/someone") };
        let dir = tempfile::tempdir().unwrap();
        let text = "[trusted.workbench]\npath = \"~/workbench/default\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(
            registry.trusted["workbench"].path,
            PathBuf::from("/home/someone/workbench/default")
        );
    }

    #[test]
    fn saving_preserves_trusted_entries() {
        // `init` rewrites the whole file. Before `trusted` existed on the type,
        // serde ignored the section on read and `save` then wrote it away.
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.demo]\npath = \"/tmp/demo\"\nupstream = \"u\"\norigin = \"o\"\n\n\
                    [trusted.workbench]\npath = \"/tmp/workbench\"\n";
        let path = write(dir.path(), text);
        let registry = load(&path).unwrap();

        save(&registry, &path).unwrap();

        let reloaded = load(&path).unwrap();
        assert_eq!(
            reloaded.trusted["workbench"].path,
            PathBuf::from("/tmp/workbench")
        );
        assert!(reloaded.repos.contains_key("demo"));
    }

    #[test]
    fn a_tilde_path_resolves_the_same_way_the_plugin_resolves_it() {
        // The two sides disagreeing meant `path = "~/repos/x"` was a working
        // allowlist entry and a broken CLI entry, so the trust set and the tool
        // covered different directories.
        unsafe { std::env::set_var("HOME", "/home/someone") };
        assert_eq!(
            expand_registry_path(Path::new("~/repos/x"), Path::new("/cfg")),
            PathBuf::from("/home/someone/repos/x")
        );
        assert_eq!(
            expand_registry_path(Path::new("~"), Path::new("/cfg")),
            PathBuf::from("/home/someone")
        );
    }

    #[test]
    fn a_relative_path_resolves_against_the_config_home_not_the_cwd() {
        assert_eq!(
            expand_registry_path(Path::new("repos/x"), Path::new("/cfg")),
            PathBuf::from("/cfg/repos/x")
        );
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        assert_eq!(
            expand_registry_path(Path::new("/abs/x"), Path::new("/cfg")),
            PathBuf::from("/abs/x")
        );
    }

    #[test]
    fn load_resolves_every_entry_so_no_caller_sees_a_raw_value() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[repos.rel]\npath = \"sub/repo\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), text)).unwrap();
        assert_eq!(registry.repos["rel"].path, dir.path().join("sub/repo"));
    }

    #[test]
    fn a_missing_file_is_an_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("absent.toml")).unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let original = load(&write(dir.path(), SAMPLE)).unwrap();
        let target = dir.path().join("out").join("repos.toml");
        save(&original, &target).unwrap();
        assert_eq!(load(&target).unwrap(), original);
    }
}
