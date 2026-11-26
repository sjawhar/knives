//! Semantic identifiers.
//!
//! Each of these is a distinct type rather than a `String` because mixing them
//! is a real bug class in this domain, not a hypothetical one. A change id and a
//! commit id look identical, are both short hex-ish strings, and mean entirely
//! different things: one change can exist as several commits, which is exactly
//! what divergence is. Likewise a local bookmark and its remote-tracking
//! counterpart share a name but point at different commits.

use std::cmp::Ordering;
use std::fmt;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    ChangeId,
    "A jj change. Stable across rewrites, and identical across disconnected clones, which is why the same change rewritten in two places collides."
);
string_id!(CommitId, "One concrete commit. A change may have several.");
string_id!(
    RepoName,
    "A managed repo's name in the registry: the command-line shorthand."
);
string_id!(
    UpstreamName,
    "The repository a fork is a fork of, as lowercase `<owner>/<name>`: what a fork's ledger directory and its state are kept under, so every machine keys them alike whatever its registry calls the fork. An upstream that is a filesystem path names no forge repository, and the fork's registry key is its name. See [`crate::config::RepoEntry::upstream_name`]."
);
string_id!(
    RemoteName,
    "A remote's name, which this tool only ever derives from a role."
);
string_id!(
    BranchName,
    "A bookmark name with no remote qualifier and no decoration."
);
string_id!(WorkspaceName, "A jj workspace.");
string_id!(
    SeriesName,
    "A named release series of one fork: a line of dated cuts on its own base, alongside the entry's default series. One path segment, checked when the registry loads."
);

impl ChangeId {
    /// The prefix this program shows; see [`short_id`].
    pub fn short(&self) -> &str {
        short_id(&self.0)
    }
}

impl CommitId {
    /// The prefix this program shows; see [`short_id`].
    pub fn short(&self) -> &str {
        short_id(&self.0)
    }
}

impl BranchName {
    /// A branch name as typed on the command line, checked to name a bookmark.
    ///
    /// `start`, `finish`, `track` and `depends` derive a workspace directory from
    /// the name, so a name that is not a bookmark is a path: before the workspace
    /// identity check, `finish ..` flattened to the checkout's grandparent and
    /// `finish ''` to its parent, and removed them. Rejected here so the degenerate
    /// spellings never reach a path at all and the message is knives', not jj's.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("a branch name is required".to_owned());
        }
        if value.starts_with('-') {
            return Err(format!(
                "{value:?} is not a branch name; it reads as an option"
            ));
        }
        if value.split('/').any(|segment| {
            segment.is_empty() || segment == "." || segment == ".." || segment.starts_with('.')
        }) {
            return Err(format!(
                "{value:?} is not a branch name: every `/`-separated segment must be non-empty \
                 and must not start with `.`"
            ));
        }
        Ok(Self::new(value))
    }
}

/// The first twelve characters of an identifier: what jj shows, enough to be
/// unique in a fork, short enough to read in a line of prose. A shorter id is
/// returned whole.
///
/// For an id the program holds as text — a forge oid, a lockfile lock, a
/// recorded anchor. [`CommitId::short`] and [`ChangeId::short`] are this for
/// the ids it has typed. Only identifiers are shortened this way: a truncated
/// branch name or file path names something that does not exist.
pub fn short_id(id: &str) -> &str {
    id.char_indices().nth(12).map_or(id, |(end, _)| &id[..end])
}

/// A branch in a particular repo.
///
/// These two always travel together: every claim, and every statement about a
/// branch, is keyed by the pair, so the key is formatted in one place rather
/// than by each caller ([`BranchTarget::key`]). The repository is the fork's
/// [`UpstreamName`], the name its state is kept under. It has no `Display`: a
/// message for a person names the fork by its registry key instead, and an
/// upstream name followed by a branch name holding a `/` reads as another
/// fork's branch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct BranchTarget {
    pub repo: UpstreamName,
    pub branch: BranchName,
}

impl BranchTarget {
    pub const fn new(repo: UpstreamName, branch: BranchName) -> Self {
        Self { repo, branch }
    }

    /// `<upstream name>/<branch>`: what the state file keys this branch's
    /// claim by.
    pub fn key(&self) -> String {
        format!("{}/{}", self.repo, self.branch)
    }
}

/// A bookmark as it appears on a commit: local, or tracking a remote.
///
/// Keeping these apart in the type system is the fix for a bug that shipped in
/// an earlier draft. `jj` renders both under one name, so a tip map keyed by
/// bare string silently took whichever row came last. On a real repo the local
/// and remote tips genuinely differed, so release parents were compared against
/// the wrong commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum BookmarkRef {
    Local(BranchName),
    Remote {
        branch: BranchName,
        remote: RemoteName,
    },
}

pub const RELEASE_PREFIX: &str = "release/";

/// What follows a dated series' prefix in the name of one of its cuts, for a
/// message: a date, then nothing, a same-day counter `.N`, or a `-<label>`
/// ([`CutKey`]).
pub const CUT_GRAMMAR: &str = "YYYY-MM-DD[.N|-<label>]";

/// Why `segment` cannot be one component of a git ref, by git's
/// `check-ref-format` rules; `None` when it can.
///
/// The one rule for a name knives makes a ref of: a series name, and the
/// date and label of a dated cut ([`CutKey`]).
pub fn ref_segment_flaw(segment: &str) -> Option<&'static str> {
    if segment.starts_with('.') {
        return Some("it starts with '.'");
    }
    if segment.ends_with('.') {
        return Some("it ends with '.'");
    }
    if segment.strip_suffix(".lock").is_some() {
        return Some("it ends with '.lock'");
    }
    if segment.contains("..") {
        return Some("it contains '..'");
    }
    if segment.contains("@{") {
        return Some("it contains '@{'");
    }
    if segment == "@" {
        return Some("it is '@'");
    }
    if segment
        .chars()
        .any(|c| c.is_ascii_control() || c == '\u{7f}')
    {
        return Some("it contains a control character");
    }
    if segment
        .chars()
        .any(|c| matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Some("it contains one of space ~ ^ : ? * [ \\");
    }
    None
}

/// Whether a release reference is one of ours for the configured publishing role.
///
/// Local releases are always ours. Remote releases are ours only when their
/// remote is the configured publish remote: an `origin` release-shaped ref is
/// a misplaced branch, rather than a release, in a split-release repository.
/// `upstream` remains somebody else's cut, while `git` is jj's internal tracking
/// view rather than a remote.
/// Naming is scheme-dependent: a dated series owns its own cuts
/// ([`ReleaseScheme::owns`]), so another series' cut is never this one's
/// release, while a fixed release is the one configured branch.
pub fn is_our_release(
    reference: &BookmarkRef,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> bool {
    if !scheme.owns(reference.branch().as_str()) {
        return false;
    }
    match reference {
        BookmarkRef::Local(_) => true,
        BookmarkRef::Remote { remote, .. } => remote.as_str() == publish_remote,
    }
}

/// How this fork names its releases.
///
/// Derived from configuration and matched exhaustively at every release-aware
/// site, so the compiler forces each of them — including ones added later — to
/// answer "what does this mean when the release is one fixed branch?", and
/// "what does this mean for a named series?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseScheme {
    /// Dated `release/YYYY-MM-DD[.N|-<label>]` cuts of the entry's default
    /// series. The default, and the historical behavior.
    Dated,
    /// Dated `release/<series>/YYYY-MM-DD[.N|-<label>]` cuts of a named
    /// series, which has its own base. Every dated rule applies; only the
    /// names differ.
    Series(SeriesName),
    /// One integration branch that is rebuilt and advanced in place. The branch's
    /// previous position plays the role of the previous release.
    Fixed(BranchName),
}

impl ReleaseScheme {
    /// What follows this series' prefix in `name`, when `name` is one of its
    /// dated cuts in any shape: `2026-08-05.2` of `release/2026-08-05.2`, or
    /// of `release/9.2/2026-08-05.2` in series `9.2`.
    ///
    /// One `/`-free segment, so the default series never reads a named
    /// series' `release/9.2/2026-10-21` as its own: before series existed,
    /// that name ordered as `("9", 0)`, after every date, and would have been
    /// the default series' newest cut.
    fn dated_suffix<'a>(&self, name: &'a str) -> Option<&'a str> {
        let rest = match self {
            Self::Dated => name.strip_prefix(RELEASE_PREFIX)?,
            Self::Series(series) => name
                .strip_prefix(RELEASE_PREFIX)?
                .strip_prefix(series.as_str())?
                .strip_prefix('/')?,
            Self::Fixed(_) => return None,
        };
        (!rest.is_empty() && !rest.contains('/')).then_some(rest)
    }

    /// Whether `name` is one of this scheme's own releases: a cut of this
    /// series, or the fixed branch. Narrower than [`is_release_name`], which
    /// answers whether a name is a release of any series of the fork.
    pub fn owns(&self, name: &str) -> bool {
        match self {
            Self::Fixed(fixed) => name == fixed.as_str(),
            Self::Dated | Self::Series(_) => self.dated_suffix(name).is_some(),
        }
    }

    /// Where `name` sorts among this series' dated cuts ([`CutKey`]): the one
    /// parser of `release/[<series>/]YYYY-MM-DD[.N|-<label>]`.
    ///
    /// `None` for a name the series does not own, for one it owns but cannot
    /// read, such as `release/9.2/foo` or a label git cannot hold in a ref
    /// ([`Self::ref_flaw`]), and always under `Fixed`. Such a name is never
    /// the newest cut, and the reap never takes it: the reaper enumerates
    /// release refs on any remote, where upstream's own `release/0.3.190`
    /// style branches also live, and a prefix test would hand those to
    /// `bookmark forget`.
    pub fn cut_key<'a>(&self, name: &'a str) -> Option<CutKey<'a>> {
        CutKey::parse(self.dated_suffix(name)?)
    }

    /// Why git cannot hold `name`, one of this series' dated cuts in any
    /// shape, as a ref ([`ref_segment_flaw`] of what follows the series'
    /// prefix), or `None`.
    pub fn ref_flaw(&self, name: &str) -> Option<&'static str> {
        self.dated_suffix(name).and_then(ref_segment_flaw)
    }

    /// The name shape this scheme's cuts take, for a message.
    pub fn cut_shape(&self) -> String {
        match self {
            Self::Dated => format!("{RELEASE_PREFIX}{CUT_GRAMMAR}"),
            Self::Series(series) => format!("{RELEASE_PREFIX}{series}/{CUT_GRAMMAR}"),
            Self::Fixed(fixed) => fixed.to_string(),
        }
    }
}

/// Where one of a series' dated cuts sorts: by its date, then by the rest of
/// its name in version order, then by that rest as bytes.
///
/// The rest is empty, a same-day counter `.N` (one or more digits), or a
/// `-<label>` (one or more characters, no `/`): `2026-11-15-speed-8.0.9`
/// has the rest `-speed-8.0.9`. So `.1.2`, a bare `.` and `.x` do not parse.
/// Nor does a name git cannot hold as a ref ([`ref_segment_flaw`]), such as
/// `-x~1`, `-a b` or `-x.lock`: no ref could ever publish it.
/// Version order reads the rest run by run: a run of digits compares as a
/// number, any other run as bytes, and a digit run sorts before any other run,
/// as `sort -V` does. A rest that is a prefix of another sorts first. So
/// `-speed-8.0.9` < `-speed-8.0.10` < `-speed-8.1` < `-speed-8.1.1`, and a
/// `.N` sorts after each `-<label>` of its date, because `-` sorts before `.`.
/// The bytes last make the order total: `-v08` and `-v8` are one version and
/// still two names.
///
/// Each series has this one order. The reap keeps only the newest cut, so a
/// later name on the same date supersedes an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutKey<'a> {
    date: &'a str,
    rest: &'a str,
}

impl<'a> CutKey<'a> {
    /// The key of `tail`, what follows the series' prefix in a cut's name.
    fn parse(tail: &'a str) -> Option<Self> {
        let (date, rest) = tail.split_at_checked(10)?;
        let dated = date.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        });
        let readable = match rest.as_bytes().split_first() {
            None => true,
            Some((b'.', counter)) => !counter.is_empty() && counter.iter().all(u8::is_ascii_digit),
            Some((b'-', label)) => !label.is_empty() && !label.contains(&b'/'),
            Some(_) => false,
        };
        (dated && readable && ref_segment_flaw(tail).is_none()).then_some(Self { date, rest })
    }
}

impl Ord for CutKey<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.date
            .cmp(other.date)
            .then_with(|| version_runs(self.rest).cmp(version_runs(other.rest)))
            .then_with(|| self.rest.cmp(other.rest))
    }
}

impl PartialOrd for CutKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One run of a cut's rest, as version order compares it: the variants'
/// order puts a number before text.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum VersionRun<'a> {
    /// A run of digits, its leading zeros dropped: a longer number is the
    /// larger, and two of one length compare as bytes, so no length overflows.
    Number { length: usize, digits: &'a [u8] },
    /// Any other run, compared as bytes.
    Text(&'a [u8]),
}

/// The runs of `rest`: each maximal stretch of digits or of other bytes.
fn version_runs(rest: &str) -> impl Iterator<Item = VersionRun<'_>> {
    rest.as_bytes()
        .chunk_by(|left, right| left.is_ascii_digit() == right.is_ascii_digit())
        .map(|run| {
            if run.first().is_some_and(u8::is_ascii_digit) {
                let zeros = run.iter().take_while(|&&byte| byte == b'0').count();
                let digits = run.get(zeros..).unwrap_or_default();
                VersionRun::Number {
                    length: digits.len(),
                    digits,
                }
            } else {
                VersionRun::Text(run)
            }
        })
}

/// Whether a branch name is a release under this scheme: the dated prefix, or the
/// one fixed integration branch.
///
/// Every dated series shares the prefix, so a cut of any series is a release
/// to every other: never a member branch. Which series owns a cut is
/// [`ReleaseScheme::owns`].
pub fn is_release_name(branch: &BranchName, scheme: &ReleaseScheme) -> bool {
    match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => {
            branch.as_str().starts_with(RELEASE_PREFIX)
        }
        ReleaseScheme::Fixed(name) => branch == name,
    }
}

/// Whether `reference` is a dated cut of any series of a fork, behind the
/// trust boundary [`is_our_release`] draws.
///
/// That is the default series' `release/<cut>` or a named series'
/// `release/<series>/<cut>`, where `<cut>` reads as a [`CutKey`]. Every
/// series' cut is a release commit, never an orphan of another's.
pub fn is_our_dated_cut(reference: &BookmarkRef, publish_remote: &str) -> bool {
    let dated = reference
        .branch()
        .as_str()
        .strip_prefix(RELEASE_PREFIX)
        .map(|rest| rest.split_once('/').map_or(rest, |(_, cut)| cut))
        .and_then(CutKey::parse)
        .is_some();
    dated
        && match reference {
            BookmarkRef::Local(_) => true,
            BookmarkRef::Remote { remote, .. } => remote.as_str() == publish_remote,
        }
}

impl BookmarkRef {
    /// The inverse of `Display`: `branch@remote` is a remote ref, anything else
    /// a local one. One parser for the grammar, so a display form crossing a
    /// module boundary as a string comes back as the same value.
    pub fn parse(text: &str) -> Self {
        match text.rsplit_once('@') {
            Some((branch, remote)) if !branch.is_empty() && !remote.is_empty() => Self::Remote {
                branch: BranchName::new(branch),
                remote: RemoteName::new(remote),
            },
            _ => Self::Local(BranchName::new(text)),
        }
    }

    pub const fn branch(&self) -> &BranchName {
        match self {
            Self::Local(branch) | Self::Remote { branch, .. } => branch,
        }
    }

    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Whether this is jj's own `@git` tracking view of a bookmark rather than
    /// a ref on a remote. It mirrors the local bookmark and names nothing of its
    /// own, so it is not a remote a branch can be fetched from or exist only on.
    pub fn is_git_view(&self) -> bool {
        matches!(self, Self::Remote { remote, .. } if remote.is_git_view())
    }
}

impl RemoteName {
    /// Whether this names jj's `git` tracking view rather than a remote.
    pub fn is_git_view(&self) -> bool {
        self.as_str() == "git"
    }
}

impl fmt::Display for BookmarkRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(branch) => write!(f, "{branch}"),
            Self::Remote { branch, remote } => write!(f, "{branch}@{remote}"),
        }
    }
}

/// The pull request number a `pr-<n>` bookmark refers to.
///
/// Fetching a pull request head creates a bookmark named for its number, not for
/// the branch the pull request came from. Matching pull requests by branch name
/// therefore never found them, so every fetched head was reported as a branch with
/// no pull request, advising us to open one for a pull request that was already
/// open. On one real repository that was 17 of 89 findings.
pub fn pull_number_from_bookmark(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("pr-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Something a branch cannot land before, as one token of a `depends`
/// statement.
///
/// Dependencies cross forks, which is the case that motivated them: a change
/// in one fork needing a pull request in a sibling, where dropping one from a
/// release without the other ships a release that cannot work. Within one
/// fork, a member can need another member. Each token says which it names:
///
/// - `<repo>#<number>`: pull request `<number>` on the upstream of the fork
///   kept under `<repo>`, its [`UpstreamName`].
/// - `origin:<owner>/<name>:<branch>`: the member branch `<branch>` of the
///   fork whose ledger states it, pushed to its own repository
///   `<owner>/<name>`. A ref name holds no `:`, so the first one after the
///   repository ends it. The repository says where the branch was when it
///   was stated; the branch is looked up in that fork, whatever its
///   repository is called since.
/// - `origin:<owner>/<name>#<number>`: pull request `<number>` on a fork's
///   own repository ([`Requirement::ForkPull`]), which only an older knives wrote.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Requirement {
    Upstream {
        repo: UpstreamName,
        number: u64,
    },
    /// A pull request on a fork's own repository. Pull requests go upstream,
    /// so `depends` records none; one an older knives recorded is still
    /// read, so its ledger reads whole, and `status` names the command that
    /// takes it back.
    ForkPull {
        repository: String,
        number: u64,
    },
    ForkBranch {
        repository: String,
        branch: BranchName,
    },
}

/// How a requirement on a fork's own repository begins: the role of the
/// remote that names that repository.
const ON_ORIGIN: &str = "origin:";

impl Requirement {
    /// One token as written ([`Requirement`] has the forms). A token without
    /// the `origin:` prefix is split at its last `#`, and its repo is
    /// whatever was written there; `knives depends` resolves a registry key
    /// to the fork's upstream name before recording it. `None` for anything
    /// else, a branch holding the `,` that joins tokens among it.
    pub fn parse(text: &str) -> Option<Self> {
        let Some(rest) = text.strip_prefix(ON_ORIGIN) else {
            let (repo, number) = text.rsplit_once('#')?;
            if repo.is_empty() {
                return None;
            }
            return Some(Self::Upstream {
                repo: UpstreamName::new(repo),
                number: number.parse().ok()?,
            });
        };
        if let Some((repository, branch)) = rest.split_once(':') {
            let branch = BranchName::parse(branch)
                .ok()
                .filter(|branch| !branch.as_str().contains(','))?;
            return is_repository(repository).then(|| Self::ForkBranch {
                repository: repository.to_owned(),
                branch,
            });
        }
        let (repository, number) = rest.rsplit_once('#')?;
        if !is_repository(repository) {
            return None;
        }
        Some(Self::ForkPull {
            repository: repository.to_owned(),
            number: number.parse().ok()?,
        })
    }

    /// The repository a fork pull request or member record spells.
    pub fn repository(&self) -> Option<&str> {
        match self {
            Self::ForkPull { repository, .. } | Self::ForkBranch { repository, .. } => {
                Some(repository)
            }
            Self::Upstream { .. } => None,
        }
    }

    /// Whether this and `other` are records of one member branch, whatever
    /// repository each spells.
    pub fn same_member(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ForkBranch { branch: one, .. }, Self::ForkBranch { branch: two, .. }) => {
                one == two
            }
            _ => false,
        }
    }

    /// Whether `text` spells this record: the same kind, number or branch,
    /// and repository, the repository's letter case aside (a forge's
    /// `<owner>/<name>` is case-blind; a branch name is not). A text that is
    /// no token is compared as it is written.
    pub fn spelled_as(&self, text: &str) -> bool {
        let Some(named) = Self::parse(text) else {
            return self.to_string() == text;
        };
        match (self, &named) {
            (
                Self::Upstream { repo, number },
                Self::Upstream {
                    repo: other,
                    number: other_number,
                },
            ) => number == other_number && repo.as_str().eq_ignore_ascii_case(other.as_str()),
            (
                Self::ForkPull { repository, number },
                Self::ForkPull {
                    repository: other,
                    number: other_number,
                },
            ) => number == other_number && repository.eq_ignore_ascii_case(other),
            (
                Self::ForkBranch { repository, branch },
                Self::ForkBranch {
                    repository: other,
                    branch: other_branch,
                },
            ) => branch == other_branch && repository.eq_ignore_ascii_case(other),
            _ => false,
        }
    }
}

/// Whether `text` is a forge repository's `<owner>/<name>`, holding none of
/// the characters a token is split at.
fn is_repository(text: &str) -> bool {
    text.split_once('/')
        .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        && !text.contains([',', '#', ':'])
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upstream { repo, number } => write!(f, "{repo}#{number}"),
            Self::ForkPull { repository, number } => write!(f, "{ON_ORIGIN}{repository}#{number}"),
            Self::ForkBranch { repository, branch } => {
                write!(f, "{ON_ORIGIN}{repository}:{branch}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    #[test]
    fn a_requirement_round_trips_and_refuses_nonsense() {
        use super::{BranchName, Requirement, UpstreamName};
        for (text, parsed) in [
            (
                "swe#4545",
                Requirement::Upstream {
                    repo: UpstreamName::new("swe"),
                    number: 4545,
                },
            ),
            (
                "origin:acme/swe#12",
                Requirement::ForkPull {
                    repository: "acme/swe".to_owned(),
                    number: 12,
                },
            ),
            (
                "origin:acme/swe:fix/a#1",
                Requirement::ForkBranch {
                    repository: "acme/swe".to_owned(),
                    branch: BranchName::new("fix/a#1"),
                },
            ),
        ] {
            assert_eq!(Requirement::parse(text), Some(parsed), "{text}");
            assert_eq!(Requirement::parse(text).unwrap().to_string(), text);
        }
        for nonsense in [
            "swe",
            "#4545",
            "swe#abc",
            "origin:acme#12",
            "origin:acme/swe",
            "origin:acme/swe#x",
            "origin:a/b/c#1",
            "origin:acme/swe:",
            "origin:acme/swe:../x",
            "origin:acme/swe:a,b",
            "origin:/swe:main",
        ] {
            assert_eq!(Requirement::parse(nonsense), None, "{nonsense}");
        }
    }

    #[test]
    fn a_branch_name_from_the_command_line_must_name_a_bookmark() {
        use super::BranchName;
        // Given: names a shell can hand over that are not bookmarks. Before the
        // workspace identity check, `finish ..` flattened to the checkout's
        // grandparent and `finish ''` to its parent — and removed them.
        for nonsense in ["", ".", "..", "-r", "feat/../x", "a//b", "feat/.hidden/x"] {
            assert!(
                BranchName::parse(nonsense).is_err(),
                "{nonsense:?} was accepted"
            );
        }
        for name in ["main", "feat/alpha", "release/2026-08-15.1", "fix-1", "a.b"] {
            assert_eq!(
                BranchName::parse(name).map(|branch| branch.as_str().to_owned()),
                Ok(name.to_owned())
            );
        }
    }
    #[test]
    fn a_fetched_pull_request_head_names_its_number() {
        use super::pull_number_from_bookmark;
        assert_eq!(pull_number_from_bookmark("pr-4671"), Some(4671));
        assert_eq!(pull_number_from_bookmark("pr-"), None);
        assert_eq!(pull_number_from_bookmark("pr-abc"), None);
        // A real branch that merely starts with the same letters is not a head.
        assert_eq!(pull_number_from_bookmark("pr-fix/thing"), None);
        assert_eq!(pull_number_from_bookmark("feat/alpha"), None);
    }

    #[test]
    fn short_id_takes_twelve_characters_and_never_splits_one() {
        use super::{CommitId, short_id};
        // Given: a full commit id, an already-short one, and text a notch
        // anchor could carry after sanitising: a multibyte character at the cut,
        // which a byte slice would split.
        assert_eq!(short_id("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short_id("0123456789ab"), "0123456789ab");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
        assert_eq!(short_id("0123456789a\u{fffd}bc"), "0123456789a\u{fffd}");
        // Then: a typed id shortens the same way, without allocating.
        assert_eq!(
            CommitId::new("0123456789abcdef0123").short(),
            "0123456789ab"
        );
    }

    /// The key of `name`, which must be one of `scheme`'s dated cuts.
    fn key<'a>(scheme: &ReleaseScheme, name: &'a str) -> CutKey<'a> {
        scheme
            .cut_key(name)
            .unwrap_or_else(|| panic!("{name} has no key"))
    }

    #[test]
    fn only_our_dated_shape_parses_as_a_dated_release() {
        let dated = ReleaseScheme::Dated;
        // Ours: a date, then nothing, a same-day counter, or a label.
        assert_eq!(
            dated.cut_key("release/2026-08-05"),
            Some(CutKey {
                date: "2026-08-05",
                rest: ""
            })
        );
        assert_eq!(
            dated.cut_key("release/2026-08-05.2"),
            Some(CutKey {
                date: "2026-08-05",
                rest: ".2"
            })
        );
        assert_eq!(
            dated.cut_key("release/2026-08-05-speed-8.0.9"),
            Some(CutKey {
                date: "2026-08-05",
                rest: "-speed-8.0.9"
            })
        );
        for name in [
            "release/2026-08-05-speed.1",
            "release/2026-08-05-speed-7.1.1.2-baseline",
            "release/2026-08-05-x",
        ] {
            assert!(dated.cut_key(name).is_some(), "{name} has no key");
        }
        // Upstream's semver release branches are NOT ours to reap, and a
        // counter has one part: `.1.2`, a bare `.` and `.x` stay unparsed.
        for name in [
            "release/0.3.190",
            "release/",
            "release/foo",
            "release/2026-8-5",
            "release/2026-08-05.",
            "release/2026-08-05.x",
            "release/2026-08-05.1.2",
            "release/2026-08-05.+1",
            "release/2026-08-05-",
            "release/2026-08-05_x",
            "release/2026-08-05x",
            "feat/2026-08-05",
        ] {
            assert_eq!(dated.cut_key(name), None, "{name} has a key");
        }
        // And: a label git cannot hold in a ref has no key, and says why.
        for name in [
            "release/2026-08-05-x~1",
            "release/2026-08-05-a b",
            "release/2026-08-05-x.lock",
            "release/2026-08-05-a..b",
            "release/2026-08-05-x^",
            "release/2026-08-05-x?",
            "release/2026-08-05-x[1]",
            "release/2026-08-05-x@{1}",
            "release/2026-08-05-x.",
        ] {
            assert_eq!(dated.cut_key(name), None, "{name} has a key");
            assert!(dated.ref_flaw(name).is_some(), "{name} has no flaw");
        }
        assert_eq!(dated.ref_flaw("release/2026-08-05-speed-8.0.9"), None);
        // And: a name the series owns but cannot read is still its own.
        assert!(dated.owns("release/foo"));
    }

    #[test]
    fn each_series_owns_only_its_own_cuts() {
        let dated = ReleaseScheme::Dated;
        let old = ReleaseScheme::Series(SeriesName::new("9.2"));
        // Given: a cut of the default series and two of series 9.2.
        let default_cut = "release/2026-10-20";
        let series_cut = "release/9.2/2026-10-21.1";
        let labelled_cut = "release/9.2/2026-11-15-speed-8.0.9";
        // Then: each series parses its own cuts and never the other's, so a
        // 9.2 cut is never the default series' newest, nor the reverse.
        assert_eq!(
            dated.cut_key(default_cut),
            Some(CutKey {
                date: "2026-10-20",
                rest: ""
            })
        );
        assert_eq!(dated.cut_key(series_cut), None);
        assert_eq!(dated.cut_key(labelled_cut), None);
        assert!(!dated.owns(series_cut));
        assert_eq!(
            old.cut_key(series_cut),
            Some(CutKey {
                date: "2026-10-21",
                rest: ".1"
            })
        );
        assert_eq!(
            old.cut_key(labelled_cut),
            Some(CutKey {
                date: "2026-11-15",
                rest: "-speed-8.0.9"
            })
        );
        assert_eq!(old.cut_key(default_cut), None);
        assert!(!old.owns(default_cut));
        assert!(!old.owns("release/9.20/2026-10-21"));
        assert!(!old.owns("release/9.2/x/2026-10-21"));
        assert!(old.owns("release/9.2/foo") && old.cut_key("release/9.2/foo").is_none());
        // And: both are releases of the fork, so neither is ever a member branch.
        assert!(is_release_name(&BranchName::new(series_cut), &dated));
        assert!(is_release_name(&BranchName::new(default_cut), &old));
        let local = |name: &str| BookmarkRef::Local(BranchName::new(name));
        for name in [default_cut, series_cut, labelled_cut] {
            assert!(is_our_dated_cut(&local(name), "origin"), "{name}");
        }
        for name in [
            "release/0.3.190",
            "release/9.2/0.3.190",
            "release/9.2/foo",
            "release/9.2/2026-10-21.1.2",
            "release/9.2/x/2026-10-21",
        ] {
            assert!(!is_our_dated_cut(&local(name), "origin"), "{name}");
        }
        let upstream = BookmarkRef::Remote {
            branch: BranchName::new(labelled_cut),
            remote: RemoteName::new("upstream"),
        };
        assert!(!is_our_dated_cut(&upstream, "origin"));
        assert_eq!(old.cut_shape(), "release/9.2/YYYY-MM-DD[.N|-<label>]");
    }

    #[test]
    fn cuts_sort_by_date_then_by_the_rest_in_version_order() {
        let old = ReleaseScheme::Series(SeriesName::new("9.2"));
        // Given: names of one series, each meant to sort before the next: a
        // number before text, a prefix first, `-` before `.`, then a later date.
        let ascending = [
            "release/9.2/2026-11-15",
            "release/9.2/2026-11-15-speed-3",
            "release/9.2/2026-11-15-speed-7.1.1.2-baseline",
            "release/9.2/2026-11-15-speed-7.1.9",
            "release/9.2/2026-11-15-speed-7.1.10",
            "release/9.2/2026-11-15-speed-8.0.9",
            "release/9.2/2026-11-15-speed-8.0.10",
            "release/9.2/2026-11-15-speed-8.1",
            "release/9.2/2026-11-15-speed-8.1.1",
            "release/9.2/2026-11-15-speed-sr",
            "release/9.2/2026-11-15-speed.1",
            "release/9.2/2026-11-15-v08",
            "release/9.2/2026-11-15-v8",
            "release/9.2/2026-11-15-v9",
            "release/9.2/2026-11-15-x-0000000000000000000000000000001",
            "release/9.2/2026-11-15-x-99999999999999999999999999999999",
            "release/9.2/2026-11-15-x-100000000000000000000000000000000",
            "release/9.2/2026-11-15.1",
            "release/9.2/2026-11-15.2",
            "release/9.2/2026-11-15.10",
            "release/9.2/2026-11-16",
            "release/9.2/2026-11-16-a",
        ];
        // Then: each pair is in order, and no two names tie.
        for pair in ascending.windows(2) {
            assert!(
                key(&old, pair[0]) < key(&old, pair[1]),
                "{} does not sort before {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_forks_9_2_release_names_sort_by_date_then_version() {
        let old = ReleaseScheme::Series(SeriesName::new("9.2"));
        // Given: every name series 9.2 of the prover fork had on 2026-11-17,
        // read with `git ls-remote`: the 59 its release remote holds and the
        // 9 only its origin holds (8.0 and 8.0.1 to 8.0.8), in the order the
        // rules give.
        let expected = [
            "release/9.2/2026-10-21",
            "release/9.2/2026-10-22",
            "release/9.2/2026-10-23",
            "release/9.2/2026-10-24",
            "release/9.2/2026-10-24.1",
            "release/9.2/2026-10-25",
            "release/9.2/2026-10-25.1",
            "release/9.2/2026-10-25.2",
            "release/9.2/2026-10-25.3",
            "release/9.2/2026-10-25.4",
            "release/9.2/2026-10-25.5",
            "release/9.2/2026-10-25.6",
            "release/9.2/2026-10-25.7",
            "release/9.2/2026-10-26-guard-candidate",
            "release/9.2/2026-10-26-speed",
            "release/9.2/2026-10-26-speed-3",
            "release/9.2/2026-10-26-speed-4",
            "release/9.2/2026-10-26-speed-sr",
            "release/9.2/2026-10-26-speed.1",
            "release/9.2/2026-10-26-speed.2",
            "release/9.2/2026-10-27-guard-let-candidate",
            "release/9.2/2026-10-27-speed-5",
            "release/9.2/2026-10-27-speed-5.1",
            "release/9.2/2026-10-27-speed-5.2",
            "release/9.2/2026-10-29-speed-6",
            "release/9.2/2026-10-30-speed-7",
            "release/9.2/2026-10-31-speed-7.1",
            "release/9.2/2026-10-31-speed-7.2",
            "release/9.2/2026-11-02-speed-7.3",
            "release/9.2/2026-11-02-speed-7.4",
            "release/9.2/2026-11-02-speed-7.5",
            "release/9.2/2026-11-02-speed-7.6",
            "release/9.2/2026-11-02-speed-7.7",
            "release/9.2/2026-11-03-label-v4",
            "release/9.2/2026-11-03-label-v4.1",
            "release/9.2/2026-11-03-speed-7.8",
            "release/9.2/2026-11-03-v-label-v4.2",
            "release/9.2/2026-11-03-v-label-v4.3",
            "release/9.2/2026-11-04-speed-7.1.1",
            "release/9.2/2026-11-04-speed-7.1.2",
            "release/9.2/2026-11-04-speed-7.5.1",
            "release/9.2/2026-11-04-speed-7.9",
            "release/9.2/2026-11-04-speed-7.9.1",
            "release/9.2/2026-11-04-v-label-v4.4",
            "release/9.2/2026-11-04-v-label-v4.5",
            "release/9.2/2026-11-05-speed-7.9.2",
            "release/9.2/2026-11-08-speed-7.9.3",
            "release/9.2/2026-11-09-speed-7.1.3",
            "release/9.2/2026-11-10-speed-7.1.1.1",
            "release/9.2/2026-11-10-speed-7.1.4",
            "release/9.2/2026-11-10-speed-7.1.5",
            "release/9.2/2026-11-10-speed-7.1.6",
            "release/9.2/2026-11-10-speed-7.1.7",
            "release/9.2/2026-11-10-speed-7.1.8",
            "release/9.2/2026-11-11-speed-7.1.1.2-baseline",
            "release/9.2/2026-11-11-speed-7.1.9",
            "release/9.2/2026-11-11-speed-7.1.10",
            "release/9.2/2026-11-12-speed-7.1.11",
            "release/9.2/2026-11-12-speed-8.0",
            "release/9.2/2026-11-13-speed-8.0.1",
            "release/9.2/2026-11-13-speed-8.0.2",
            "release/9.2/2026-11-13-speed-8.0.3",
            "release/9.2/2026-11-13-speed-8.0.4",
            "release/9.2/2026-11-14-speed-8.0.5",
            "release/9.2/2026-11-14-speed-8.0.6",
            "release/9.2/2026-11-14-speed-8.0.7",
            "release/9.2/2026-11-14-speed-8.0.8",
            "release/9.2/2026-11-15-speed-8.0.9",
        ];
        let in_order = |mut names: Vec<&'static str>| {
            names.sort_by_key(|&name| key(&old, name));
            names
        };

        // When: they are sorted from `git ls-remote`'s byte order, and from
        // the reverse of the expected order.
        let mut bytewise = expected.to_vec();
        bytewise.sort_unstable();
        let mut reversed = expected.to_vec();
        reversed.reverse();

        // Then: both give the expected order, and no name is the default
        // series'.
        assert_eq!(expected.len(), 68);
        assert_eq!(in_order(bytewise), expected);
        assert_eq!(in_order(reversed), expected);
        assert!(
            expected
                .iter()
                .all(|name| ReleaseScheme::Dated.cut_key(name).is_none())
        );
    }

    use super::*;

    #[test]
    fn a_local_and_a_remote_bookmark_of_one_branch_are_different_values() {
        // Given: one branch name reachable two ways
        let branch = BranchName::new("feat/alpha");
        // When: it is expressed as a local ref and as a remote-tracking ref
        let local = BookmarkRef::Local(branch.clone());
        let remote = BookmarkRef::Remote {
            branch,
            remote: RemoteName::new("origin"),
        };
        // Then: they are distinct, so a map cannot silently collapse them
        assert_ne!(local, remote);
        assert_eq!(local.branch(), remote.branch());
        assert!(local.is_local());
        assert!(!remote.is_local());
    }

    #[test]
    fn a_remote_bookmark_displays_with_its_remote() {
        let reference = BookmarkRef::Remote {
            branch: BranchName::new("feat/alpha"),
            remote: RemoteName::new("origin"),
        };
        assert_eq!(reference.to_string(), "feat/alpha@origin");
    }

    #[test]
    fn under_a_fixed_scheme_the_fixed_branch_is_the_release_and_dated_names_are_not() {
        use super::{BookmarkRef, BranchName, ReleaseScheme, RemoteName, is_our_release};
        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let local = |name: &str| BookmarkRef::Local(BranchName::new(name));
        let remote = |name: &str, r: &str| BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(r),
        };
        // Local releases are ours; tracked refs are ours only on the role that
        // publishes them.
        assert!(is_our_release(&local("integration"), &fixed, "release"));
        assert!(is_our_release(
            &remote("integration", "origin"),
            &fixed,
            "origin"
        ));
        assert!(is_our_release(
            &remote("integration", "release"),
            &fixed,
            "release"
        ));
        assert!(!is_our_release(
            &remote("integration", "origin"),
            &fixed,
            "release"
        ));
        assert!(!is_our_release(
            &remote("integration", "upstream"),
            &fixed,
            "origin"
        ));
        assert!(!is_our_release(
            &remote("integration", "git"),
            &fixed,
            "origin"
        ));
        // Under Fixed, a dated name is not one of this repo's releases.
        assert!(!is_our_release(
            &local("release/2026-07-29"),
            &fixed,
            "origin"
        ));
        // Dated behavior is unchanged.
        assert!(is_our_release(
            &local("release/2026-07-29"),
            &ReleaseScheme::Dated,
            "origin"
        ));
        assert!(!is_our_release(
            &local("integration"),
            &ReleaseScheme::Dated,
            "origin"
        ));
    }
}
