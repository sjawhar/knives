//! Joining a ledger the registry names and no git directory here reaches.
//!
//! A machine whose `repos.toml` shares a fork's ledger, and that nobody set
//! up to carry it, would otherwise have that fork's entries travel nowhere
//! and no claim of it held anywhere but here. So knives makes the git
//! directory itself, as a machine is set up by hand to share a ledger:
//! `<owner>--<name>.git` in [`ledger_repositories`], over the ledger root,
//! whose `origin` is the ledger's repository on the fork's own forge, and
//! which names this machine. A caller in a harness session names it for
//! that session on this host ([`this_machine`]): two sessions on one host,
//! each with its own config home, never share a ref, and the same session
//! joining again gets the same name back. What the remote already holds
//! under that name is this machine's own history, taken up before the
//! directory is in place ([`make`]), so a config home made again resumes
//! the claims it took rather than refuse its own ref. A directory already at
//! that path is never touched, and a dry run joins nothing.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::REMOTE;
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
/// its origin's else its upstream's, and it commits as [`this_machine`],
/// from what the remote already holds of that machine's ref ([`make`]).
/// Nothing is made where something already stands at its path, or when the
/// remote cannot be asked: the next command that routes `ledger` tries again.
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
///
/// Before the rename, the remote is asked for `refs/knives/<machine>`, and
/// what it holds there is taken up
/// ([`crate::ledger_git::take_up_own_ref`]). A name the remote already has
/// is this machine's own, from a git directory whose disk or config home is
/// gone: made without that history, this one would refuse its own ref at
/// the first write, and the claims taken under it could be released only
/// from another machine. A remote that cannot be asked, or a fetch that
/// fails, makes nothing, so the next command asks again rather than find a
/// git directory that refuses its own ref.
fn make(root: &Path, git_dir: &Path, url: &str, machine: &str) -> Result<bool, String> {
    let failed = |cause: String| format!("could not make {}: {cause}", git_dir.display());
    let repositories = ledger_repositories(root);
    let home = repositories.parent().unwrap_or(root);
    std::fs::create_dir_all(home)
        .map_err(|error| failed(format!("making {}: {error}", home.display())))?;
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
        &[os("remote"), os("add"), os(REMOTE), os(url)],
        &[os("config"), os("knives.machine"), os(machine)],
    ] {
        git(&[[os("--git-dir"), made.as_os_str()].as_slice(), args].concat()).map_err(&failed)?;
    }
    crate::ledger_git::Repository::new(&made, root, std::iter::empty())
        .and_then(|repository| crate::ledger_git::take_up_own_ref(&repository, REMOTE, machine))
        .map_err(|error| {
            failed(format!(
                "asking {url} for this machine's ref, refs/knives/{machine}: {error}"
            ))
        })?;
    std::fs::create_dir_all(&repositories)
        .map_err(|error| failed(format!("making {}: {error}", repositories.display())))?;
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

/// This machine's name on a ledger it joins, chosen once, when [`make`]
/// writes it to the git directory's `knives.machine`: for a caller in a
/// harness session ([`crate::commands::claim::harness_session`]),
/// [`session_machine_name`] of its host name, its `/etc/machine-id` and that
/// session; else [`machine_name`] of its host name, its `/etc/machine-id`
/// and a random number.
fn this_machine() -> String {
    let host = crate::liveness::host_name();
    let id = std::fs::read_to_string("/etc/machine-id").ok();
    crate::commands::claim::harness_session().map_or_else(
        || machine_name(&host, id.as_deref(), crate::lock::random_u64()),
        |(_, owner)| session_machine_name(&host, id.as_deref(), &owner),
    )
}

/// `<short host name>-<8 hex digits>`, as machines set up by hand to share
/// a ledger are named: the host name as [`named`] keeps it, and the first
/// eight characters of `machine_id` when they are hex digits, else eight
/// from `random`. The name of a machine that joins in no harness session.
fn machine_name(host: &str, machine_id: Option<&str>, random: u64) -> String {
    let digits = machine_id
        .and_then(|id| id.get(..8))
        .filter(|id| id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(|| format!("{:08x}", random & 0xffff_ffff), str::to_owned);
    named(host, &digits)
}

/// `<short host name>-<8 hex digits>` for a machine that joins in the
/// harness session `owner`: the host name as [`named`] keeps it, and the
/// first eight hex digits of the SHA-256 of `machine_id` trimmed (empty when
/// there is none), a newline, and `owner`.
///
/// A host runs many sessions, and each that joins with a config home of its
/// own commits to a ref of its own: under one name, the second to push would
/// be refused as another writer of the first one's ref. The same session on
/// the same host gets the same name back, so a config home it makes again
/// takes up the ref it committed to ([`make`]); the machine id keeps one
/// session's name on two hosts of one host name apart.
fn session_machine_name(host: &str, machine_id: Option<&str>, owner: &str) -> String {
    use sha2::Digest as _;
    let [a, b, c, d, ..]: [u8; 32] = sha2::Sha256::new()
        .chain_update(machine_id.unwrap_or_default().trim())
        .chain_update("\n")
        .chain_update(owner)
        .finalize()
        .into();
    named(host, &format!("{:08x}", u32::from_be_bytes([a, b, c, d])))
}

/// `<short host name>-<digits>`: the host name up to its first `.`, and
/// `digits`.
///
/// The name spells this machine's ref, `refs/knives/<machine>`, as one ref
/// component ([`crate::ledger_git::ref_component`]). So each character of
/// the host name no component can hold (a control character, a space, any
/// of `~^:?*[\/`) becomes `-`, as does the `{` of `@{`, and the name never
/// starts with `-`: a host name that would leave nothing gives the digits
/// alone.
fn named(host: &str, digits: &str) -> String {
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
    if short.is_empty() {
        digits.to_owned()
    } else {
        format!("{short}-{digits}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ids::CommitId;
    use crate::ledger_sweep::{destinations, lab, unpullable};

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

    /// The ledger's remote: a bare repository git reaches at `url` through
    /// the URL rule a session hands it (`GIT_CONFIG_COUNT` and its numbered
    /// pair), with the caller in the harness session `owner-one`; both are
    /// this process's environment until it drops.
    struct Remote {
        git_dir: PathBuf,
        _dir: tempfile::TempDir,
        _environment: crate::config::test_support::EnvironmentGuard,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Remote {
        fn reached_at(url: &str) -> Self {
            let lock = crate::config::test_support::environment_lock();
            let environment = crate::config::test_support::no_harness_session(&[
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
            ]);
            environment.set("KNIVES_OWNER", "owner-one");
            let dir = tempfile::tempdir().unwrap();
            let git_dir = dir.path().join("ledger.git");
            lab::init(&git_dir, true);
            environment.set("GIT_CONFIG_COUNT", "1");
            environment.set(
                "GIT_CONFIG_KEY_0",
                &format!("url.{}.insteadOf", git_dir.display()),
            );
            environment.set("GIT_CONFIG_VALUE_0", url);
            Self {
                git_dir,
                _dir: dir,
                _environment: environment,
                _lock: lock,
            }
        }

        /// Point `reference` here at a new commit of an empty tree; that commit.
        fn commit_at(&self, reference: &str) -> CommitId {
            let tree = lab::git(&self.git_dir, &["mktree"]);
            let commit = lab::git(
                &self.git_dir,
                &[
                    "commit-tree",
                    &tree,
                    "-m",
                    "committed by an earlier config home",
                ],
            );
            lab::git(&self.git_dir, &["update-ref", reference, &commit]);
            CommitId::new(commit)
        }
    }

    #[test]
    fn a_ledger_no_git_directory_reaches_is_joined_once_and_then_carries_its_forks() {
        // Given: a new machine whose registry shares two forks' ledger with
        // acme/ledger, and one fork's not at all, and that ledger's remote,
        // which holds nothing yet.
        let _remote = Remote::reached_at("https://forge.invalid/acme/ledger.git");
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
        // Given: a fork whose origin is a directory on this machine, and the
        // ledger's remote on the upstream's forge.
        let _remote = Remote::reached_at("https://forge.invalid/acme/ledger");
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
    fn a_join_takes_up_the_ref_the_remote_holds_under_this_machines_name() {
        // Given: a new machine whose registry shares a fork's ledger, and a
        // remote that already holds a commit under the name this machine
        // joins as, as a config home that wrote there and then was deleted
        // leaves it.
        let remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let (_home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let machine = this_machine();
        let own = format!("refs/knives/{machine}");
        let commit = remote.commit_at(&own);

        // When: where the ledger travels is read.
        let found = destinations(&root, &registry).unwrap();

        // Then: the git directory it made names that machine, and holds the
        // remote's commit as this machine's own ref and as the remote's copy
        // of it.
        let git_dir = joined_git_dir(&root, "acme/ledger");
        assert_eq!(config(&git_dir, "knives.machine"), Some(machine.clone()));
        for reference in [&own, &format!("refs/knives-remotes/origin/{machine}")] {
            assert_eq!(
                lab::at(&git_dir, reference).as_ref(),
                Some(&commit),
                "{reference}"
            );
        }
        // And: a write as that machine is not refused as another's.
        let [destination] = found.found.as_slice() else {
            panic!("{found:?}");
        };
        assert!(!crate::ledger_git::has_new_entries(&destination.repository, &machine).unwrap());
        // And: the join sent nothing: the remote's ref is where it was.
        assert_eq!(lab::at(&remote.git_dir, &own), Some(commit));
    }

    #[test]
    fn a_ledger_whose_remote_cannot_be_asked_is_not_joined_and_the_next_read_tries_again() {
        // Given: a new machine whose registry shares a fork's ledger on a
        // forge whose host does not resolve.
        let (home, root, registry) = new_machine(&[(
            "a-repo",
            "https://forge.invalid/ours/a-repo",
            Some("acme/ledger"),
        )]);
        let fork = UpstreamName::new("acme/a-repo");
        {
            // The lock keeps the remote unreachable: another test's `Remote`
            // reaches the same URL while it holds it. It is released before
            // this test's own `Remote` takes it.
            let _lock = crate::config::test_support::environment_lock();

            // When: where the ledger travels is read.
            let mut said = Vec::new();
            let found =
                super::super::destinations::destinations_telling(&root, &registry, &mut said)
                    .unwrap();

            // Then: nothing is made or said, and the fork is unreached, with
            // why: the remote could not be asked for this machine's ref.
            assert_eq!(
                listed(home.path()),
                BTreeSet::from(["repos.toml".to_owned()])
            );
            assert!(said.is_empty(), "{}", String::from_utf8_lossy(&said));
            let why = found
                .unreached
                .get(&fork)
                .and_then(|unreached| unreached.join_failed.as_deref())
                .unwrap();
            assert!(
                why.starts_with(&format!(
                    "could not make {}: asking https://forge.invalid/acme/ledger for this \
                     machine's ref, refs/knives/",
                    std::path::absolute(joined_git_dir(&root, "acme/ledger"))
                        .unwrap()
                        .display()
                )),
                "{why}"
            );
        }

        // When: the remote can be reached, and it is read again.
        let _remote = Remote::reached_at("https://forge.invalid/acme/ledger");
        let again = destinations(&root, &registry).unwrap();

        // Then: the ledger is joined.
        assert!(again.unreached.is_empty(), "{:?}", again.unreached);
        assert_eq!(again.found.len(), 1);
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
    fn a_machine_joined_in_a_session_is_named_for_its_host_machine_id_and_session() {
        // One host, machine id and session give one name: the first eight
        // hex digits of the SHA-256 of the trimmed id, a newline and the
        // session.
        let id = Some("0123abcd4567ef890123abcd4567ef89\n");
        let host = "box-one.example.invalid";
        assert_eq!(
            session_machine_name(host, id, "owner-one"),
            "box-one-e6fa3479"
        );
        // Another session on the same host gets another name.
        assert_eq!(
            session_machine_name(host, id, "owner-two"),
            "box-one-46e08708"
        );
        // A missing machine id, or one that is empty once trimmed, still
        // gives one name per session.
        for missing in [None, Some(""), Some(" \n")] {
            assert_eq!(
                session_machine_name(host, missing, "owner-one"),
                "box-one-04164234",
                "{missing:?}"
            );
            assert_eq!(
                session_machine_name(host, missing, "owner-two"),
                "box-one-627a5184",
                "{missing:?}"
            );
        }
        // The host name is kept as outside a session: each character no ref
        // component holds becomes `-`, and the name never starts with one.
        for (host, name) in [
            (
                "box one:two~3^4?5*6[7\\8\u{7}9",
                "box-one-two-3-4-5-6-7-8-9-38c73318",
            ),
            ("box@{1}", "box@-1}-38c73318"),
            ("-box", "box-38c73318"),
            (".hidden", "38c73318"),
            ("", "38c73318"),
        ] {
            let named = session_machine_name(host, Some("0123abcd"), "owner-one");
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
