//! gh's configuration files, read in the grammar gh writes them in, or not
//! at all.
//!
//! The default host `knives gh` certifies a creation against comes, when
//! nothing states one, from `config.yml`'s `hosts:` block or `hosts.yml` —
//! files gh writes through a YAML library and a user may hand-edit. Three
//! review rounds found knives reading a shape gh reads differently: a
//! heading spelled `"ho\x73ts":`, a flow document, a second document, a
//! no-break space before a key, a tab indent gh rejects outright. Each was
//! a partial reading of YAML that a fuller one would have answered another
//! way. This module reads none of YAML: it reads the grammar gh writes —
//! block mappings of `key:`, `key: scalar` and `key: {}` lines (the empty
//! map gh writes for a user with no stored token), keys of one canonical
//! charset behind at most one layer of quotes, space indentation, `#`
//! comments, blank lines, one document, or the one-line document `{}` gh
//! leaves after the last `gh auth logout` — and the first line outside that
//! grammar, in either file, makes the default host unknown, which refuses
//! every creation that needs it with the remedy to state the host. There is
//! no reading past such a line and no falling through from one file to the
//! other: a `config.yml` that is refused is refused, whatever `hosts.yml`
//! says. `GH_HOST`, `--hostname` and a host in the command bypass the files.
//!
//! The cost is accepted by design: a hand-edited file YAML and gh read
//! without complaint may be refused here. The refusal names the file, the
//! line, and the remedy, and the file gh writes is never refused.

use std::path::{Path, PathBuf};

/// Whether `key` is spelled in the characters a key knives reads may carry
/// (`[A-Za-z0-9._:-]`): a host with its port, a setting name, an alias —
/// nothing YAML gives another meaning to, and nothing a quote layer decodes.
fn is_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// YAML's separation whitespace: ASCII space and tab, nothing else. Rust's
/// `trim` would also strip a no-break space or a Unicode space, which YAML
/// reads as content — a key gh does not read must not become one knives
/// reads.
const SPACE: [char; 2] = [' ', '\t'];

/// The characters that begin something other than a plain scalar in YAML:
/// a collection, an anchor, an alias, a tag, a block scalar, a comment, a
/// directive, a reserved indicator. A value beginning with one is refused.
const INDICATORS: &str = "-?:,[]{}#&*!|>'\"%@`";

/// gh's configuration directory, by go-gh's `ConfigDir` precedence
/// (`pkg/config/config.go`): `GH_CONFIG_DIR`, `$XDG_CONFIG_HOME/gh`,
/// `$HOME/.config/gh` (the Windows `AppData` step has no Unix reading).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GH_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(xdg).join("gh"));
    }
    std::env::home_dir().map(|home| home.join(".config").join("gh"))
}

/// The hosts gh is configured for, in file order, with the file they came from.
///
/// `config.yml`'s `hosts:` block when the file has a top-level `hosts` key
/// (the pre-multi-account layout gh still reads and keeps ahead of the
/// other file, `config.go` `load`), else `hosts.yml`'s top-level keys; none
/// when neither file exists or has any. Either file outside the grammar
/// ([`parse`]), a `hosts` heading or a host key carrying a value, or a file
/// that cannot be read, is the refusal — never a guess at what gh would do
/// with it, and never the other file.
pub fn configured_hosts() -> Result<(&'static str, Vec<String>), String> {
    let Some(dir) = config_dir() else {
        return Ok(("hosts.yml", Vec::new()));
    };
    let general = dir.join("config.yml");
    let entries = read(&general)?;
    if let Some(at) = entries
        .iter()
        .position(|entry| entry.depth == 0 && entry.key == "hosts")
    {
        let hosts = entries
            .get(at + 1..)
            .unwrap_or_default()
            .iter()
            .take_while(|entry| entry.depth > 0)
            .filter(|entry| entry.depth == 1);
        return host_keys(&general, entries.get(at).into_iter().chain(hosts))
            .map(|hosts| ("config.yml", hosts));
    }
    let hosts = dir.join("hosts.yml");
    let entries = read(&hosts)?;
    host_keys(&hosts, entries.iter().filter(|entry| entry.depth == 0))
        .map(|hosts| ("hosts.yml", hosts))
}

/// The keys of `entries` — the `hosts` heading and its hosts, or `hosts.yml`'s
/// top-level keys — each of which must open a map: one carrying a scalar
/// is refused naming `path`. The heading itself is not returned.
fn host_keys<'a>(
    path: &Path,
    entries: impl Iterator<Item = &'a Entry>,
) -> Result<Vec<String>, String> {
    let mut hosts = Vec::new();
    for entry in entries {
        if entry.value.is_some() {
            return Err(refusal(path, entry.line, &entry.text));
        }
        if !(entry.depth == 0 && entry.key == "hosts") || path.ends_with("hosts.yml") {
            hosts.push(entry.key.clone());
        }
    }
    Ok(hosts)
}

/// `path` parsed by the grammar; no entries when the file does not exist;
/// the refusal when it cannot be read or a line is outside the grammar.
fn read(path: &Path) -> Result<Vec<Entry>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "gh's {} cannot be read ({error}), so the default host cannot be told: state the \
                 host in the command (-R HOST/OWNER/REPO, --hostname) or set GH_HOST",
                path.display()
            ));
        }
    };
    parse(&text).map_err(|(line, text)| refusal(path, line, &text))
}

fn refusal(path: &Path, line: usize, text: &str) -> String {
    format!(
        "gh's {} is not YAML knives reads (line {line}: {text:?}), so the default host cannot be \
         told: state the host in the command (-R HOST/OWNER/REPO, --hostname) or set GH_HOST",
        path.display()
    )
}

/// One `key:` or `key: scalar` line of a file in the grammar.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// 1-based line in the file.
    pub line: usize,
    /// The line as written, less trailing whitespace.
    pub text: String,
    /// Nesting: 0 for the outermost map, one more for each map opened by a
    /// `key:` line above.
    pub depth: usize,
    /// The key, its one quote layer removed.
    pub key: String,
    /// The scalar after the colon, quotes and trailing comment removed;
    /// `None` when the line opens a map, or holds the empty map `{}`.
    pub value: Option<String>,
}

/// What follows an entry's colon.
enum Rest<'a> {
    /// Nothing: the entry opens a map on the deeper lines below (or is null).
    Opens,
    /// `{}`: the empty map gh writes; nothing may be deeper below it.
    Empty,
    Scalar(&'a str),
}

/// `text` as the block mappings gh writes, or the first line outside the
/// grammar (1-based, trailing whitespace trimmed).
///
/// The grammar: an optional byte-order mark; lines that are blank, a `#`
/// comment, or an entry `INDENT KEY: [SCALAR] [# comment]`; INDENT of
/// spaces only (YAML forbids tabs there, and gh errors on them), the first
/// entry's at any depth and every later one at the depth of an open map —
/// deeper only directly under a `key:` with no scalar, shallower only to a
/// depth already open; KEY of [`is_key`]'s charset, behind one layer of matching
/// `"` or `'` whose content is read verbatim (a space inside the quotes is
/// outside the charset, not trimmed), separation whitespace allowed before
/// the colon, unique within its map; SCALAR plain (no
/// leading [`INDICATORS`], no `: ` inside, no trailing `:`), in one layer
/// of quotes, or the empty map `{}`, on the one line, followed by nothing
/// but a comment. Every
/// other line — a document marker, a flow collection, a list item, a
/// complex key, an anchor, an alias, a tag, a block scalar, a bare scalar,
/// a key or value with a character YAML or a quote layer would decode, a
/// control byte — is the error.
pub fn parse(text: &str) -> Result<Vec<Entry>, (usize, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // The empty map gh writes as a whole file (measured: `gh auth logout` of
    // the only account leaves hosts.yml as exactly `{}` and a newline): no
    // entries. Only that document; `{}` among entries stays outside.
    if text.trim_end_matches(['\n', '\r']) == "{}" {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    // The open maps: each level's indent and the keys seen in it.
    let mut levels: Vec<(usize, Vec<String>)> = Vec::new();
    let mut opened = false;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim_end_matches(SPACE);
        let content = line.trim_start_matches(SPACE);
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let refused = || Err((number, line.to_owned()));
        let indent = line.len() - content.len();
        if line.get(..indent).is_some_and(|lead| lead.contains('\t'))
            || content.chars().any(|c| c.is_control() && c != '\t')
        {
            return refused();
        }
        let Some((key, rest)) = entry(content) else {
            return refused();
        };
        match levels.last().map(|(at, _)| *at) {
            None => levels.push((indent, Vec::new())),
            Some(at) if indent > at => {
                if !opened {
                    return refused();
                }
                levels.push((indent, Vec::new()));
            }
            Some(at) if indent < at => {
                while levels.last().is_some_and(|(at, _)| *at > indent) {
                    levels.pop();
                }
                if levels.last().is_none_or(|(at, _)| *at != indent) {
                    return refused();
                }
            }
            Some(_) => {}
        }
        let Some((_, keys)) = levels.last_mut() else {
            return refused();
        };
        if keys.iter().any(|seen| seen == key) {
            return refused();
        }
        keys.push(key.to_owned());
        opened = matches!(rest, Rest::Opens);
        entries.push(Entry {
            line: number,
            text: line.to_owned(),
            depth: levels.len() - 1,
            key: key.to_owned(),
            value: match rest {
                Rest::Scalar(value) => Some(value.to_owned()),
                Rest::Opens | Rest::Empty => None,
            },
        });
    }
    Ok(entries)
}

/// `content` (no surrounding whitespace) as `(key, rest)`, or `None` when
/// it is not an entry of the grammar.
fn entry(content: &str) -> Option<(&str, Rest<'_>)> {
    // A quoted key is its content verbatim; a bare key ends before the
    // separation whitespace that may precede its colon.
    let (key, rest) = if let quote @ ('"' | '\'') = content.chars().next()? {
        let body = content.get(1..)?;
        let close = body.find(quote)?;
        (body.get(..close)?, body.get(close + 1..)?)
    } else {
        // The key ends at YAML's mapping indicator: the first `:` followed
        // by whitespace or ending the line — `ghe.example:8443:` is one key,
        // its inner colon plain text.
        let colon = content.match_indices(':').map(|(at, _)| at).find(|&at| {
            content
                .get(at + 1..)
                .is_some_and(|after| after.is_empty() || after.starts_with(SPACE))
        })?;
        (
            content.get(..colon)?.trim_end_matches(SPACE),
            content.get(colon..)?,
        )
    };
    if !is_key(key) {
        return None;
    }
    let rest = rest.trim_start_matches(SPACE).strip_prefix(':')?;
    if !rest.is_empty() && !rest.starts_with(SPACE) {
        return None;
    }
    let rest = rest.trim_start_matches(SPACE);
    if rest.is_empty() || rest.starts_with('#') {
        return Some((key, Rest::Opens));
    }
    if let Some(after) = rest.strip_prefix("{}")
        && (after.is_empty()
            || after.starts_with(SPACE) && after.trim_start_matches(SPACE).starts_with('#'))
    {
        return Some((key, Rest::Empty));
    }
    scalar(rest).map(|value| (key, Rest::Scalar(value)))
}

/// `rest` as one scalar on the line, an optional trailing comment dropped;
/// `None` when it is anything else.
fn scalar(rest: &str) -> Option<&str> {
    match rest.chars().next()? {
        quote @ ('"' | '\'') => {
            let body = rest.get(1..)?;
            let close = body.find(quote)?;
            let value = body.get(..close)?;
            let after = body.get(close + 1..)?.trim_start_matches(SPACE);
            (!value.contains('\\') && (after.is_empty() || after.starts_with('#'))).then_some(value)
        }
        first if INDICATORS.contains(first) => None,
        _ => {
            let value = rest
                .find(" #")
                .or_else(|| rest.find("\t#"))
                .map_or(rest, |at| rest.get(..at).unwrap_or(rest))
                .trim_end_matches(SPACE);
            (!value.contains(": ") && !value.contains(":\t") && !value.ends_with(':'))
                .then_some(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, parse};

    fn keys(text: &str) -> Vec<(usize, String, Option<String>)> {
        parse(text)
            .expect("in the grammar")
            .into_iter()
            .map(|entry| (entry.depth, entry.key, entry.value))
            .collect()
    }

    #[test]
    fn the_files_gh_writes_are_read_with_their_nesting() {
        let config = "# What protocol to use when performing git operations. Supported values: ssh, https\ngit_protocol: https\n# What editor gh should run when creating issues, pull requests, etc. If blank, will refer to environment.\neditor:\naliases:\n    co: pr checkout\n    igrep: issue list --label=\"$1\"\nversion: \"1\"\n";
        assert_eq!(
            keys(config),
            [
                (0, "git_protocol".to_owned(), Some("https".to_owned())),
                (0, "editor".to_owned(), None),
                (0, "aliases".to_owned(), None),
                (1, "co".to_owned(), Some("pr checkout".to_owned())),
                (
                    1,
                    "igrep".to_owned(),
                    Some("issue list --label=\"$1\"".to_owned())
                ),
                (0, "version".to_owned(), Some("1".to_owned())),
            ]
        );
        let hosts = "ghe.example:8443:\n    user: m\n    oauth_token: gho_x\n    git_protocol: ssh\n    users:\n        m:\n            oauth_token: gho_x\nother.example:\n    users:\n        n: {}\n    user: n\n";
        assert_eq!(
            keys(hosts)
                .into_iter()
                .map(|(depth, key, _)| (depth, key))
                .collect::<Vec<_>>(),
            [
                (0, "ghe.example:8443".to_owned()),
                (1, "user".to_owned()),
                (1, "oauth_token".to_owned()),
                (1, "git_protocol".to_owned()),
                (1, "users".to_owned()),
                (2, "m".to_owned()),
                (3, "oauth_token".to_owned()),
                (0, "other.example".to_owned()),
                (1, "users".to_owned()),
                (2, "n".to_owned()),
                (1, "user".to_owned()),
            ]
        );
    }

    #[test]
    fn a_hand_edit_inside_the_grammar_reads_as_gh_reads_it() {
        for (text, expected) in [
            (
                "\u{feff}hosts:\n    a.example:\n",
                vec![(0, "hosts", None), (1, "a.example", None)],
            ),
            (
                "\"hosts\" :\n    'a.example'\t:\t# main\n",
                vec![(0, "hosts", None), (1, "a.example", None)],
            ),
            (
                "hosts:  # comment\n\n    a.example:\n",
                vec![(0, "hosts", None), (1, "a.example", None)],
            ),
            (
                "  a.example:\n      user: m\n",
                vec![(0, "a.example", None), (1, "user", Some("m"))],
            ),
            (
                "a:\n    b:\n        c: 1\n    d: 'x # y' # z\ne: -\n",
                vec![],
            ),
        ] {
            if expected.is_empty() {
                assert!(parse(text).is_err(), "{text:?}");
                continue;
            }
            assert_eq!(
                keys(text),
                expected
                    .into_iter()
                    .map(|(depth, key, value)| (depth, key.to_owned(), value.map(str::to_owned)))
                    .collect::<Vec<_>>(),
                "{text:?}"
            );
        }
        assert_eq!(
            keys("a:\n    b:\n        c: 1\n    d: 'x # y' # z\ne: f\n"),
            [
                (0, "a".to_owned(), None),
                (1, "b".to_owned(), None),
                (2, "c".to_owned(), Some("1".to_owned())),
                (1, "d".to_owned(), Some("x # y".to_owned())),
                (0, "e".to_owned(), Some("f".to_owned())),
            ]
        );
    }

    #[test]
    fn the_first_line_outside_the_grammar_is_the_error_with_its_number() {
        for (text, line) in [
            // Round-13: headings YAML decodes to `hosts` that a literal
            // reader does not, and structures that hold a hosts key knives
            // never reads.
            ("version: \"1\"\n\"\\x68osts\":\n    a.example:\n", 2),
            ("version: \"1\"\n? hosts\n: {a.example: {user: m}}\n", 2),
            ("{version: \"1\", hosts: {a.example: {user: m}}}\n", 1),
            ("version: \"1\"\n&a hosts:\n    a.example:\n", 2),
            ("version: \"1\"\n---\nhosts:\n    a.example:\n", 2),
            ("version: \"1\"\nhosts\n", 2),
            ("hosts: {}\n    a.example:\n", 2),
            ("hosts: {} x\n", 1),
            ("hosts: {a: b}\n", 1),
            ("version: \"1\"\nhosts: !!map\n    a.example:\n", 2),
            ("version: \"1\"\nhosts: *anchor\n", 2),
            ("a.example:\n  - x\n", 2),
            ("a.example:\n\tuser: m\n", 2),
            ("a.example:\n    user: m\n  {}\n", 3),
            ("a.example:\n    user: m\n  other.example:\n", 3),
            ("  a.example:\n    user: m\nother.example:\n", 3),
            ("a.example: m\n    user: m\n", 2),
            ("a.example:\n    user: m\n    user: n\n", 3),
            ("\u{a0}a.example:\n", 1),
            ("a.example\u{a0}:\n", 1),
            ("a/example:\n", 1),
            ("\"a.example\\\"\":\n", 1),
            ("\"a.example \":\n    user: m\n", 1),
            ("' a.example':\n    user: m\n", 1),
            ("\"hosts \":\n    a.example:\n", 1),
            ("{}\na.example:\n", 1),
            ("a.example:\n{}\n", 2),
            (" {}\n", 1),
            ("{} # c\n", 1),
            ("a.example:\n    user: \"m\n", 2),
            ("a.example:\n    user: \"m\" x\n", 2),
            ("a.example:\n    user: m: n\n", 2),
            ("a.example:\n    user: m:\n", 2),
            ("a.example:\n    user: |\n        m\n", 2),
            ("a.example:\n    user: [m]\n", 2),
            ("a.example:\n    user: m\x01\n", 2),
            ("%YAML 1.2\n---\na.example:\n", 1),
            ("a.example:x\n", 1),
        ] {
            assert_eq!(parse(text).map_err(|(at, _)| at), Err(line), "{text:?}");
        }
        assert_eq!(
            parse("a:\n   b: c\n"),
            Ok(vec![
                Entry {
                    line: 1,
                    text: "a:".to_owned(),
                    depth: 0,
                    key: "a".to_owned(),
                    value: None,
                },
                Entry {
                    line: 2,
                    text: "   b: c".to_owned(),
                    depth: 1,
                    key: "b".to_owned(),
                    value: Some("c".to_owned()),
                },
            ])
        );
        assert_eq!(parse(""), Ok(Vec::new()));
        assert_eq!(parse("# only\n\n"), Ok(Vec::new()));
        // The whole-file empty map gh writes after the last logout.
        assert_eq!(parse("{}\n"), Ok(Vec::new()));
        assert_eq!(parse("{}"), Ok(Vec::new()));
        assert_eq!(parse("\u{feff}{}\r\n"), Ok(Vec::new()));
    }
}
