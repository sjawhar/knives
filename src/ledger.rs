// allow: SIZE_OK: 2029 lines - entry type, storage, filters, and writer are one domain.
//! What agents did and decided here, in order, forever.
//!
//! [`crate::store`] holds current intent and is rewritten whole on every change:
//! `knives finish` deletes the claim that said why a branch exists, and nothing
//! remembers it afterwards. Agents then rediscover a mysterious branch by
//! archaeology, or draw a conclusion from a stale one.
//!
//! One directory of immutable markdown files per repository, beside
//! `state.json` — each entry its own file, TOML frontmatter between `+++`
//! fences, the text as the body. An entry is an event (this tool observed one
//! of its own commands) or a note (an agent asserted something), anchored to
//! the subject's tip at write time. That anchor is why the record does not
//! rot: a reader who sees the tip has moved since knows to re-verify rather
//! than inherit the conclusion. Nothing derived is stored — a recorded
//! past-tense judgment stays true, while a cached disposition goes wrong the
//! moment upstream moves.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::default_config_path;
use crate::ids::UpstreamName;
use crate::statement::Statement;

/// The directory holding every repository's ledger, beside `state.json`.
pub fn default_ledger_root() -> PathBuf {
    default_config_path().with_file_name("ledger")
}

/// Where a fork's ledger lives: `<owner>/<name>/` of the repository it is a
/// fork of ([`crate::config::RepoEntry::upstream_name`]), a directory of entry
/// files beside `state.json`.
///
/// The name is lowercase, owner and repository both, and must stay so. GitHub
/// treats an owner and a repository name case-insensitively, and so does
/// knives everywhere it compares remotes ([`crate::remote_url::same_remote`]
/// uses `eq_ignore_ascii_case`): two machines whose `repos.toml` spells
/// `acme/tool` and `Acme/Tool` check out one repository. Spelled as written,
/// they would file that repository's entries in two directories on a
/// case-sensitive filesystem, each machine reading only its own half, and the
/// ledger would diverge without a word. That divergence is what naming a
/// ledger after its upstream repository exists to end; the capitals are not
/// to come back for tidiness.
///
/// Each entry is immutable, so concurrent writers never share a file and a git
/// history over the directory is pure additions.
pub fn default_ledger_path(repo: &UpstreamName) -> PathBuf {
    default_ledger_root().join(repo.as_str())
}

/// Whether `part` can be one directory name on the way to a fork's ledger.
///
/// Not empty, `.` or `..`, holding no `/` or `\`, and not a `.git` in any
/// letter case, which is where the git directory over the root lives.
pub fn is_ledger_component(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.contains(['/', '\\'])
        && !part.eq_ignore_ascii_case(".git")
}

/// Whether `name` is `<owner>/<name>`, each an [`is_ledger_component`].
///
/// That is the directory a fork whose upstream is a forge repository is
/// kept under, and the one shape a ledger repository carries. The registry
/// refuses an upstream whose name is not one, and so does
/// [`crate::ledger_git::Repository::new`].
pub fn is_fork_directory(name: &str) -> bool {
    name.split_once('/').is_some_and(|(owner, repository)| {
        is_ledger_component(owner) && is_ledger_component(repository)
    })
}

/// Set once this process has appended an entry; see [`appended`].
static APPENDED: AtomicBool = AtomicBool::new(false);

/// Whether this process has appended an entry to any ledger, so that it has
/// something to hand to [`crate::ledger_sweep::hand_off`] before it exits.
pub fn appended() -> bool {
    APPENDED.load(Ordering::Relaxed)
}

/// Who put an entry there.
///
/// Two values, not three. The question a reader asks is whether a machine
/// observed this or an agent asserted it; a supersession or a parking arrives as
/// an event through `finish --superseded-by` and `start --why`, and everything an
/// agent asserts is a note. Asking a writing agent to grade its own entry as
/// judgment-versus-note is a decision with no read-time payoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Event,
    Note,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Event => "event",
            Self::Note => "note",
        })
    }
}

/// Which entry class a read wants; a disposition remains a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryClass {
    Note,
    Event,
    Disposition,
}

/// One entry of the ledger.
///
/// Unknown frontmatter keys are ignored rather than rejected: entries are never
/// rewritten, so a newer binary may add a field and an older one must still
/// read the file. That is the whole schema-evolution story, and it is why there
/// is no version number.
///
/// The serde derives here are the `--json` report surface — their
/// skip-if-absent attributes are why an absent anchor is absent in JSON too.
/// The file surface is `Frontmatter`, which is this struct minus `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// When it was written, RFC 3339 UTC.
    pub ts: String,
    /// Resolved exactly as a claim's owner is.
    pub owner: String,
    /// The writer's `git config user.email` where it wrote, so a reader can
    /// tell whose machine an `owner` was: absent when none was configured,
    /// and on every entry written before knives recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The ref this is about — a branch or a release name. Absent for an entry
    /// about the repository itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub kind: Kind,
    /// A terminal ruling about the subject — `merged-elsewhere`, `withdrawn`,
    /// `ruled-out` — with provenance in [`Entry::evidence`]. It stays a note:
    /// unknown frontmatter keys are the ledger's compatible extension point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// A statement about the subject that knives cannot infer — its pull
    /// request, that it has none upstream, what superseded it, what it needs
    /// first. Only this field states anything: an event's prose never does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<Statement>,
    pub text: String,
    /// Free strings backing the entry: commit ids, `file:line`, `<repo>#<number>`,
    /// URLs, and they may name other repositories. Every audit claim that
    /// survived red-teaming cited one; every false finding lacked one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// The subject's tip when this was written, absent when it did not resolve.
    ///
    /// Never caller-supplied. A branch deleted since leaves the entry valid with
    /// no anchor; a tip that has moved tells the reader to re-verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// The pull request stated for the subject, from its live `pull` statement
    /// only. Never a forge call: this is a write path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The parent set a release cut or edit left behind, one item per parent
    /// with every local bookmark holding it at the time. Written by `cut`,
    /// `include`, `drop`, `advance` and `rebase`; the record a later edit uses
    /// to tell which parent is which branch once a rebase done outside jj has
    /// left no ancestry or change id to say so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<RecordedParent>,
}

/// One release parent as an event recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedParent {
    pub commit: String,
    /// Every local bookmark at the commit when the event was written; empty for
    /// a parent nothing named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
}

/// The machine surface of an entry file: every 1.1 field except the text,
/// which is the markdown body rather than a TOML value, so prose reads and
/// writes as prose.
///
/// Kept separate from [`Entry`] deliberately: `Entry`'s serde is the `--json`
/// report surface and must keep `text`; this is the file surface and must not.
#[derive(Debug, Serialize, Deserialize)]
struct Frontmatter {
    ts: String,
    owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    statement: Option<Statement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pr: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parents: Vec<RecordedParent>,
}

impl Frontmatter {
    fn of(entry: &Entry) -> Self {
        Self {
            ts: entry.ts.clone(),
            owner: entry.owner.clone(),
            email: entry.email.clone(),
            subject: entry.subject.clone(),
            kind: entry.kind,
            disposition: entry.disposition.clone(),
            statement: entry.statement.clone(),
            evidence: entry.evidence.clone(),
            anchor: entry.anchor.clone(),
            pr: entry.pr,
            parents: entry.parents.clone(),
        }
    }

    /// The frontmatter as TOML, with any statement as one inline table.
    ///
    /// toml writes every nested struct as a `[section]`, never inline, so the
    /// statement is rendered on its own and put first: the first line of a
    /// document always belongs to its root table, whatever sections follow.
    fn into_toml(mut self) -> Result<String, toml::ser::Error> {
        let mut text = String::new();
        if let Some(statement) = self.statement.take() {
            text.push_str("statement = ");
            statement.serialize(toml::ser::ValueSerializer::new(&mut text))?;
            text.push('\n');
        }
        text.push_str(&toml::to_string(&self)?);
        Ok(text)
    }

    fn into_entry(self, text: String) -> Entry {
        Entry {
            ts: self.ts,
            owner: self.owner,
            email: self.email,
            subject: self.subject,
            kind: self.kind,
            disposition: self.disposition,
            statement: self.statement,
            text,
            evidence: self.evidence,
            anchor: self.anchor,
            pr: self.pr,
            parents: self.parents,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
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
    #[error("{path} is not a ledger entry: {detail}")]
    Parse { path: PathBuf, detail: String },
    #[error("{path} has an unreadable timestamp `{ts}`")]
    Timestamp { path: PathBuf, ts: String },
    #[error("{path} already exists: two writers drew the same nanosecond and suffix")]
    Collision { path: PathBuf },
    #[error(
        "{} still holds the entries an older knives filed under the fork's registry key; this \
         knives reads the fork's ledger at {}, so it would read it as holding none of them. \
         Run `knives ledger migrate` once to move them",
        former.display(),
        path.display()
    )]
    FormerName { path: PathBuf, former: PathBuf },
    #[error("serialising a ledger entry: {source}")]
    Serialise {
        #[from]
        source: toml::ser::Error,
    },
}

/// Whether `directory` holds a ledger entry file of its own.
///
/// That is a `*.md` regular file directly inside it, not inside a directory
/// below it: a registry key can be the owner directory of other forks'
/// upstream names (`acme` beside `acme/demo`). Unreadable is taken as
/// holding one, so a directory nobody could read is refused rather than
/// passed as migrated.
pub fn holds_entries(directory: &Path) -> bool {
    let Ok(listing) = std::fs::read_dir(directory) else {
        return directory.exists();
    };
    listing.into_iter().any(|dirent| {
        dirent.is_err()
            || dirent.is_ok_and(|dirent| {
                dirent.file_type().is_ok_and(|kind| kind.is_file())
                    && Path::new(&dirent.file_name()).extension() == Some(OsStr::new("md"))
            })
    })
}

/// A problem line for each directory under `root` holding entries that no
/// fork in `registry` is kept under.
///
/// A fork is kept under `<owner>/<name>` of its upstream, and under its
/// registry key where an older knives kept it and a filesystem upstream
/// keeps it still; [`holds_entries`] decides what holds entries.
///
/// Nothing reads such a directory. It is what a fork leaves behind when its
/// `upstream` changes, or when two machines' registries spell one upstream
/// as two repositories: the fork then reads its ledger, under the new name,
/// as empty, and only this says where its entries went. A directory that
/// cannot be listed is a problem too, rather than a ledger with none.
pub fn unclaimed_problems(root: &Path, registry: &crate::config::Registry) -> Vec<String> {
    let claimed: BTreeSet<String> = registry
        .repos
        .iter()
        .flat_map(|(key, entry)| [key.clone(), entry.upstream_name(key).to_string()])
        .collect();
    let subdirectories = |directory: &Path| match std::fs::read_dir(directory) {
        Ok(listing) => Ok(listing
            .filter_map(|dirent| {
                let dirent = dirent.ok()?;
                let name = dirent.file_name().to_string_lossy().into_owned();
                (dirent.file_type().is_ok_and(|kind| kind.is_dir())
                    && !name.eq_ignore_ascii_case(".git"))
                .then_some(name)
            })
            .collect::<Vec<_>>()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(format!(
            "could not list {} for ledger entries no fork is kept under: {source}",
            directory.display()
        )),
    };
    let unclaimed = |name: String| {
        let directory = root.join(&name);
        (!claimed.contains(&name) && holds_entries(&directory)).then(|| {
            format!(
                "{} holds ledger entries that no fork in repos.toml is kept under, so nothing \
                 reads them, and the fork they belong to reads its ledger without them. Either \
                 correct that fork's `upstream` in repos.toml to the repository they were filed \
                 under, or move them into the directory of the fork they belong to, \
                 {}/<owner>/<name>",
                directory.display(),
                root.display()
            )
        })
    };
    let owners = match subdirectories(root) {
        Ok(owners) => owners,
        Err(problem) => return vec![problem],
    };
    let mut problems = Vec::new();
    for owner in owners {
        match subdirectories(&root.join(&owner)) {
            Ok(names) => problems.extend(
                names
                    .into_iter()
                    .filter_map(|name| unclaimed(format!("{owner}/{name}"))),
            ),
            Err(problem) => problems.push(problem),
        }
        problems.extend(unclaimed(owner));
    }
    problems
}

/// One repository's ledger directory.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: PathBuf,
    /// Where an older knives filed this fork's entries, under its registry
    /// key, when that is not where they are now.
    former: Option<PathBuf>,
}

impl Ledger {
    /// A fork's ledger at the default location: the one place a command
    /// acting on a fork resolves it.
    ///
    /// An older knives filed a fork's entries under its registry key, and
    /// this one reads them under the fork's upstream name. Every read of a
    /// ledger resolved here refuses while the registry key's directory still
    /// holds entries ([`LedgerError::FormerName`]): read from the new
    /// directory alone, it would answer "no notches yet", or find no
    /// recorded cut for a release's drop guard to check against.
    pub fn for_fork(fork: &crate::bind::Fork<'_>) -> Self {
        let former = (fork.name.as_str() != fork.upstream.as_str())
            .then(|| default_ledger_root().join(fork.name.as_str()));
        Self {
            path: default_ledger_path(&fork.upstream),
            former,
        }
    }

    /// At an exact path, for a test or for a caller with its own config home.
    pub const fn at(path: PathBuf) -> Self {
        Self { path, former: None }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Refuse while the fork's registry-key directory still holds entries;
    /// see [`Ledger::for_fork`].
    fn migrated(&self) -> Result<(), LedgerError> {
        match &self.former {
            Some(former) if holds_entries(former) => Err(LedgerError::FormerName {
                path: self.path.clone(),
                former: former.clone(),
            }),
            _ => Ok(()),
        }
    }

    /// Write one entry as one new immutable file, returning its path.
    ///
    /// The entry is written to a temporary file before `persist_noclobber`
    /// atomically makes its final name visible. Two agents appending at the
    /// same moment write two different files, so there is nothing to interleave
    /// and no lock to hold. A filename collision — same nanosecond, same random
    /// suffix — errors loudly instead of retrying, because at that resolution a
    /// retry would paper over a broken clock or random source.
    pub fn append(&self, entry: &Entry) -> Result<PathBuf, LedgerError> {
        self.migrated()?;
        let ts: jiff::Timestamp = entry.ts.parse().map_err(|_| LedgerError::Timestamp {
            path: self.path.clone(),
            ts: entry.ts.clone(),
        })?;
        // What every read would refuse is refused before it is written, and
        // so is a kind only a reader has: written, it would state nothing.
        if let Some(statement) = &entry.statement {
            let refused = if statement.kind == crate::statement::StatementKind::Unknown {
                Err("a statement of a kind this knives does not know states nothing".to_owned())
            } else {
                statement.check()
            };
            refused.map_err(|detail| LedgerError::Parse {
                path: self.path.clone(),
                detail,
            })?;
        }
        let contents = format!(
            "+++\n{}+++\n{}\n",
            Frontmatter::of(entry).into_toml()?,
            entry.text
        );
        std::fs::create_dir_all(&self.path).map_err(|source| LedgerError::Write {
            path: self.path.clone(),
            source,
        })?;
        let path = self.path.join(entry_file_name(ts));
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.path).map_err(|source| LedgerError::Write {
                path: self.path.clone(),
                source,
            })?;
        temporary
            .write_all(contents.as_bytes())
            .map_err(|source| LedgerError::Write {
                path: path.clone(),
                source,
            })?;
        persist_entry(temporary, path.clone())?;
        APPENDED.store(true, Ordering::Relaxed);
        Ok(path)
    }

    /// Every entry, oldest first: lexicographic filename order, which the
    /// fixed-width stamp makes chronological order.
    ///
    /// A ledger directory that does not exist yet is empty rather than an
    /// error: a repository nobody has notched is the normal case. An entry
    /// file that does not parse IS an error, because a ledger the tool cannot
    /// read must not read as a ledger with nothing in it. Only `*.md` files
    /// are entries: an editor's or a sync tool's droppings beside them are
    /// ignored, not fatal. Each read parses the whole directory — O(all
    /// history) — which is acceptable at observed scales; chronological
    /// filenames leave a tail-read optimization open if `KNIVES_TIMING` shows
    /// it is needed.
    pub fn entries(&self) -> Result<Vec<Entry>, LedgerError> {
        Ok(self
            .entry_files()?
            .into_iter()
            .map(|(_, entry)| entry)
            .collect())
    }

    /// [`Ledger::entries`], each beside the file it was read from.
    pub fn entry_files(&self) -> Result<Vec<(PathBuf, Entry)>, LedgerError> {
        self.entry_reads()?
            .into_iter()
            .map(|(path, read)| read.map(|entry| (path, entry)))
            .collect()
    }

    /// Each entry file in name order, beside what reading it gave: for a
    /// caller that has a use for the entries that do parse when one does
    /// not. Only listing the directory fails the call.
    pub fn entry_reads(&self) -> Result<Vec<EntryRead>, LedgerError> {
        self.migrated()?;
        let listing = match std::fs::read_dir(&self.path) {
            Ok(listing) => listing,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(source) => {
                return Err(LedgerError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let mut files = Vec::new();
        for dirent in listing {
            let path = dirent
                .map_err(|source| LedgerError::Read {
                    path: self.path.clone(),
                    source,
                })?
                .path();
            if path.extension() == Some(OsStr::new("md")) {
                files.push(path);
            }
        }
        files.sort();
        Ok(files
            .into_iter()
            .map(|path| {
                let read = parse_file(&path);
                (path, read)
            })
            .collect())
    }
}

/// One entry file, beside what reading it gave ([`Ledger::entry_reads`]).
pub type EntryRead = (PathBuf, Result<Entry, LedgerError>);

/// Which entries a read wants.
#[derive(Debug, Default, Clone, Copy)]
pub struct Filter<'a> {
    /// Only entries about this ref.
    pub subject: Option<&'a str>,
    /// Only entries stamped with this pull request.
    pub pr: Option<u64>,
    /// Limit reads to one entry class. A disposition is also a note.
    pub only: Option<EntryClass>,
    /// Keep at most this many, the newest of them. `None` keeps everything.
    pub limit: Option<usize>,
}

/// Entries matching `filter`, oldest first, and how many matched before the limit.
///
/// The count travels with the result so a truncated read can say so: a window
/// that silently drops the older half of a branch's history is how a reader
/// concludes the history is short.
pub fn select<'a>(entries: &'a [Entry], filter: &Filter<'_>) -> (Vec<&'a Entry>, usize) {
    let pr_subject = filter.pr.map(|number| format!("#{number}"));
    let matched: Vec<&Entry> = entries
        .iter()
        .filter(|entry| {
            filter
                .subject
                .is_none_or(|wanted| entry.subject.as_deref() == Some(wanted))
        })
        .filter(|entry| {
            filter.pr.is_none_or(|wanted| {
                entry.pr == Some(wanted) || entry.subject.as_deref() == pr_subject.as_deref()
            })
        })
        .filter(|entry| {
            filter.only.is_none_or(|class| match class {
                EntryClass::Event => entry.kind == Kind::Event,
                EntryClass::Note => entry.kind == Kind::Note,
                EntryClass::Disposition => entry.disposition.is_some(),
            })
        })
        .collect();
    let matched_count = matched.len();
    let skipped = filter
        .limit
        .map_or(0, |limit| matched_count.saturating_sub(limit));
    (matched.into_iter().skip(skipped).collect(), matched_count)
}

/// The newest entry about `subject`: the last match in the order `entries`
/// returns, which is stamp order on disk.
///
/// The stamp is the authority now that every entry is its own file — there is
/// no shared file whose append order could disagree with it, and two writers
/// inside the same nanosecond have no meaningful "newer" to preserve.
pub fn newest_for<'a>(entries: &'a [Entry], subject: &str) -> Option<&'a Entry> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.subject.as_deref() == Some(subject))
}

/// One re-verification failure: content the entry cites that the repository no
/// longer shows, or an anchor whose subject moved.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct VerifyFlag {
    pub ts: String,
    pub subject: Option<String>,
    pub what: String,
}

/// Re-check commit-shaped evidence and anchors against a visibility-scoped
/// commit listing. A short evidence SHA matches a visible full id by prefix.
pub fn verify_entries(
    entries: &[Entry],
    visible: &BTreeSet<String>,
    tips: &BTreeMap<String, String>,
) -> Vec<VerifyFlag> {
    let mut flags = Vec::new();
    for entry in entries {
        for evidence in &entry.evidence {
            if is_commit_token(evidence) && !visible_contains(visible, evidence) {
                flags.push(VerifyFlag {
                    ts: entry.ts.clone(),
                    subject: entry.subject.clone(),
                    what: format!("evidence {evidence} not found in this repository"),
                });
            }
        }
        if let Some(anchor) = &entry.anchor {
            if !visible_contains(visible, anchor) {
                flags.push(VerifyFlag {
                    ts: entry.ts.clone(),
                    subject: entry.subject.clone(),
                    what: format!("anchor {anchor} vanished"),
                });
            } else if let Some(tip) = entry
                .subject
                .as_deref()
                .and_then(|subject| tips.get(subject))
                && !tip.starts_with(anchor)
            {
                flags.push(VerifyFlag {
                    ts: entry.ts.clone(),
                    subject: entry.subject.clone(),
                    what: format!("anchor moved: {anchor} -> {tip}"),
                });
            }
        }
    }
    flags
}

fn is_commit_token(token: &str) -> bool {
    (12..=40).contains(&token.len())
        && token
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
}

fn visible_contains(visible: &BTreeSet<String>, token: &str) -> bool {
    visible
        .range(token.to_owned()..)
        .next()
        .is_some_and(|id| id.starts_with(token))
}
pub fn age(ts: &str, now: jiff::Timestamp) -> Option<String> {
    let then = ts.parse::<jiff::Timestamp>().ok()?;
    let seconds = now.as_second().saturating_sub(then.as_second()).max(0);
    Some(if seconds < 60 {
        "now".to_owned()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    })
}

/// Render ledger text for an inline human terminal field.
///
/// C0 controls and DEL become U+FFFD so compact fields cannot emit terminal
/// control sequences while JSON remains the raw structured representation.
pub fn inline_human_text(text: &str) -> String {
    escape_human_text(text, false)
}

/// Render ledger body prose for a human terminal.
///
/// Line feeds remain available for the caller to indent; every other C0
/// control and DEL becomes U+FFFD.
pub fn body_human_text(text: &str) -> String {
    escape_human_text(text, true)
}

fn escape_human_text(text: &str, preserve_line_feeds: bool) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_control() && !(preserve_line_feeds && character == '\n') {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

/// An entry before its automatic fields are stamped.
#[derive(Debug)]
pub struct Draft<'a> {
    /// The ref this is about, or nothing for an entry about the repository.
    pub subject: Option<&'a str>,
    pub kind: Kind,
    /// A terminal ruling token when this note is a disposition.
    pub disposition: Option<String>,
    /// What the entry states about its subject, beside its prose.
    pub statement: Option<Statement>,
    pub text: String,
    pub evidence: Vec<String>,
    /// The pull request stated for the subject, read from the store by the
    /// caller. Never a forge call: a round trip here would make every claim,
    /// track and sync pay for a network hop to record what it just did.
    pub pr: Option<u64>,
    /// The parent set a release cut or edit leaves behind; empty for every
    /// other entry.
    pub parents: Vec<RecordedParent>,
}

impl<'a> Draft<'a> {
    /// An event: something this tool did, recorded as part of doing it. It
    /// states nothing until the caller sets [`Draft::statement`].
    pub const fn event(subject: Option<&'a str>, text: String, pr: Option<u64>) -> Self {
        Self {
            subject,
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text,
            evidence: Vec::new(),
            pr,
            parents: Vec::new(),
        }
    }
}

/// Where automatic events go, and who is writing them.
///
/// Bound once per command rather than threaded as four arguments: every event a
/// single run records has the same repository, checkout, owner and ledger.
#[derive(Debug)]
pub struct Scribe {
    ledger: Ledger,
    repo: UpstreamName,
    /// The checkout whose refs anchor entries; none for a writer acting on the
    /// ledger alone.
    checkout: Option<PathBuf>,
    owner: String,
    /// The writer's `git config user.email`, asked once, at the first entry:
    /// a command that writes nothing never asks.
    email: std::sync::OnceLock<Option<String>>,
}

impl Scribe {
    pub const fn new(ledger: Ledger, repo: UpstreamName, path: PathBuf, owner: String) -> Self {
        Self {
            ledger,
            repo,
            checkout: Some(path),
            owner,
            email: std::sync::OnceLock::new(),
        }
    }

    /// A writer with no checkout of the fork to ask, whose entries carry no
    /// anchor.
    pub const fn unanchored(ledger: Ledger, repo: UpstreamName, owner: String) -> Self {
        Self {
            ledger,
            repo,
            checkout: None,
            owner,
            email: std::sync::OnceLock::new(),
        }
    }

    pub const fn repo(&self) -> &UpstreamName {
        &self.repo
    }

    pub const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Append `draft`, stamping the fields no caller supplies.
    pub fn record(&self, draft: &Draft<'_>) -> Result<Entry, LedgerError> {
        self.record_to(draft).map(|(entry, _)| entry)
    }

    /// [`Scribe::record`], with the path of the file the entry was written to.
    pub fn record_to(&self, draft: &Draft<'_>) -> Result<(Entry, PathBuf), LedgerError> {
        let entry = Entry {
            ts: monotonic_now().to_string(),
            owner: self.owner.clone(),
            email: self
                .email
                .get_or_init(|| git_user_email(self.checkout.as_deref()))
                .clone(),
            subject: draft.subject.map(str::to_owned),
            kind: draft.kind,
            disposition: draft.disposition.clone(),
            statement: draft.statement.clone(),
            text: draft.text.clone(),
            evidence: draft.evidence.clone(),
            anchor: self.anchor(draft.subject),
            pr: draft.pr,
            parents: draft.parents.clone(),
        };
        let path = self.ledger.append(&entry)?;
        Ok((entry, path))
    }

    /// Record that this tool did something, as part of doing it.
    pub fn event(
        &self,
        subject: Option<&str>,
        text: String,
        pr: Option<u64>,
    ) -> Result<Entry, LedgerError> {
        self.record(&Draft::event(subject, text, pr))
    }

    /// The subject's tip now, or nothing when it does not resolve.
    ///
    /// One local repository open per append. A branch deleted since, a reaped
    /// release ref, and a checkout that is not a repository all land here, and
    /// none of them is a reason to lose the entry.
    fn anchor(&self, subject: Option<&str>) -> Option<String> {
        let subject = subject?;
        crate::jj::Repo::open(self.checkout.as_ref()?)
            .ok()?
            .resolve_commit(subject)
            .ok()
            .map(|commit| commit.as_str().to_owned())
    }
}

/// `git config user.email` where the writer writes: in the fork's checkout,
/// where a repository's own identity overrides the user's, or where the
/// command runs when there is no checkout. `None` when git reports none, or
/// an empty one: an entry carries the address git would commit as, or no
/// address, never a guess.
fn git_user_email(checkout: Option<&Path>) -> Option<String> {
    let mut command = checkout.map_or_else(crate::bind::git_command, crate::bind::git);
    let output = command
        .args(["config", "--get", "user.email"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let email = String::from_utf8(output.stdout).ok()?;
    let email = email.trim();
    (!email.is_empty()).then(|| email.to_owned())
}

/// Make a completed temporary entry visible without replacing an existing one.
fn persist_entry(temporary: tempfile::NamedTempFile, path: PathBuf) -> Result<(), LedgerError> {
    temporary
        .persist_noclobber(&path)
        .map(|_| ())
        .map_err(|error| {
            let source = error.error;
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                LedgerError::Collision { path }
            } else {
                LedgerError::Write { path, source }
            }
        })
}

/// `20260815T221403.123456789Z-4f2a.md`: the entry's timestamp compacted to a
/// filename at nanosecond precision, then four random hex characters. Every
/// column is fixed width, so lexicographic order over the directory is
/// chronological order — the property `entries` sorts by.
///
/// The suffix exists for two writers inside the same nanosecond. It comes from
/// `RandomState` hashing the stamp and a process-wide counter — the idiom
/// `src/hook/guidance.rs` already uses for its nonce — because the crate
/// carries no `rand` dependency and sixteen bits do not justify one.
fn entry_file_name(ts: jiff::Timestamp) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    ts.as_nanosecond().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    let suffix = hasher.finish() & 0xffff;
    format!(
        "{}.{:09}Z-{suffix:04x}.md",
        ts.strftime("%Y%m%dT%H%M%S"),
        ts.subsec_nanosecond()
    )
}

/// One entry file: TOML frontmatter between `+++` fences, then the text.
fn parse_file(path: &Path) -> Result<Entry, LedgerError> {
    let text = std::fs::read_to_string(path).map_err(|source| LedgerError::Read {
        path: path.to_owned(),
        source,
    })?;
    let Some(rest) = text.strip_prefix("+++\n") else {
        return Err(LedgerError::Parse {
            path: path.to_owned(),
            detail: "missing the opening +++ fence".to_owned(),
        });
    };
    let (frontmatter, body) = split_fenced(rest).map_err(|detail| LedgerError::Parse {
        path: path.to_owned(),
        detail,
    })?;
    // Checked here rather than at every reader: a timestamp nothing can order
    // is a corrupt record, and one loud error beats a breadcrumb with no age.
    if frontmatter.ts.parse::<jiff::Timestamp>().is_err() {
        return Err(LedgerError::Timestamp {
            path: path.to_owned(),
            ts: frontmatter.ts,
        });
    }
    // So is a statement whose value its kind cannot hold: a reader skipping
    // it would read an older statement, or a shorter list, as the live one.
    if let Some(statement) = &frontmatter.statement {
        statement.check().map_err(|detail| LedgerError::Parse {
            path: path.to_owned(),
            detail,
        })?;
    }
    Ok(frontmatter.into_entry(body))
}

/// The frontmatter before the closing `+++` fence, and the body after it.
///
/// The closing fence is the first `+++` line whose preceding block parses as
/// TOML — not the first `+++` line outright, because a frontmatter value
/// containing a newline serializes as a multi-line TOML string, and such a
/// string may itself contain a bare `+++` line. A fence-looking line that does
/// not close a parseable block is part of the frontmatter and the scan moves
/// on; the body is returned verbatim minus the trailing newline `append` adds.
fn split_fenced(rest: &str) -> Result<(Frontmatter, String), String> {
    let mut front = String::new();
    let mut first_error: Option<toml::de::Error> = None;
    let mut lines = rest.split_inclusive('\n');
    while let Some(line) = lines.next() {
        if line == "+++\n" || line == "+++" {
            match toml::from_str::<Frontmatter>(&front) {
                Ok(frontmatter) => {
                    let mut body: String = lines.collect();
                    if body.ends_with('\n') {
                        body.pop();
                    }
                    return Ok((frontmatter, body));
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        front.push_str(line);
    }
    Err(first_error.map_or_else(
        || "missing the closing +++ fence".to_owned(),
        |error| error.to_string(),
    ))
}

/// `Timestamp::now`, bumped to be strictly later than every stamp this process
/// has handed out.
///
/// Two draws back-to-back can be equal on a coarse platform clock, and equal
/// stamps would leave read order to the random filename suffix — the sync
/// tests assert the order of entries one command writes, so that order must
/// not be a coin toss. A clock that did not advance is nudged one nanosecond
/// past the last stamp instead. Across processes the wall clock is the order,
/// as it always was; the suffix and `create_new` cover genuinely concurrent
/// writers.
fn monotonic_now() -> jiff::Timestamp {
    static LAST: std::sync::Mutex<Option<jiff::Timestamp>> = std::sync::Mutex::new(None);
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = jiff::Timestamp::now();
    let stamp = match *last {
        Some(previous) if now <= previous => previous + jiff::SignedDuration::from_nanos(1),
        _ => now,
    };
    *last = Some(stamp);
    stamp
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn entry(subject: Option<&str>, text: &str) -> Entry {
        Entry {
            ts: "2026-08-15T22:14:03Z".to_owned(),
            owner: "ses_fff688".to_owned(),
            email: None,
            subject: subject.map(str::to_owned),
            kind: Kind::Note,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: Some("6c42fe71".to_owned()),
            pr: None,
            parents: Vec::new(),
            disposition: None,
            statement: None,
        }
    }
    fn entry_at(ts: &str, subject: Option<&str>, text: &str) -> Entry {
        Entry {
            ts: ts.to_owned(),
            ..entry(subject, text)
        }
    }

    /// The one entry file in `dir`, for a test that inspects what was written.
    fn only_file(dir: &Path) -> PathBuf {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|dirent| dirent.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one entry file: {files:?}");
        files.remove(0)
    }

    #[test]
    fn entries_read_back_in_stamp_order_whatever_the_write_order() {
        // Chronology lives in the filename stamp now, not in a shared file's
        // append order: whoever wrote first by clock reads first, even when
        // the later entry hit the disk earlier.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));

        ledger
            .append(&entry_at(
                "2026-08-15T22:14:03.000000002Z",
                Some("feat/beta"),
                "second",
            ))
            .unwrap();
        ledger
            .append(&entry_at(
                "2026-08-15T22:14:03.000000001Z",
                Some("feat/alpha"),
                "first",
            ))
            .unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].text, "first");
        assert_eq!(read[1].text, "second");
        assert_eq!(read[0].subject.as_deref(), Some("feat/alpha"));
        assert_eq!(read[0].kind, Kind::Note);
        assert_eq!(read[0].anchor.as_deref(), Some("6c42fe71"));
    }

    #[test]
    fn a_ledger_that_does_not_exist_yet_is_empty_rather_than_an_error() {
        // A repository nobody has notched is the normal case, not a failure.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("never-written"));
        assert!(ledger.entries().unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_ledger_ancestor_is_an_error_not_an_empty_ledger() {
        // `exists` turns the ENOTDIR from a file occupying `blocked` into
        // false; `read_dir` must instead preserve the error, or history reads
        // as never-notched.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "not a directory").unwrap();
        let ledger = Ledger::at(blocked.join("a-repo"));

        let error = ledger.entries().unwrap_err();
        assert!(
            matches!(&error, LedgerError::Read { path: actual, .. } if actual.as_path() == ledger.path()),
            "was: {error}"
        );
    }

    #[test]
    fn an_absent_subject_pr_and_anchor_survive_as_absent() {
        // A repo-level entry has no subject; an entry about a deleted branch has
        // no anchor. Neither may come back as an empty string, which would read
        // as a branch named "".
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let bare = Entry {
            anchor: None,
            ..entry(None, "the fork needs a release cut before Friday")
        };
        ledger.append(&bare).unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read[0].subject, None);
        assert_eq!(read[0].anchor, None);
        assert_eq!(read[0].pr, None);
        // And: absent fields are omitted from the frontmatter rather than
        // written as some empty stand-in, so nothing reads back as present.
        let text = std::fs::read_to_string(only_file(ledger.path())).unwrap();
        assert!(!text.contains("subject"), "was: {text}");
        assert!(!text.contains("anchor"), "was: {text}");
    }

    #[test]
    fn a_frontmatter_key_this_version_does_not_know_is_ignored_rather_than_rejected() {
        // Entry files are never rewritten, so a newer binary may add a key and
        // an older one must still read the file. That is the whole evolution
        // story.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("20260815T221403.000000000Z-0000.md"),
            "+++\nts = \"2026-08-15T22:14:03Z\"\nowner = \"x\"\nkind = \"event\"\n\
             from_the_future = \"v\"\n+++\nclaimed\n",
        )
        .unwrap();

        let read = Ledger::at(path).entries().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].kind, Kind::Event);
        assert_eq!(read[0].text, "claimed");
    }

    #[test]
    fn a_multi_line_text_round_trips_verbatim() {
        // The body IS the text — no escaping layer to get wrong in either
        // direction — and a fence-looking line inside the text must stay text
        // rather than truncate the body.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let text = "parked\nby the owner\n+++\nthat line is prose, not a fence";
        ledger.append(&entry(Some("feat/alpha"), text)).unwrap();
        assert_eq!(ledger.entries().unwrap()[0].text, text);
    }

    #[test]
    fn an_evidence_string_containing_a_fence_line_still_round_trips() {
        // TOML serializes a string with newlines as a multi-line string whose
        // lines land verbatim inside the frontmatter — possibly a bare `+++`.
        // The closing fence is therefore the first `+++` line whose block
        // parses as TOML, not the first `+++` line outright; this is the test
        // that breaks if that rule regresses to a plain split.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let sneaky = Entry {
            evidence: vec!["quoted from the review:\n+++\ndo not merge".to_owned()],
            ..entry(Some("feat/alpha"), "promised a follow-up")
        };
        ledger.append(&sneaky).unwrap();
        assert_eq!(ledger.entries().unwrap()[0], sneaky);
    }

    #[test]
    fn a_file_that_is_not_an_entry_is_reported_by_name() {
        // A ledger the tool cannot read must not read as a ledger with nothing
        // in it: that is the silent-empty failure this whole record exists to
        // prevent. One bad file fails the read, and the error names the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        Ledger::at(path.clone())
            .append(&entry(Some("feat/alpha"), "fine"))
            .unwrap();
        std::fs::write(
            path.join("20990101T000000.000000000Z-dead.md"),
            "not a ledger entry at all\n",
        )
        .unwrap();

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(
                &error,
                LedgerError::Parse { path, .. }
                    if path.ends_with("20990101T000000.000000000Z-dead.md")
            ),
            "was: {error}"
        );
    }

    #[test]
    fn an_unreadable_timestamp_in_a_file_is_reported_rather_than_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("20260815T221403.000000000Z-0000.md"),
            "+++\nts = \"last tuesday\"\nowner = \"x\"\nkind = \"note\"\n+++\na\n",
        )
        .unwrap();

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(&error, LedgerError::Timestamp { ts, .. } if ts == "last tuesday"),
            "was: {error}"
        );
    }

    #[test]
    fn a_statement_whose_value_its_kind_cannot_hold_is_reported_rather_than_read_as_none() {
        // A peer's or a hand's entry with a value its kind cannot hold must
        // fail the read: read as no statement, a newest malformed `pull`
        // would hide the valid one before it, and a `depends` list read
        // without its unreadable requirements would be written back without
        // them by the next `knives depends`. `superseded` and `fork-only`
        // hold prose, so any text is theirs.
        for (kind, value) in [
            ("pull", "12a"),
            ("pull", ""),
            ("pull", "-3"),
            ("depends", "acme/demo#7,acme/demo"),
            ("depends", "acme/demo#seven"),
            ("depends", "acme/demo#7,,sibling#49"),
            ("depends", ""),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a-repo");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("20260815T221403.000000000Z-0000.md"),
                format!(
                    "+++\nstatement = {{ kind = \"{kind}\", value = \"{value}\" }}\n\
                     ts = \"2026-08-15T22:14:03Z\"\nowner = \"x\"\nsubject = \"feat/alpha\"\n\
                     kind = \"event\"\n+++\nstated\n"
                ),
            )
            .unwrap();

            let error = Ledger::at(path).entries().unwrap_err();
            assert!(
                matches!(
                    &error,
                    LedgerError::Parse { path, detail }
                        if path.ends_with("20260815T221403.000000000Z-0000.md")
                            && detail.contains(kind)
                ),
                "{kind} = {value:?} was read, or failed otherwise: {error}"
            );
        }
    }

    #[test]
    fn every_statement_value_knives_writes_reads_back() {
        for (kind, value) in [
            ("pull", "1234"),
            ("depends", "acme/demo#7,sibling#49"),
            ("superseded", "feat/alpha-v2"),
            ("superseded", "main@upstream"),
            ("fork-only", "stated with `knives track --fork-only`"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a-repo");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("20260815T221403.000000000Z-0000.md"),
                format!(
                    "+++\nstatement = {{ kind = \"{kind}\", value = \"{value}\" }}\n\
                     ts = \"2026-08-15T22:14:03Z\"\nowner = \"x\"\nsubject = \"feat/alpha\"\n\
                     kind = \"event\"\n+++\nstated\n"
                ),
            )
            .unwrap();
            let read = Ledger::at(path).entries();
            assert!(read.is_ok(), "{kind} = {value:?}: {read:?}");
        }
    }

    /// An entry file under `dir` stating `statement`, an inline table's body.
    fn stating(dir: &Path, name: &str, ts: &str, statement: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(name),
            format!(
                "+++\nstatement = {{ {statement} }}\nts = \"{ts}\"\nowner = \"x\"\n\
                 subject = \"feat/alpha\"\nkind = \"event\"\n+++\nstated\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_statement_kind_a_newer_knives_writes_reads_and_states_nothing_here() {
        // Given: a valid pull statement, and a newer one of a kind this
        // knives does not know, as a peer running a newer knives would send.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        stating(
            &path,
            "20260815T221403.000000000Z-0000.md",
            "2026-08-15T22:14:03Z",
            "kind = \"pull\", value = \"1234\"",
        );
        stating(
            &path,
            "20260815T221500.000000000Z-0000.md",
            "2026-08-15T22:15:00Z",
            "kind = \"blocks\", value = \"anything at all\"",
        );

        // Then: the ledger reads, and the unknown kind leaves the pull live.
        let entries = Ledger::at(path).entries().unwrap();
        assert_eq!(entries.len(), 2);
        let live = crate::statement::Statements::from_entries(&entries);
        assert_eq!(live.pull("feat/alpha"), Some(1234));
    }

    #[test]
    fn a_misspelt_statement_field_is_reported_rather_than_read_as_a_forget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        stating(
            &path,
            "20260815T221403.000000000Z-0000.md",
            "2026-08-15T22:14:03Z",
            "kind = \"pull\", vaule = \"1234\"",
        );

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(&error, LedgerError::Parse { detail, .. } if detail.contains("vaule")),
            "was: {error}"
        );
    }

    #[test]
    fn an_entry_with_an_unreadable_timestamp_cannot_be_written() {
        // The filename stamp derives from `ts`, so a timestamp nothing can
        // order is refused at the write rather than discovered at some later
        // read of the whole directory.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let bad = Entry {
            ts: "last tuesday".to_owned(),
            ..entry(Some("feat/alpha"), "a")
        };
        let error = ledger.append(&bad).unwrap_err();
        assert!(
            matches!(&error, LedgerError::Timestamp { ts, .. } if ts == "last tuesday"),
            "was: {error}"
        );
    }

    #[test]
    fn a_forks_ledger_is_its_upstream_directory_beside_the_state_file() {
        let _lock = crate::config::test_support::environment_lock();
        let environment =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        environment.set("KNIVES_CONFIG_HOME", "/tmp/knives-home");
        let entry = crate::config::RepoEntry::new(
            "https://forge.invalid/Acme/Tool.git",
            "https://forge.invalid/ours/tool.git",
        );
        assert_eq!(
            default_ledger_path(&entry.upstream_name("t")),
            std::path::PathBuf::from("/tmp/knives-home/ledger/acme/tool")
        );
    }

    #[test]
    fn a_file_without_the_md_extension_is_not_an_entry() {
        // An editor's or a sync tool's droppings beside the entries are not
        // entries and not a reason to refuse the whole record.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        Ledger::at(path.clone())
            .append(&entry(Some("feat/alpha"), "fine"))
            .unwrap();
        std::fs::write(path.join(".20990101.md.swp"), "junk").unwrap();

        let read = Ledger::at(path).entries().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].text, "fine");
    }

    #[test]
    fn a_leftover_temporary_file_is_not_an_entry() {
        // Atomic appends leave only final markdown files behind, but a crash can
        // leave the temporary file: it must not poison the ledger read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        let ledger = Ledger::at(path.clone());
        ledger.append(&entry(Some("feat/alpha"), "fine")).unwrap();
        let _temporary = tempfile::NamedTempFile::new_in(path).unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].text, "fine");
    }

    #[test]
    fn filenames_carry_the_stamp_so_lexicographic_order_is_chronological() {
        // `entries` sorts by name and nothing else; the fixed-width stamp is
        // the property that makes that sort a chronology. A second boundary is
        // the trap: 22:14:04 with no subsecond digits must still sort after
        // 22:14:03.999999999, so the stamp always carries nine digits.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        ledger
            .append(&entry_at(
                "2026-08-15T22:14:03.999999999Z",
                Some("feat/alpha"),
                "earlier",
            ))
            .unwrap();
        ledger
            .append(&entry_at(
                "2026-08-15T22:14:04Z",
                Some("feat/alpha"),
                "later",
            ))
            .unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read[0].text, "earlier");
        assert_eq!(read[1].text, "later");

        let mut names: Vec<String> = std::fs::read_dir(ledger.path())
            .unwrap()
            .map(|dirent| dirent.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(
            names[0].starts_with("20260815T221403.999999999Z-"),
            "was: {}",
            names[0]
        );
        assert!(
            names[1].starts_with("20260815T221404.000000000Z-"),
            "was: {}",
            names[1]
        );
        assert_eq!(
            Path::new(&names[0]).extension(),
            Some(OsStr::new("md")),
            "was: {}",
            names[0]
        );
    }

    #[test]
    fn persisting_over_an_existing_entry_is_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.md");
        std::fs::write(&path, "already here").unwrap();
        let temporary = tempfile::NamedTempFile::new_in(dir.path()).unwrap();

        let error = persist_entry(temporary, path.clone()).unwrap_err();
        assert!(
            matches!(&error, LedgerError::Collision { path: actual } if actual.as_path() == path.as_path()),
            "was: {error}"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "already here");
    }

    #[test]
    fn concurrent_writers_produce_distinct_files_that_all_parse() {
        // JSONL needed a lockfile so two agents could not interleave one shared
        // file. A file per entry needs none: `create_new` either wins a fresh
        // name or errors, so the assertions left worth making are that nothing
        // is lost, nothing collides and everything parses when several agents
        // notch at once — the ordinary case on a machine running many.
        const WRITERS: usize = 4;
        const EACH: usize = 25;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");

        std::thread::scope(|scope| {
            for writer in 0..WRITERS {
                let path = path.clone();
                let _ = scope.spawn(move || {
                    let ledger = Ledger::at(path);
                    for index in 0..EACH {
                        // Real writers draw from the monotonic scribe clock; these
                        // do too, so stamps are process-unique and a collision cannot happen.
                        let mut record = entry(Some("feat/alpha"), &format!("{writer}:{index}"));
                        record.ts = monotonic_now().to_string();
                        ledger.append(&record).unwrap();
                    }
                });
            }
        });

        let files = std::fs::read_dir(&path).unwrap().count();
        assert_eq!(files, WRITERS * EACH, "every append is its own file");
        let entries = Ledger::at(path).entries().unwrap();
        assert_eq!(entries.len(), WRITERS * EACH, "every file parses");
        for writer in 0..WRITERS {
            for index in 0..EACH {
                let wanted = format!("{writer}:{index}");
                assert!(
                    entries.iter().any(|entry| entry.text == wanted),
                    "missing: {wanted}"
                );
            }
        }
    }
    fn stamped(subject: Option<&str>, pr: Option<u64>, text: &str) -> Entry {
        Entry {
            pr,
            ..entry(subject, text)
        }
    }

    #[test]
    fn a_subject_filter_keeps_only_that_refs_chronology() {
        let entries = vec![
            stamped(Some("feat/alpha"), None, "one"),
            stamped(Some("feat/beta"), None, "two"),
            stamped(Some("feat/alpha"), None, "three"),
            stamped(None, None, "repo-level"),
        ];
        let (selected, matched) = select(
            &entries,
            &Filter {
                subject: Some("feat/alpha"),
                ..Filter::default()
            },
        );
        assert_eq!(matched, 2);
        let texts: Vec<&str> = selected.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["one", "three"], "oldest first, nothing else");
    }

    #[test]
    fn a_release_ref_is_a_subject_like_any_branch() {
        // Releases are first-class subjects: the audit of what a cut contained is
        // filed under the cut's own name.
        let entries = vec![stamped(
            Some("release/2026-08-15"),
            None,
            "cut with 3 parents",
        )];
        let (selected, _) = select(
            &entries,
            &Filter {
                subject: Some("release/2026-08-15"),
                ..Filter::default()
            },
        );
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn a_pull_request_filter_reads_the_stamped_field_only() {
        let entries = vec![
            stamped(Some("feat/alpha"), Some(1157), "one"),
            stamped(Some("feat/alpha"), None, "mentions #1157 in its text only"),
        ];
        let (selected, matched) = select(
            &entries,
            &Filter {
                pr: Some(1157),
                ..Filter::default()
            },
        );
        assert_eq!(matched, 1);
        assert_eq!(selected[0].text, "one");
    }

    #[test]
    fn a_limit_keeps_the_newest_and_reports_how_many_it_did_not_show() {
        // A window that silently drops the older half is how a reader concludes a
        // branch has no history.
        let entries: Vec<Entry> = (0..25)
            .map(|index| stamped(Some("feat/alpha"), None, &format!("entry {index}")))
            .collect();
        let (selected, matched) = select(
            &entries,
            &Filter {
                limit: Some(20),
                ..Filter::default()
            },
        );
        assert_eq!(matched, 25);
        assert_eq!(selected.len(), 20);
        assert_eq!(selected[0].text, "entry 5");
        assert_eq!(selected[19].text, "entry 24");
    }

    #[test]
    fn the_newest_entry_for_a_subject_is_the_last_one_given() {
        // Last in the order given, which on disk is stamp order: the helper
        // itself never reorders, so a hand-built slice keeps its own order.
        let entries = vec![
            Entry {
                ts: "2026-08-15T23:00:00Z".to_owned(),
                ..stamped(Some("feat/alpha"), None, "given first, clock ahead")
            },
            Entry {
                ts: "2026-08-15T22:00:00Z".to_owned(),
                ..stamped(Some("feat/alpha"), None, "given second, clock behind")
            },
            stamped(Some("feat/beta"), None, "another branch"),
        ];
        assert_eq!(
            newest_for(&entries, "feat/alpha").map(|e| e.text.as_str()),
            Some("given second, clock behind")
        );
        assert_eq!(newest_for(&entries, "feat/never-notched"), None);
    }

    #[test]
    fn an_age_is_the_shortest_form_that_is_still_true() {
        let now: jiff::Timestamp = "2026-08-15T12:00:00Z".parse().unwrap();
        assert_eq!(age("2026-08-15T11:59:31Z", now).as_deref(), Some("now"));
        assert_eq!(age("2026-08-15T11:48:00Z", now).as_deref(), Some("12m"));
        assert_eq!(age("2026-08-15T08:00:00Z", now).as_deref(), Some("4h"));
        assert_eq!(age("2026-08-12T12:00:00Z", now).as_deref(), Some("3d"));
        // A clock that ran backwards is not a negative age.
        assert_eq!(age("2026-08-15T12:00:30Z", now).as_deref(), Some("now"));
        // Only reachable for an entry assembled by hand: `entries` rejects these.
        assert_eq!(age("last tuesday", now), None);
    }

    #[test]
    fn stamps_drawn_back_to_back_strictly_advance() {
        // A tight loop outpaces the clock's real granularity somewhere; the
        // bump keeps every stamp strictly later than the one before anyway.
        let mut previous = monotonic_now();
        for _ in 0..1000 {
            let next = monotonic_now();
            assert!(next > previous, "stamps must advance: {next} <= {previous}");
            previous = next;
        }
    }

    #[test]
    fn entries_written_back_to_back_read_back_in_write_order() {
        // One hundred writes as fast as the machine can make them: equal
        // wall-clock stamps would hand the order to the random suffix, and the
        // monotonic bump is what forbids equal stamps within a process.
        let dir = tempfile::tempdir().unwrap();
        let scribe = scribe(dir.path());
        for index in 0..100 {
            scribe
                .event(Some("feat/alpha"), format!("entry {index}"), None)
                .unwrap();
        }
        let texts: Vec<String> = scribe
            .ledger
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.text)
            .collect();
        let wanted: Vec<String> = (0..100).map(|index| format!("entry {index}")).collect();
        assert_eq!(texts, wanted);
    }
    fn scribe(dir: &std::path::Path) -> Scribe {
        Scribe::new(
            Ledger::at(dir.join("ledger").join("a-repo")),
            UpstreamName::new("a-repo"),
            dir.join("not-a-repository"),
            "ses_fff688".to_owned(),
        )
    }

    #[test]
    fn an_event_stamps_the_fields_no_caller_supplies() {
        let dir = tempfile::tempdir().unwrap();
        let scribe = scribe(dir.path());

        let written = scribe
            .event(
                Some("feat/alpha"),
                "claimed: fixing the parser".to_owned(),
                Some(4545),
            )
            .unwrap();

        assert_eq!(written.kind, Kind::Event);
        assert_eq!(written.owner, "ses_fff688");
        assert_eq!(written.subject.as_deref(), Some("feat/alpha"));
        assert_eq!(written.pr, Some(4545));
        assert!(
            written.ts.parse::<jiff::Timestamp>().is_ok(),
            "was: {}",
            written.ts
        );
        // And: it is on disk, not just returned.
        assert_eq!(scribe.ledger.entries().unwrap(), vec![written]);
    }

    #[test]
    fn an_entry_carries_the_writers_git_email_and_none_when_git_has_none() {
        // Given: a checkout whose own config names its committer.
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.email", "writer@example.test"][..],
        ] {
            let status = crate::bind::git(&checkout).args(args).status().unwrap();
            assert!(status.success(), "git {args:?}");
        }
        let scribe = Scribe::new(
            Ledger::at(dir.path().join("ledger").join("a-repo")),
            UpstreamName::new("a-repo"),
            checkout,
            "ses_fff688".to_owned(),
        );

        // When: it writes an entry.
        let written = scribe.event(None, "noted".to_owned(), None).unwrap();

        // Then: the entry carries that address, on disk as in the report.
        assert_eq!(written.email.as_deref(), Some("writer@example.test"));
        let text = std::fs::read_to_string(only_file(scribe.ledger.path())).unwrap();
        assert!(
            text.contains("email = \"writer@example.test\""),
            "was: {text}"
        );
        assert_eq!(scribe.ledger.entries().unwrap(), vec![written]);

        // And: an entry with no address writes no email field, and reads
        // back as none, as every entry written before the field does.
        let plain_dir = tempfile::tempdir().unwrap();
        let plain = Ledger::at(plain_dir.path().join("a-repo"));
        let bare = entry(None, "no address");
        plain.append(&bare).unwrap();
        let text = std::fs::read_to_string(only_file(plain.path())).unwrap();
        assert!(!text.contains("email"), "was: {text}");
        assert_eq!(plain.entries().unwrap(), vec![bare]);
    }

    #[test]
    fn an_anchor_is_omitted_when_the_subject_does_not_resolve() {
        // A branch deleted since, a reaped release ref, or a path that is not a
        // repository. None of them invalidates the entry, so none of them may
        // fail the write.
        let dir = tempfile::tempdir().unwrap();
        let written = scribe(dir.path())
            .event(Some("feat/long-gone"), "claim released".to_owned(), None)
            .unwrap();
        assert_eq!(written.anchor, None);
    }

    #[test]
    fn a_note_carries_its_evidence_and_a_repo_level_entry_has_no_subject() {
        let dir = tempfile::tempdir().unwrap();
        let written = scribe(dir.path())
            .record(&Draft {
                subject: None,
                kind: Kind::Note,
                disposition: None,
                statement: None,
                text: "the release remote is out of date".to_owned(),
                evidence: vec!["06d778b9".to_owned(), "a-repo#1157".to_owned()],
                pr: None,
                parents: Vec::new(),
            })
            .unwrap();

        assert_eq!(written.kind, Kind::Note);
        assert_eq!(written.subject, None);
        assert_eq!(written.evidence, ["06d778b9", "a-repo#1157"]);
    }

    #[test]
    fn an_append_that_cannot_be_written_is_an_error_rather_than_a_shrug() {
        // A ledger append failure fails its command loudly: the ledger and the
        // state file live in one config home, and a write that can fail one
        // can fail the other. Here a stray file squats on the directory name.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger").join("a-repo");
        std::fs::create_dir_all(dir.path().join("ledger")).unwrap();
        std::fs::write(&path, "a file where the ledger directory should be").unwrap();
        let blocked = Scribe::new(
            Ledger::at(path),
            UpstreamName::new("a-repo"),
            dir.path().to_owned(),
            "ses_fff688".to_owned(),
        )
        .event(Some("feat/alpha"), "claimed".to_owned(), None);
        assert!(
            matches!(blocked, Err(LedgerError::Write { .. })),
            "was: {blocked:?}"
        );
    }

    #[test]
    fn a_disposition_round_trips_and_an_entry_without_one_stays_clean() {
        // Removing the frontmatter mirror would lose a ruling on disk; emitting
        // None would turn ordinary notes into an invented disposition field.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let disposition = Entry {
            disposition: Some("ruled-out".to_owned()),
            ..entry(Some("feat/alpha"), "split into a plugin")
        };
        ledger.append(&disposition).unwrap();
        assert_eq!(ledger.entries().unwrap(), vec![disposition]);

        let plain_dir = tempfile::tempdir().unwrap();
        let plain = Ledger::at(plain_dir.path().join("a-repo"));
        plain
            .append(&entry(Some("feat/beta"), "still investigating"))
            .unwrap();
        let text = std::fs::read_to_string(only_file(plain.path())).unwrap();
        assert!(!text.contains("disposition"), "was: {text}");
    }

    #[test]
    fn a_statement_round_trips_as_an_inline_table_and_an_entry_without_one_stays_clean() {
        // The statement is one machine-readable line in the file, and a forget
        // (no value) must come back as a forget rather than as no statement.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let stated = Entry {
            kind: Kind::Event,
            statement: Some(Statement {
                kind: crate::statement::StatementKind::Pull,
                value: Some("1234".to_owned()),
            }),
            ..entry(Some("feat/alpha"), "stated as #1234")
        };
        ledger.append(&stated).unwrap();
        assert_eq!(ledger.entries().unwrap(), vec![stated]);
        let text = std::fs::read_to_string(only_file(ledger.path())).unwrap();
        assert!(
            text.contains("\nstatement = { kind = \"pull\", value = \"1234\" }\n"),
            "was: {text}"
        );

        let forget_dir = tempfile::tempdir().unwrap();
        let forget_ledger = Ledger::at(forget_dir.path().join("a-repo"));
        let forget = Entry {
            kind: Kind::Event,
            statement: Some(Statement {
                kind: crate::statement::StatementKind::Pull,
                value: None,
            }),
            ..entry(Some("feat/alpha"), "pull request statement forgotten")
        };
        forget_ledger.append(&forget).unwrap();
        assert_eq!(forget_ledger.entries().unwrap(), vec![forget]);
        let text = std::fs::read_to_string(only_file(forget_ledger.path())).unwrap();
        assert!(
            text.contains("\nstatement = { kind = \"pull\" }\n"),
            "was: {text}"
        );

        // And: beside a field toml writes as a section, the statement still
        // reads back as the entry's own rather than as part of the section.
        let sectioned_dir = tempfile::tempdir().unwrap();
        let sectioned_ledger = Ledger::at(sectioned_dir.path().join("a-repo"));
        let sectioned = Entry {
            kind: Kind::Event,
            statement: Some(Statement {
                kind: crate::statement::StatementKind::Superseded,
                value: Some("feat/alpha-v2".to_owned()),
            }),
            parents: vec![RecordedParent {
                commit: "a".repeat(40),
                branches: vec!["feat/alpha".to_owned()],
            }],
            ..entry(Some("feat/alpha"), "superseded by feat/alpha-v2")
        };
        sectioned_ledger.append(&sectioned).unwrap();
        assert_eq!(sectioned_ledger.entries().unwrap(), vec![sectioned]);

        let plain_dir = tempfile::tempdir().unwrap();
        let plain = Ledger::at(plain_dir.path().join("a-repo"));
        plain
            .append(&entry(Some("feat/beta"), "still investigating"))
            .unwrap();
        let text = std::fs::read_to_string(only_file(plain.path())).unwrap();
        assert!(!text.contains("statement"), "was: {text}");
    }

    #[test]
    fn a_recorded_parent_set_round_trips_with_every_branch_name() {
        // The parent set is the one record of which branch a released parent
        // was that survives the bookmark moving; a file that kept the commit
        // but lost a name would let `include` carry that branch twice.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let cut = Entry {
            kind: Kind::Event,
            parents: vec![
                RecordedParent {
                    commit: "a".repeat(40),
                    branches: vec!["anchor/alpha".to_owned(), "feat/alpha".to_owned()],
                },
                RecordedParent {
                    commit: "b".repeat(40),
                    branches: Vec::new(),
                },
            ],
            ..entry(Some("release/2026-08-04"), "cut release/2026-08-04")
        };
        ledger.append(&cut).unwrap();
        assert_eq!(ledger.entries().unwrap(), vec![cut]);

        let plain_dir = tempfile::tempdir().unwrap();
        let plain = Ledger::at(plain_dir.path().join("a-repo"));
        plain
            .append(&entry(Some("feat/beta"), "still investigating"))
            .unwrap();
        let text = std::fs::read_to_string(only_file(plain.path())).unwrap();
        assert!(!text.contains("parents"), "was: {text}");
    }

    #[test]
    fn filters_select_by_class() {
        // Class filtering must retain dispositions as notes while still letting a
        // discovery read isolate terminal rulings.
        let entries = vec![
            Entry {
                kind: Kind::Event,
                ..entry(Some("feat/alpha"), "synced")
            },
            entry(Some("feat/alpha"), "still investigating"),
            Entry {
                disposition: Some("ruled-out".to_owned()),
                ..entry(Some("feat/alpha"), "split into a plugin")
            },
        ];

        for (class, expected) in [
            (Some(EntryClass::Disposition), 1),
            (Some(EntryClass::Note), 2),
            (Some(EntryClass::Event), 1),
            (None, 3),
        ] {
            let (selected, _) = select(
                &entries,
                &Filter {
                    only: class,
                    ..Filter::default()
                },
            );
            assert_eq!(selected.len(), expected, "class: {class:?}");
        }
    }

    #[test]
    fn a_pr_subject_matches_the_pr_filter() {
        // Removing the #number subject fallback would strand cross-repository
        // rulings from the PR history that gives them their meaning.
        let entry = Entry {
            subject: Some("#4545".to_owned()),
            pr: None,
            ..entry(None, "split into a plugin")
        };
        let by_pr = select(
            std::slice::from_ref(&entry),
            &Filter {
                pr: Some(4545),
                ..Filter::default()
            },
        );
        let by_subject = select(
            std::slice::from_ref(&entry),
            &Filter {
                subject: Some("#4545"),
                ..Filter::default()
            },
        );

        assert_eq!(by_pr.1, 1);
        assert_eq!(by_subject.1, 1);
    }

    #[test]
    fn verify_flags_vanished_evidence_and_moved_anchors() {
        // Removing the membership check lets a vanished cited SHA look valid;
        // ignoring a changed subject tip lets an inherited conclusion look current.
        let entries = vec![
            Entry {
                evidence: vec!["aaaaaaaaaaaa".to_owned()],
                anchor: None,
                ..entry(Some("feat/present"), "still present")
            },
            Entry {
                ts: "2026-08-15T22:14:04Z".to_owned(),
                evidence: vec!["deadbeefdead".to_owned()],
                anchor: None,
                ..entry(Some("feat/missing"), "the cited change vanished")
            },
            Entry {
                ts: "2026-08-15T22:14:05Z".to_owned(),
                anchor: Some("bbbbbbbbbbbb".to_owned()),
                ..entry(Some("feat/moved"), "the branch changed")
            },
        ];
        let visible = BTreeSet::from([
            "aaaaaaaaaaaa9999".to_owned(),
            "bbbbbbbbbbbb9999".to_owned(),
            "cccccccccccc9999".to_owned(),
        ]);
        let tips = BTreeMap::from([("feat/moved".to_owned(), "cccccccccccc9999".to_owned())]);

        assert_eq!(
            verify_entries(&entries, &visible, &tips),
            vec![
                VerifyFlag {
                    ts: "2026-08-15T22:14:04Z".to_owned(),
                    subject: Some("feat/missing".to_owned()),
                    what: "evidence deadbeefdead not found in this repository".to_owned(),
                },
                VerifyFlag {
                    ts: "2026-08-15T22:14:05Z".to_owned(),
                    subject: Some("feat/moved".to_owned()),
                    what: "anchor moved: bbbbbbbbbbbb -> cccccccccccc9999".to_owned(),
                },
            ]
        );
    }
}
