//! The canonical spellings `knives gh` compares, and nothing else.
//!
//! Seven review rounds found one defect under different spellings: knives
//! reproducing how gh, go-gh and GitHub normalise a repository spec, a head
//! or an endpoint — a percent-escape in a URL, an scp shorthand, a `www.`
//! host, an empty `-R`, a `gh-resolved` marker carrying a host, `--help=false`
//! — and getting one corner wrong, which is a pull request on the upstream
//! for a branch never ruled UPSTREAM. This module ends the mimicry. knives
//! compares a spelling it can read byte for byte, and refuses every other
//! spelling toward a registered upstream with a remedy that names the
//! canonical one. An over-refusal costs a retype; a hole costs the rule.
//!
//! The grammar: a repository is `OWNER/REPO` or `HOST/OWNER/REPO`, every
//! segment of [`SEGMENT_CHARS`] (GitHub's own charset for a login or a
//! repository name; a host is spelled the same way); a head is
//! `OWNER:BRANCH` with `OWNER` the registered fork's own origin owner and
//! `BRANCH` of [`BRANCH_CHARS`] not beginning with `-` — a bare branch is
//! the base repository's own to gh, and is refused toward an upstream; a
//! switch's `=value` is one of Go's `strconv.ParseBool` spellings. A spelling outside
//! the grammar is [`Err`] carrying the refusal, never a guess.

/// The characters one repository segment — a host, an owner, a name — may carry.
pub const SEGMENT_CHARS: &str = "A-Za-z0-9._-";

/// The characters a branch name may carry: a segment's, plus `/`.
pub const BRANCH_CHARS: &str = "A-Za-z0-9._/-";

/// Whether `text` is one canonical segment.
///
/// Non-empty, every character of [`SEGMENT_CHARS`], not the path steps `.`
/// and `..`, and not ending in `.git` in any case — GitHub names none of
/// those, and every one is a spelling some client rewrites on the way to
/// the server.
pub fn is_segment(text: &str) -> bool {
    !text.is_empty()
        && text != "."
        && text != ".."
        && !text
            .get(text.len().saturating_sub(4)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".git"))
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A repository named in canonical form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// `OWNER/REPO` on `default_host`, or `HOST/OWNER/REPO`; `None` for
    /// anything else — a URL of any scheme, an scp shorthand, a `.git`
    /// suffix, a trailing `/`, a percent-escape, an empty segment, a fourth
    /// part.
    pub fn parse(spec: &str, default_host: &str) -> Option<Self> {
        let mut parts = spec.split('/');
        let (first, second, third, fourth) =
            (parts.next(), parts.next(), parts.next(), parts.next());
        let (host, owner, name) = match (first, second, third, fourth) {
            (Some(owner), Some(name), None, None) => (default_host, owner, name),
            (Some(host), Some(owner), Some(name), None) => (host, owner, name),
            _ => return None,
        };
        ([host, owner, name].into_iter().all(is_segment)).then(|| Self {
            host: host.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// `OWNER/REPO` exactly, on the given `host` — the shape a `gh repo
    /// set-default` marker takes, whose host is always the marked remote's.
    pub fn parse_on(spec: &str, host: &str) -> Option<Self> {
        let (owner, name) = spec.split_once('/')?;
        ([host, owner, name].into_iter().all(is_segment)).then(|| Self {
            host: host.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The https URL the registry is compared by (`remote_url::same_remote`).
    pub fn url(&self) -> String {
        format!("https://{}/{}/{}.git", self.host, self.owner, self.name)
    }
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.owner, self.name)
    }
}

/// The refusal for a repository spelled outside the grammar, naming where
/// it was stated (`-R`, `GH_REPO`, `remote.<name>.gh-resolved`) and the
/// canonical form.
pub fn repo_refusal(source: &str, text: &str) -> String {
    format!(
        "knives compares repositories only as OWNER/REPO or HOST/OWNER/REPO (each segment of \
         [{SEGMENT_CHARS}]); {source} {text:?} is not one: state it that way"
    )
}

/// Whether `text` is a branch name knives compares: non-empty, every
/// character of [`BRANCH_CHARS`], not beginning with `-`.
pub fn is_branch(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

/// The refusal for a head whose branch is spelled outside the grammar: not
/// a branch name knives compares (the owner, if any, is judged by the
/// caller).
pub fn head_refusal(repo: &str, spelling: &str, text: &str) -> String {
    format!(
        "knives compares heads only as OWNER:BRANCH with the branch of [{BRANCH_CHARS}], not \
         beginning with -; {spelling} {text:?} for {repo} is not one: state the branch"
    )
}

/// Go's `strconv.ParseBool`, the reading pflag gives a switch's `=value`.
pub fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// The spellings [`parse_bool`] reads, for a remedy.
pub const BOOL_SPELLINGS: &str = "1 0 t f T F TRUE FALSE True False true false";

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;

    const HOST: &str = concat!("github", ".com");

    #[test]
    fn a_repository_is_two_or_three_canonical_segments_and_nothing_else() {
        let repo = |host: &str, owner: &str, name: &str| Repo {
            host: host.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        };
        assert_eq!(
            Repo::parse("acme/work", HOST),
            Some(repo(HOST, "acme", "work"))
        );
        assert_eq!(
            Repo::parse("forge.example/acme/work", HOST),
            Some(repo("forge.example", "acme", "work"))
        );
        assert_eq!(
            Repo::parse("Acme_1/wo.rk-2", HOST),
            Some(repo(HOST, "Acme_1", "wo.rk-2"))
        );
        assert_eq!(
            repo(HOST, "acme", "work").url(),
            format!("https://{HOST}/acme/work.git")
        );
        for spec in [
            "",
            "acme",
            "acme/",
            "/acme/work",
            "acme//work",
            "acme/work/",
            "a/b/c/d",
            "acme/work.git",
            "acme/work.GIT",
            "acme/.",
            "../work",
            &format!("https://{HOST}/acme/work"),
            &format!("HTTPS://{HOST}/acme/work"),
            &format!("git@{HOST}:acme/work"),
            &format!("ssh://git@{HOST}/acme/work"),
            &format!("{HOST}/acme/wo%72k"),
            "acme/wo%72k",
            "acme/work?x=1",
            "acme/work#frag",
            "acme:work",
            "acme/wo rk",
            "acme/wörk",
            "acme@x/work",
            "-",
            "./work",
        ] {
            assert_eq!(Repo::parse(spec, HOST), None, "{spec:?}");
        }
    }

    #[test]
    fn a_marker_value_is_owner_and_name_on_the_remotes_host() {
        assert_eq!(
            Repo::parse_on("acme/work", "forge.example"),
            Some(Repo {
                host: "forge.example".to_owned(),
                owner: "acme".to_owned(),
                name: "work".to_owned(),
            })
        );
        for (spec, host) in [
            ("forge.example/acme/work", HOST),
            ("acme", HOST),
            ("acme/", HOST),
            ("", HOST),
            ("acme/work", ""),
            ("acme/work", "forge example"),
        ] {
            assert_eq!(Repo::parse_on(spec, host), None, "{spec:?} on {host:?}");
        }
    }

    #[test]
    fn a_branch_is_the_bookmark_charset_without_a_colon_or_a_leading_dash() {
        for branch in ["feat/x", "main", "a.b_c-d/e", "v1.2.3"] {
            assert!(is_branch(branch), "{branch}");
        }
        for text in [
            "", "-x", "o:feat/x", "feat x", "feat/x?", "feat/ö", "feat/x\n", "@{-1}", "feat\\x",
        ] {
            assert!(!is_branch(text), "{text:?}");
        }
    }

    #[test]
    fn a_switch_value_is_read_as_go_reads_a_bool() {
        for text in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(text), Some(true), "{text}");
        }
        for text in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(text), Some(false), "{text}");
        }
        for text in ["", "yes", "no", "tRUE", "Yes", "on", "2", "-1", "true "] {
            assert_eq!(parse_bool(text), None, "{text:?}");
        }
    }

    /// A small deterministic generator (xorshift64*), so the corpus below is
    /// the same on every run and a failure names a reproducible string.
    struct Generator(u64);

    impl Generator {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, bound: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(bound).unwrap_or(1)).unwrap_or(0)
        }

        fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
            from[self.below(from.len())]
        }
    }

    /// Every spelling the corpus is built from: canonical segments, the
    /// separators and escapes seven rounds of review reached for, and text
    /// outside ASCII.
    const PIECES: &[&str] = &[
        "acme",
        "work",
        "a",
        "Z9",
        "x.y",
        "_-",
        "github.com",
        "GitHub.COM",
        "/",
        "/",
        "/",
        "//",
        "%",
        "%2f",
        "%75",
        ":",
        "@",
        "?",
        "#",
        ".git",
        "https://",
        "HTTPS://",
        "ssh://",
        "git@",
        "www.",
        "",
        "",
        " ",
        "ö",
        "\u{200b}",
        "-",
        "..",
        ".",
        ".GIT",
        "{owner}",
        ":owner",
        "\\",
        "\n",
    ];

    #[test]
    fn the_repository_parser_never_normalises_what_it_reads() {
        // A smoke test of the parser over 400 generated strings plus the
        // pieces themselves: whatever it reads respells to the input byte
        // for byte and round-trips, and whatever it refuses is refused with
        // the canonical form named. It says nothing about gh or GitHub —
        // which spellings gh would send to a registered upstream is the
        // integration suite's question (`tests/gh_command.rs`).
        let mut generator = Generator(0x9E37_79B9_7F4A_7C15);
        let mut corpus: Vec<String> = PIECES.iter().map(|piece| (*piece).to_owned()).collect();
        for _ in 0..400 {
            let length = 1 + generator.below(6);
            corpus.push((0..length).map(|_| generator.pick(PIECES)).collect());
        }
        let mut read = 0;
        for spec in &corpus {
            let Some(repo) = Repo::parse(spec, HOST) else {
                // Refused: and the refusal names the canonical form.
                let refusal = repo_refusal("-R", spec);
                assert!(
                    refusal.contains("OWNER/REPO or HOST/OWNER/REPO"),
                    "{refusal}"
                );
                assert!(refusal.contains("state it that way"), "{refusal}");
                continue;
            };
            read += 1;
            assert!(is_segment(&repo.host), "{spec:?} -> {repo}");
            assert!(is_segment(&repo.owner), "{spec:?} -> {repo}");
            assert!(is_segment(&repo.name), "{spec:?} -> {repo}");
            // The spelling is its own reading: two parts on the default
            // host, or three parts verbatim.
            let respelled = if spec.matches('/').count() == 1 {
                format!("{}/{}", repo.owner, repo.name)
            } else {
                repo.to_string()
            };
            assert_eq!(&respelled, spec, "{spec:?} was normalised to {repo}");
            assert_eq!(Repo::parse(&repo.to_string(), HOST), Some(repo.clone()));
            assert_eq!(
                Repo::parse_on(&format!("{}/{}", repo.owner, repo.name), &repo.host),
                Some(repo)
            );
        }
        assert!(read > 0, "the corpus never produced a canonical spelling");
    }

    #[test]
    fn every_over_refusal_names_the_canonical_spelling() {
        let repo = repo_refusal("GH_REPO", &format!("https://{HOST}/acme/work"));
        assert!(repo.contains("OWNER/REPO or HOST/OWNER/REPO"), "{repo}");
        assert!(
            repo.contains(&format!("GH_REPO \"https://{HOST}/acme/work\"")),
            "{repo}"
        );
        let shape = head_refusal("registered", "-f head=", "o:feat x");
        assert!(shape.contains("only as OWNER:BRANCH"), "{shape}");
        assert!(shape.contains("state the branch"), "{shape}");
        assert!(shape.contains(BRANCH_CHARS), "{shape}");
    }
}
