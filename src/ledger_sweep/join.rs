//! Joining a ledger the registry names and no git directory here reaches.
//!
//! A machine whose `repos.toml` shares a fork's ledger, and that nobody set
//! up to carry it, would otherwise have that fork's entries travel nowhere
//! and no claim of it held anywhere but here. So knives makes the git
//! directory itself, as a machine is set up by hand to share a ledger:
//! `<owner>--<name>.git` in [`ledger_repositories`], over the ledger root,
//! whose `origin` is the ledger's repository on the fork's own forge, and
//! which names this machine. A directory already at that path is never
//! touched, and a dry run joins nothing.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::destinations::ledger_repositories;
use crate::config::Registry;
use crate::ids::UpstreamName;

/// The git directory joining `ledger`, `<owner>/<name>`, makes beside
/// `root`: `<owner>--<name>.git` in [`ledger_repositories`].
pub(super) fn joined_git_dir(root: &Path, ledger: &str) -> PathBuf {
    ledger_repositories(root).join(format!("{}.git", ledger.replacen('/', "--", 1)))
}

/// Why a fork whose ledger is `ledger` is carried nowhere although a git
/// directory stands at [`joined_git_dir`]: its origin names another
/// repository, and knives changes no git directory it did not make.
pub(super) fn occupied(root: &Path, ledger: &str) -> String {
    let git_dir = joined_git_dir(root, ledger);
    format!(
        "{} is there already, and its origin is not {ledger}; knives changes no git directory \
         it did not make. Give it {ledger} as its origin, or move it away so that knives joins \
         the ledger itself",
        git_dir.display()
    )
}

/// Join each ledger `unreached` names, the forks no git directory reaches
/// by the `<owner>/<name>` of the ledger each belongs to, saying each join
/// on `said`; why each one that could not be joined was not, by ledger.
pub(super) fn join_unreached(
    root: &Path,
    registry: &Registry,
    unreached: &BTreeMap<UpstreamName, String>,
    said: &mut dyn Write,
) -> BTreeMap<String, String> {
    let mut by_ledger: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (fork, ledger) in unreached {
        by_ledger.entry(ledger).or_default().push(fork.as_str());
    }
    let mut failed = BTreeMap::new();
    for (ledger, forks) in by_ledger {
        match join(root, registry, ledger, &forks) {
            Ok(Some(line)) => {
                let _ = writeln!(said, "{}", crate::ledger::inline_human_text(&line));
            }
            Ok(None) => {}
            Err(why) => {
                failed.insert(ledger.to_owned(), why);
            }
        }
    }
    failed
}

/// Make [`joined_git_dir`] for `ledger`, which `forks` name; the line that
/// says so, `None` when another command made it first, or why it could not
/// be made.
///
/// Its `origin` is the URL of a fork entry naming `ledger` ([`ledger_url`]),
/// its origin's else its upstream's, and it commits as [`this_machine`].
/// Nothing is made where something already stands at its path.
fn join(
    root: &Path,
    registry: &Registry,
    ledger: &str,
    forks: &[&str],
) -> Result<Option<String>, String> {
    let root =
        &std::path::absolute(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let git_dir = joined_git_dir(root, ledger);
    if git_dir.symlink_metadata().is_ok() {
        return Err(occupied(root, ledger));
    }
    let url = registry
        .repos
        .values()
        .filter(|entry| entry.shares_ledger_with(ledger))
        .find_map(|entry| {
            ledger_url(&entry.origin, ledger).or_else(|| ledger_url(&entry.upstream, ledger))
        })
        .ok_or_else(|| {
            format!(
                "neither the origin nor the upstream repos.toml gives {} is a remote URL to \
                 take {ledger}'s URL from",
                forks.join(", ")
            )
        })?;
    let machine = this_machine();
    Ok(make(root, &git_dir, &url, &machine)?.then(|| {
        format!(
            "joined the ledger {ledger}: made {} as machine {machine}, carrying {}",
            git_dir.display(),
            forks.join(", ")
        )
    }))
}

/// Make the git directory `git_dir` over the ledger root `root`, an
/// absolute path, whose `origin` is `url` and whose `knives.machine` is
/// `machine`; whether this call made it, rather than another command that
/// joined the same ledger at the same moment.
///
/// It is made whole in a scratch directory beside [`ledger_repositories`]
/// and only then renamed into it, because every command reads each
/// directory there as a git directory: half made, one with no origin yet
/// would carry nothing, and one with no machine would refuse every sweep.
/// Its `core.worktree` is the root, which is not made: a root that does not
/// exist yet holds no entries, and the first write or pull makes it.
fn make(root: &Path, git_dir: &Path, url: &str, machine: &str) -> Result<bool, String> {
    let failed = |cause: String| format!("could not make {}: {cause}", git_dir.display());
    let repositories = ledger_repositories(root);
    std::fs::create_dir_all(&repositories)
        .map_err(|error| failed(format!("making {}: {error}", repositories.display())))?;
    let home = repositories.parent().unwrap_or(root);
    let scratch = tempfile::Builder::new()
        .prefix(".knives-joining-")
        .tempdir_in(home)
        .map_err(|error| {
            failed(format!(
                "making a scratch directory in {}: {error}",
                home.display()
            ))
        })?;
    let made = scratch.path().join("ledger.git");
    let os = OsStr::new;
    git(&[os("init"), os("--quiet"), os("--bare"), made.as_os_str()]).map_err(&failed)?;
    for args in [
        [os("config"), os("core.bare"), os("false")].as_slice(),
        &[os("config"), os("core.worktree"), root.as_os_str()],
        &[os("remote"), os("add"), os("origin"), os(url)],
        &[os("config"), os("knives.machine"), os(machine)],
    ] {
        git(&[[os("--git-dir"), made.as_os_str()].as_slice(), args].concat()).map_err(&failed)?;
    }
    match std::fs::rename(&made, git_dir) {
        Ok(()) => Ok(true),
        // Another command joined first: what is there is its directory.
        Err(_) if git_dir.symlink_metadata().is_ok() => Ok(false),
        Err(error) => Err(failed(error.to_string())),
    }
}

/// Run `git <args>`; git's own words when it fails.
fn git(args: &[&OsStr]) -> Result<(), String> {
    crate::ledger_git::run(crate::bind::git_command().args(args))
        .map(drop)
        .map_err(|error| error.to_string())
}

/// `fork_url` with its repository path replaced by `ledger`, its
/// `<owner>/<name>`: the scheme, user, host and port kept as spelled, and a
/// `.git` suffix too when the path had one. `None` for a value that names no
/// repository on a host: a filesystem path, or a `file://` URL, whose
/// authority is empty.
///
/// A forge serves a fork and the ledger its owners share from one host, so
/// the fork's own URL says how this machine reaches the ledger, with the
/// credentials and the protocol it already uses there.
fn ledger_url(fork_url: &str, ledger: &str) -> Option<String> {
    let url = fork_url.trim().trim_end_matches('/');
    let (authority, path) = crate::remote_url::remote_authority_and_path(url).or_else(|| {
        // scp form without a user, `host:path`, as git reads it: no `/`
        // before the colon.
        let (host, path) = url.split_once(':')?;
        (!url.contains("://") && !host.is_empty() && !host.contains('/')).then_some((host, path))
    })?;
    let repository = path.trim_start_matches('/');
    if authority.is_empty() || repository.is_empty() {
        return None;
    }
    let kept = url.strip_suffix(repository)?;
    let suffix = repository
        .strip_prefix(crate::remote_url::without_git_suffix(repository))
        .unwrap_or_default();
    Some(format!("{kept}{ledger}{suffix}"))
}

/// This machine's name on a ledger it joins: [`machine_name`] of its host
/// name, its `/etc/machine-id` and a random number.
fn this_machine() -> String {
    let host = crate::liveness::host_name();
    let id = std::fs::read_to_string("/etc/machine-id").ok();
    machine_name(&host, id.as_deref(), crate::lock::random_u64())
}

/// `<short host name>-<8 hex digits>`, as machines set up by hand to share
/// a ledger are named: the host name up to its first `.`, and the first
/// eight characters of `machine_id` when they are hex digits, else eight
/// from `random`.
///
/// The name spells this machine's ref, `refs/knives/<machine>`, as one ref
/// component ([`crate::ledger_git::ref_component`]). So each character of
/// the host name no component can hold (a control character, a space, any
/// of `~^:?*[\/`) becomes `-`, as does the `{` of `@{`, and the name never
/// starts with `-`: a host name that would leave nothing gives the digits
/// alone.
fn machine_name(host: &str, machine_id: Option<&str>, random: u64) -> String {
    let short: String = host
        .split('.')
        .next()
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() || " ~^:?*[\\/".contains(character) {
                '-'
            } else {
                character
            }
        })
        .collect();
    let short = short.replace("@{", "@-");
    let short = short.trim_start_matches('-');
    let digits = machine_id
        .and_then(|id| id.get(..8))
        .filter(|id| id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(|| format!("{:08x}", random & 0xffff_ffff), str::to_owned);
    if short.is_empty() {
        digits
    } else {
        format!("{short}-{digits}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ledger_sweep::{destinations, unpullable};

    /// A config home with a registry naming `forks`, each `(key, origin,
    /// ledger)`: the fork `acme/<key>`, whose upstream is on the forge, and
    /// no ledger root or git directory at all, as a new machine has.
    fn new_machine(forks: &[(&str, &str, Option<&str>)]) -> (tempfile::TempDir, PathBuf, Registry) {
        use std::fmt::Write as _;
        let home = tempfile::tempdir().unwrap();
        let mut text = String::new();
        for (key, origin, ledger) in forks {
            let _ = writeln!(
                text,
                "[repos.{key}]\nupstream = \"https://forge.invalid/acme/{key}\"\n\
                 origin = \"{origin}\"\n{}",
                ledger.map_or_else(String::new, |ledger| format!("ledger = \"{ledger}\"\n"))
            );
        }
        let path = home.path().join("repos.toml");
        std::fs::write(&path, text).unwrap();
        let registry = crate::config::load(&path).unwrap();
        let root = home.path().join("ledger");
        (home, root, registry)
    }

    /// `git --git-dir <git_dir> config <key>`'s value, or `None`.
    fn config(git_dir: &Path, key: &str) -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["config", "--get", key])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    /// Every name in `directory`, or none when it does not exist.
    fn listed(directory: &Path) -> BTreeSet<String> {
        std::fs::read_dir(directory).map_or_else(
            |_| BTreeSet::new(),
            |listing| {
                listing
                    .map(|dirent| dirent.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            },
        )
    }

    #[test]
    fn a_ledger_no_git_directory_reaches_is_joined_once_and_then_carries_its_forks() {
        // Given: a new machine whose registry shares two forks' ledger with
        // acme/ledger, and one fork's not at all.
        let (home, root, registry) = new_machine(&[
            (
                "a-repo",
                "https://forge.invalid/ours/a-repo.git",
                Some("acme/ledger"),
            ),
            (
                "b-repo",
                "https://forge.invalid/ours/b-repo",
                Some("acme/ledger"),
            ),
            ("c-repo", "https://forge.invalid/ours/c-repo", None),
        ]);

        // When: where the ledger travels is read, as a sweep or a pull does.
        let mut said = Vec::new();
        let found =
            super::super::destinations::destinations_telling(&root, &registry, &mut said).unwrap();

        // Then: the git directory is made, over the root, reaching the
        // ledger on the fork's forge, and naming this machine.
        let git_dir = ledger_repositories(&root).join("acme--ledger.git");
        assert_eq!(config(&git_dir, "core.bare").as_deref(), Some("false"));
        assert_eq!(
            config(&git_dir, "core.worktree"),
            Some(std::path::absolute(&root).unwrap().display().to_string())
        );
        assert_eq!(
            config(&git_dir, "remote.origin.url").as_deref(),
            Some("https://forge.invalid/acme/ledger.git")
        );
        let machine = config(&git_dir, "knives.machine").unwrap();
        let (_, digits) = machine.rsplit_once('-').unwrap();
        assert_eq!(digits.len(), 8, "{machine}");
        assert!(
            digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{machine}"
        );
        crate::ledger_git::ref_component("machine", &machine).unwrap();
        // And: it says so in one line, naming the forks it carries.
        assert_eq!(
            String::from_utf8(said).unwrap(),
            format!(
                "joined the ledger acme/ledger: made {} as machine {machine}, carrying \
                 acme/a-repo, acme/b-repo\n",
                std::path::absolute(&git_dir).unwrap().display()
            )
        );
        // And: the directory is one of the destinations, carrying both forks,
        // and no fork is unreached; nothing else was made.
        assert!(found.unreached.is_empty(), "{:?}", found.unreached);
        let [destination] = found.found.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(destination.machine, machine);
        assert_eq!(
            destination.repository.forks(),
            &BTreeSet::from([
                UpstreamName::new("acme/a-repo"),
                UpstreamName::new("acme/b-repo")
            ])
        );
        assert!(!root.exists(), "the ledger root was made");
        assert_eq!(
            listed(home.path()),
            BTreeSet::from(["ledger-repositories".to_owned(), "repos.toml".to_owned()])
        );

        // When: it is read again.
        let mut again = Vec::new();
        let second =
            super::super::destinations::destinations_telling(&root, &registry, &mut again).unwrap();

        // Then: it finds the directory, makes nothing and says nothing.
        assert!(again.is_empty(), "{}", String::from_utf8_lossy(&again));
        assert_eq!(
            listed(&ledger_repositories(&root)),
            BTreeSet::from(["acme--ledger.git".to_owned()])
        );
        assert_eq!(second.found.len(), 1);
        assert_eq!(config(&git_dir, "knives.machine"), Some(machine));
    }

    #[test]
    fn a_dry_run_joins_nothing_and_says_the_run_would_join_the_ledger() {
        // Given: a new machine whose registry shares a fork's ledger.
        let (home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let fork = UpstreamName::new("acme/a-repo");

        // When: a dry run asks what a pull would hold back.
        let problems = unpullable(&root, &registry, &[&fork], &[]);

        // Then: nothing is made, and the fork's problem says the run that is
        // not a dry run joins its ledger first.
        assert_eq!(
            listed(home.path()),
            BTreeSet::from(["repos.toml".to_owned()])
        );
        let said = problems.get(&fork).unwrap().join("\n");
        assert!(
            said.contains("has acme/ledger as its origin yet; a run that is not a dry run joins"),
            "{said}"
        );
        assert!(
            said.contains(&joined_git_dir(&root, "acme/ledger").display().to_string()),
            "{said}"
        );
    }

    #[test]
    fn a_fork_with_no_forge_origin_takes_the_ledger_url_from_its_upstream() {
        // Given: a fork whose origin is a directory on this machine.
        let (_home, root, registry) =
            new_machine(&[("a-repo", "/srv/ours/a-repo", Some("acme/ledger"))]);

        // When: its ledger is joined.
        destinations(&root, &registry).unwrap();

        // Then: the ledger's URL is on the upstream's forge.
        assert_eq!(
            config(&joined_git_dir(&root, "acme/ledger"), "remote.origin.url").as_deref(),
            Some("https://forge.invalid/acme/ledger")
        );
    }

    #[test]
    fn the_ledger_url_is_the_fork_url_with_the_ledger_as_its_repository() {
        for (fork, ledger) in [
            (
                "https://forge.invalid/ours/a-repo",
                "https://forge.invalid/acme/ledger",
            ),
            (
                "https://forge.invalid/ours/a-repo.git",
                "https://forge.invalid/acme/ledger.git",
            ),
            (
                "https://forge.invalid/ours/a-repo.GIT/",
                "https://forge.invalid/acme/ledger.GIT",
            ),
            (
                "https://someone@forge.invalid:8443/ours/a-repo",
                "https://someone@forge.invalid:8443/acme/ledger",
            ),
            (
                "git@forge.invalid:ours/a-repo.git",
                "git@forge.invalid:acme/ledger.git",
            ),
            (
                "git@forge.invalid:/ours/a-repo",
                "git@forge.invalid:/acme/ledger",
            ),
            (
                "ssh://git@forge.invalid/ours/a-repo.git",
                "ssh://git@forge.invalid/acme/ledger.git",
            ),
            (
                "ssh://git@forge.invalid:2222/ours/a-repo",
                "ssh://git@forge.invalid:2222/acme/ledger",
            ),
            (
                "forge.invalid:ours/a-repo.git",
                "forge.invalid:acme/ledger.git",
            ),
        ] {
            assert_eq!(
                ledger_url(fork, "acme/ledger").as_deref(),
                Some(ledger),
                "{fork}"
            );
        }
        for not_a_forge in [
            "/srv/ours/a-repo",
            "file:///srv/ours/a-repo.git",
            "https://forge.invalid",
        ] {
            assert_eq!(
                ledger_url(not_a_forge, "acme/ledger"),
                None,
                "{not_a_forge}"
            );
        }
    }

    #[test]
    fn a_machine_is_named_for_its_short_host_name_and_its_machine_id() {
        // A readable machine id gives its first eight characters.
        assert_eq!(
            machine_name(
                "box-one.example.invalid",
                Some("0123abcd4567ef890123abcd4567ef89\n"),
                0
            ),
            "box-one-0123abcd"
        );
        // An id that is missing, short or not hex gives eight random digits.
        for id in [None, Some("0123abc"), Some("0123abcz4567ef89"), Some("")] {
            assert_eq!(
                machine_name("box-one", id, 0x1234_5678_9abc_def0),
                "box-one-9abcdef0",
                "{id:?}"
            );
        }
        assert_eq!(machine_name("box-one", None, 7), "box-one-00000007");
        // A host name character no ref component holds becomes `-`, and the
        // name never starts with one.
        for (host, name) in [
            (
                "box one:two~3^4?5*6[7\\8\u{7}9",
                "box-one-two-3-4-5-6-7-8-9-0123abcd",
            ),
            ("box@{1}", "box@-1}-0123abcd"),
            ("-box", "box-0123abcd"),
            (".hidden", "0123abcd"),
            ("", "0123abcd"),
        ] {
            let named = machine_name(host, Some("0123abcd"), 0);
            assert_eq!(named, name, "{host:?}");
            crate::ledger_git::ref_component("machine", &named).unwrap();
        }
    }

    #[test]
    fn this_machines_name_is_a_ref_component_ending_in_eight_hex_digits() {
        let named = this_machine();
        crate::ledger_git::ref_component("machine", &named).unwrap();
        let digits = named
            .rsplit_once('-')
            .map_or(named.as_str(), |(_, digits)| digits);
        assert_eq!(digits.len(), 8, "{named}");
        assert!(
            digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{named}"
        );
    }
}
