//! Semantic identifiers.
//!
//! Each of these is a distinct type rather than a `String` because mixing them
//! is a real bug class in this domain, not a hypothetical one. A change id and a
//! commit id look identical, are both short hex-ish strings, and mean entirely
//! different things: one change can exist as several commits, which is exactly
//! what divergence is. Likewise a local bookmark and its remote-tracking
//! counterpart share a name but point at different commits.

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
    /// Dated `release/YYYY-MM-DD[.n]` cuts of the entry's default series. The
    /// default, and the historical behavior.
    Dated,
    /// Dated `release/<series>/YYYY-MM-DD[.n]` cuts of a named series, which
    /// has its own base. Every dated rule applies; only the names differ.
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

    /// The ordering key of one of this series' dated cuts, numeric suffixes
    /// compared numerically. Lenient: any name the series owns has one.
    /// `None` for a name the series does not own, and always under `Fixed`.
    pub fn cut_order(&self, name: &str) -> Option<(String, u32)> {
        let rest = self.dated_suffix(name)?;
        Some(match rest.split_once('.') {
            Some((date, suffix)) => (date.to_owned(), suffix.parse().unwrap_or(0)),
            None => (rest.to_owned(), 0),
        })
    }

    /// Parse `release/[<series>/]YYYY-MM-DD[.N]`, the one shape this series'
    /// dated cuts take.
    ///
    /// Stricter than [`Self::cut_order`] on purpose: the reaper enumerates
    /// release refs on any remote, where upstream's own `release/0.3.190` style
    /// branches also live, and a prefix test would hand those to `bookmark
    /// forget`. Returns the `(date, suffix)` ordering key so "newest" is one
    /// `max_by_key` away. The suffix follows `u32::from_str` semantics rather
    /// than strict digits, so leading `+` signs and zeroes parse; every such
    /// name is still ours by shape, leaving the reap safety property unaffected.
    pub fn dated_cut(&self, name: &str) -> Option<(String, u32)> {
        let bare = self.dated_suffix(name)?;
        let (date, suffix) = match bare.split_once('.') {
            Some((date, suffix)) => (date, suffix.parse::<u32>().ok()?),
            None => (bare, 0),
        };
        let bytes = date.as_bytes();
        let dated = bytes.len() == 10
            && bytes.get(4) == Some(&b'-')
            && bytes.get(7) == Some(&b'-')
            && date
                .bytes()
                .enumerate()
                .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit());
        dated.then(|| (date.to_owned(), suffix))
    }

    /// The name shape this scheme's cuts take, for a message.
    pub fn cut_shape(&self) -> String {
        match self {
            Self::Dated => format!("{RELEASE_PREFIX}YYYY-MM-DD[.N]"),
            Self::Series(series) => format!("{RELEASE_PREFIX}{series}/YYYY-MM-DD[.N]"),
            Self::Fixed(fixed) => fixed.to_string(),
        }
    }
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

/// Whether `name` is a dated cut of any series of a fork: the default series'
/// `release/YYYY-MM-DD[.N]` or a named series' `release/<series>/YYYY-MM-DD[.N]`.
/// Every series' cut is a release commit, never an orphan of another's.
pub fn any_dated_cut(name: &str) -> bool {
    ReleaseScheme::Dated.dated_cut(name).is_some()
        || name
            .strip_prefix(RELEASE_PREFIX)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(series, _)| {
                ReleaseScheme::Series(SeriesName::new(series))
                    .dated_cut(name)
                    .is_some()
            })
}

/// Whether `reference` is one of our dated cuts, of any series: [`any_dated_cut`]
/// behind the trust boundary [`is_our_release`] draws.
pub fn is_our_dated_cut(reference: &BookmarkRef, publish_remote: &str) -> bool {
    any_dated_cut(reference.branch().as_str())
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

/// Something a branch cannot land before: a pull request in some managed repo.
///
/// Written `<repo>#<number>`, where `<repo>` is the required fork's
/// [`UpstreamName`]. The repo is named because dependencies cross forks,
/// which is the case that motivated this: a change in one fork needing a pull request
/// in a sibling, where dropping one from a release without the other ships a release
/// that cannot work.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Requirement {
    pub repo: UpstreamName,
    pub number: u64,
}

impl Requirement {
    /// `<repo>#<number>` as written. The repo is split off at the last `#`,
    /// and is whatever was written there; `knives depends` resolves a
    /// registry key to the fork's upstream name before recording it.
    pub fn parse(text: &str) -> Option<Self> {
        let (repo, number) = text.rsplit_once('#')?;
        if repo.is_empty() {
            return None;
        }
        Some(Self {
            repo: UpstreamName::new(repo),
            number: number.parse().ok()?,
        })
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
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
        use super::Requirement;
        let parsed = Requirement::parse("swe#4545").unwrap();
        assert_eq!(parsed.repo.as_str(), "swe");
        assert_eq!(parsed.number, 4545);
        assert_eq!(parsed.to_string(), "swe#4545");
        assert!(Requirement::parse("swe").is_none());
        assert!(Requirement::parse("#4545").is_none());
        assert!(Requirement::parse("swe#abc").is_none());
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

    #[test]
    fn only_our_dated_shape_parses_as_a_dated_release() {
        let dated = ReleaseScheme::Dated;
        // Ours, with and without a same-day suffix.
        assert_eq!(
            dated.dated_cut("release/2026-08-05"),
            Some(("2026-08-05".to_owned(), 0))
        );
        assert_eq!(
            dated.dated_cut("release/2026-08-05.2"),
            Some(("2026-08-05".to_owned(), 2))
        );
        // Upstream's semver release branches are NOT ours to reap.
        assert_eq!(dated.dated_cut("release/0.3.190"), None);
        // Shape violations.
        assert_eq!(dated.dated_cut("release/"), None);
        assert_eq!(dated.dated_cut("release/2026-8-5"), None);
        assert_eq!(dated.dated_cut("release/2026-08-05."), None);
        assert_eq!(dated.dated_cut("release/2026-08-05.x"), None);
        assert_eq!(dated.dated_cut("release/2026-08-05.1.2"), None);
        assert_eq!(dated.dated_cut("feat/2026-08-05"), None);
    }

    #[test]
    fn each_series_owns_only_its_own_cuts() {
        let dated = ReleaseScheme::Dated;
        let old = ReleaseScheme::Series(SeriesName::new("9.2"));
        // Given: a cut of the default series and one of series 9.2.
        let default_cut = "release/2026-10-20";
        let series_cut = "release/9.2/2026-10-21.1";
        // Then: each series parses its own cut and never the other's, so a
        // 9.2 cut is never the default series' newest, nor the reverse.
        assert_eq!(
            dated.dated_cut(default_cut),
            Some(("2026-10-20".to_owned(), 0))
        );
        assert_eq!(dated.dated_cut(series_cut), None);
        assert_eq!(dated.cut_order(series_cut), None);
        assert!(!dated.owns(series_cut));
        assert_eq!(
            old.dated_cut(series_cut),
            Some(("2026-10-21".to_owned(), 1))
        );
        assert_eq!(old.dated_cut(default_cut), None);
        assert!(!old.owns(default_cut));
        assert!(!old.owns("release/9.20/2026-10-21"));
        assert!(!old.owns("release/9.2/x/2026-10-21"));
        // And: both are releases of the fork, so neither is ever a member branch.
        assert!(is_release_name(&BranchName::new(series_cut), &dated));
        assert!(is_release_name(&BranchName::new(default_cut), &old));
        assert!(any_dated_cut(default_cut) && any_dated_cut(series_cut));
        assert!(!any_dated_cut("release/0.3.190") && !any_dated_cut("release/9.2/0.3.190"));
        assert_eq!(old.cut_shape(), "release/9.2/YYYY-MM-DD[.N]");
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
