//! One reading of a `gh` command line, the way cobra and pflag read it.
//!
//! Every question `knives gh` asks of its arguments — which repository a
//! `pr create` targets, which head it states, which endpoint a `gh api` call
//! addresses, whether it carries a body — is a lookup on the one
//! [`GhInvocation`] this module builds, never a second scan of argv. Five
//! review rounds found the same defect under different spellings: readers
//! that each walked argv on their own read a flag's value as a positional, a
//! first occurrence where gh keeps the last, or a valued flag before the verb
//! as the verb. A single parse removes the class.
//!
//! The grammar is gh's: `gh [--help|--version] <command> [flags…] <verb>
//! [flags…] [positionals…]`, flags anywhere after the command, a persistent
//! `-R/--repo` valid before or after the verb, each flag's kind from a
//! per-command table (the tables are audited against gh's own `--help` text,
//! kept under `tests/fixtures`), value spellings `--flag v`, `--flag=v`,
//! `-f v`, `-fv`, `-f=v`, shorthand clusters whose first valued shorthand takes
//! the rest of the cluster or the next argument, `--flag=true|false` and
//! `-d=true` on a switch, `--` ending flags, string flags last-wins, and gh's
//! own aliases of the verbs knives reads (`pr new` is `pr create`, `pr co` is
//! `pr checkout`) normalised. A dash-argument the table does not define is
//! kept by name, for the caller that must refuse rather than guess.

/// What a flag does with the argument after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Takes a value: the next argument, or what follows `=` or the shorthand.
    Valued,
    /// Takes none; `--flag=true|false` and `-x=true` are accepted, as pflag does.
    Switch,
}

/// One flag gh defines: its long name, its shorthand, and its kind.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub long: &'static str,
    pub short: Option<char>,
    pub kind: Kind,
}

const fn valued(long: &'static str, short: Option<char>) -> Spec {
    Spec {
        long,
        short,
        kind: Kind::Valued,
    }
}

const fn switch(long: &'static str, short: Option<char>) -> Spec {
    Spec {
        long,
        short,
        kind: Kind::Switch,
    }
}

/// cobra's own flags, on every command.
const COBRA_FLAGS: &[Spec] = &[switch("help", Some('h'))];

/// `gh pr`'s persistent flags, valid before or after the verb.
pub const PR_FLAGS: &[Spec] = &[valued("repo", Some('R'))];

/// `gh pr create`'s flags, as `gh pr create --help` lists them (gh 2.98.0).
pub const PR_CREATE_FLAGS: &[Spec] = &[
    valued("assignee", Some('a')),
    valued("base", Some('B')),
    valued("body", Some('b')),
    valued("body-file", Some('F')),
    switch("draft", Some('d')),
    switch("dry-run", None),
    switch("editor", Some('e')),
    switch("fill", Some('f')),
    switch("fill-first", None),
    switch("fill-verbose", None),
    valued("head", Some('H')),
    valued("label", Some('l')),
    valued("milestone", Some('m')),
    switch("no-maintainer-edit", None),
    valued("project", Some('p')),
    valued("recover", None),
    valued("reviewer", Some('r')),
    valued("template", Some('T')),
    valued("title", Some('t')),
    switch("web", Some('w')),
];

/// `gh api`'s flags, as `gh api --help` lists them (gh 2.98.0).
pub const API_FLAGS: &[Spec] = &[
    switch("allow-escape-sequences", None),
    valued("cache", None),
    valued("field", Some('F')),
    valued("header", Some('H')),
    valued("hostname", None),
    switch("include", Some('i')),
    valued("input", None),
    valued("jq", Some('q')),
    valued("method", Some('X')),
    switch("paginate", None),
    valued("preview", Some('p')),
    valued("raw-field", Some('f')),
    switch("silent", None),
    switch("slurp", None),
    valued("template", Some('t')),
    switch("verbose", None),
];

/// The valued flags of the `pr` verbs knives injects a positional target
/// for, from gh 2.98.0's help for each: what keeps a flag's value from
/// being read as the target. These verbs' switches are not enumerated —
/// nothing is gated on them — so an unlisted dash-argument is read as a
/// switch there, the shim's own rule.
const PR_TARGETED_VALUED: &[Spec] = &[
    valued("jq", Some('q')),
    valued("template", Some('t')),
    valued("json", None),
    valued("body", Some('b')),
    valued("body-file", Some('F')),
    valued("branch", None),
    valued("comment", Some('c')),
    valued("reason", Some('r')),
    valued("color", None),
    valued("interval", Some('i')),
    valued("subject", None),
    valued("match-head-commit", None),
    valued("author-email", Some('A')),
    valued("label", Some('l')),
    valued("milestone", Some('m')),
    valued("project", Some('p')),
    valued("reviewer", None),
    valued("assignee", None),
    valued("title", Some('T')),
    valued("recover", None),
    valued("add-assignee", None),
    valued("remove-assignee", None),
    valued("add-label", None),
    valued("remove-label", None),
    valued("add-reviewer", None),
    valued("remove-reviewer", None),
    valued("add-project", None),
    valued("remove-project", None),
];

/// How strictly a command's flags are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strictness {
    /// Every flag is in the table; one that is not is recorded as unknown.
    Complete,
    /// Only the valued flags are in the table; an unlisted dash-argument is a
    /// switch, which is all a passthrough needs to keep positionals apart.
    ValuedOnly,
}

/// The flags one command (or one verb of it) defines.
#[derive(Debug, Clone, Copy)]
struct Grammar {
    tables: [&'static [Spec]; 3],
    strictness: Strictness,
}

impl Grammar {
    fn lookup_long(&self, name: &str) -> Option<&'static Spec> {
        self.tables
            .iter()
            .flat_map(|table| table.iter())
            .find(|spec| spec.long == name)
    }

    fn lookup_short(&self, short: char) -> Option<&'static Spec> {
        self.tables
            .iter()
            .flat_map(|table| table.iter())
            .find(|spec| spec.short == Some(short))
    }
}

/// One flag occurrence, in argument order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// The flag's long name, whichever spelling was used.
    pub name: &'static str,
    /// Its value; `None` for a switch given bare, or a valued flag with no
    /// argument left to take (gh's own error).
    pub value: Option<String>,
}

/// A `gh` command line, read once.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GhInvocation {
    /// The command: `pr`, `api`, `auth`, … `None` when argv starts with a
    /// flag (`gh --version`) or is empty.
    pub command: Option<String>,
    /// For `pr`, the verb after gh's aliases are normalised; the index it sat
    /// at in argv rides with it, for the caller that inserts after it.
    pub verb: Option<(String, usize)>,
    /// Every flag, in order.
    pub flags: Vec<Flag>,
    /// Every positional after the command, the verb excluded.
    pub positionals: Vec<String>,
    /// Every dash-argument the command's table does not define, as written.
    pub unknown: Vec<String>,
}

impl GhInvocation {
    /// Read `args` — everything after `gh` — as gh would.
    pub fn parse(args: &[String]) -> Self {
        let Some(command) = args.first().filter(|first| !first.starts_with('-')) else {
            return Self::default();
        };
        let mut invocation = Self {
            command: Some(command.clone()),
            ..Self::default()
        };
        let rest = args.get(1..).unwrap_or(&[]);
        match command.as_str() {
            "pr" => {
                let verb = find_verb(rest, PR_FLAGS).map(|(verb, index)| {
                    let canonical = match verb {
                        "new" => "create",
                        "co" => "checkout",
                        other => other,
                    };
                    (canonical.to_owned(), index + 1)
                });
                let grammar = match verb.as_ref().map(|(verb, _)| verb.as_str()) {
                    Some("create") => Grammar {
                        tables: [COBRA_FLAGS, PR_FLAGS, PR_CREATE_FLAGS],
                        strictness: Strictness::Complete,
                    },
                    _ => Grammar {
                        tables: [COBRA_FLAGS, PR_FLAGS, PR_TARGETED_VALUED],
                        strictness: Strictness::ValuedOnly,
                    },
                };
                let skip = verb.as_ref().map(|(_, index)| index - 1);
                invocation.verb = verb;
                parse_flags(rest, skip, grammar, &mut invocation);
            }
            "api" => {
                // `-R` is not a `gh api` flag (gh refuses it), but the shim
                // routed a token by it and scripts pass it; read, so the
                // routing keeps working and the endpoint stays a positional.
                parse_flags(
                    rest,
                    None,
                    Grammar {
                        tables: [COBRA_FLAGS, API_FLAGS, PR_FLAGS],
                        strictness: Strictness::Complete,
                    },
                    &mut invocation,
                );
            }
            _ => {
                // A command knives only routes a token for: `-R/--repo` is
                // read (the shim did), nothing else is judged.
                parse_flags(
                    rest,
                    None,
                    Grammar {
                        tables: [COBRA_FLAGS, PR_FLAGS, &[]],
                        strictness: Strictness::ValuedOnly,
                    },
                    &mut invocation,
                );
            }
        }
        invocation
    }

    /// Every value the flag `name` was given, in order.
    pub fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.flags
            .iter()
            .filter(move |flag| flag.name == name)
            .filter_map(|flag| flag.value.as_deref())
    }

    /// The value gh keeps for a string flag: the last one given.
    pub fn last(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|flag| flag.name == name && flag.value.is_some())
            .and_then(|flag| flag.value.as_deref())
    }

    /// Whether the flag `name` appeared at all, with a value or without.
    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag.name == name)
    }

    /// The `pr` verb, alias-normalised.
    pub fn verb(&self) -> Option<&str> {
        self.verb.as_ref().map(|(verb, _)| verb.as_str())
    }

    /// `key=value` field arguments (`-f`/`--raw-field`, `-F`/`--field`) whose key
    /// is `key`, in order.
    pub fn fields<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.flags
            .iter()
            .filter(|flag| matches!(flag.name, "raw-field" | "field"))
            .filter_map(|flag| flag.value.as_deref())
            .filter_map(move |assignment| {
                assignment
                    .split_once('=')
                    .filter(|(name, _)| *name == key)
                    .map(|(_, value)| value)
            })
    }
}

/// The verb the way cobra's `stripFlags` finds it: the first argument after
/// the command that is not a flag or a flag's value. Between the command and
/// its verb, a `--long` without `=` or a two-character `-x` that is not one of
/// the parent's own switches takes the next argument with it — cobra does not
/// know the child's flags yet, so it assumes a value; a longer cluster (`-dH…`)
/// or an `=` form stands alone.
fn find_verb<'a>(rest: &'a [String], parent: &[Spec]) -> Option<(&'a str, usize)> {
    let mut index = 0;
    while let Some(argument) = rest.get(index) {
        if argument == "--" {
            return None;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let bare = long.split_once('=').is_none();
            let parent_switch = long == "help"
                || parent
                    .iter()
                    .any(|spec| spec.long == long && spec.kind == Kind::Switch);
            index += if bare && !parent_switch { 2 } else { 1 };
            continue;
        }
        if let Some(shorts) = argument.strip_prefix('-')
            && !shorts.is_empty()
        {
            let two = shorts.chars().count() == 1;
            let parent_switch = shorts == "h"
                || parent.iter().any(|spec| {
                    spec.short.is_some_and(|short| shorts == short.to_string())
                        && spec.kind == Kind::Switch
                });
            index += if two && !parent_switch { 2 } else { 1 };
            continue;
        }
        return Some((argument.as_str(), index));
    }
    None
}

/// Read every flag and positional in `rest` against `grammar`, pflag's way;
/// `skip` is the verb's index in `rest`, a positional that is not recorded.
fn parse_flags(
    rest: &[String],
    skip: Option<usize>,
    grammar: Grammar,
    invocation: &mut GhInvocation,
) {
    let mut index = 0;
    while let Some(argument) = rest.get(index) {
        let at = index;
        index += 1;
        if Some(at) == skip {
            continue;
        }
        if argument == "--" {
            invocation
                .positionals
                .extend(rest.get(index..).unwrap_or(&[]).iter().cloned());
            return;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            let Some(Spec {
                long: name, kind, ..
            }) = grammar.lookup_long(name)
            else {
                match grammar.strictness {
                    Strictness::Complete => invocation.unknown.push(argument.clone()),
                    // An unlisted long flag with no `=`: cobra would take
                    // the next argument only if the flag is valued, which
                    // is unknown here; a switch is assumed, as the shim did.
                    Strictness::ValuedOnly => {}
                }
                continue;
            };
            let value = match (kind, inline) {
                (_, Some(value)) => Some(value.to_owned()),
                (Kind::Valued, None) => {
                    index += 1;
                    rest.get(index - 1).cloned()
                }
                (Kind::Switch, None) => None,
            };
            invocation.flags.push(Flag { name, value });
            continue;
        }
        let Some(cluster) = argument.strip_prefix('-').filter(|rest| !rest.is_empty()) else {
            // A positional, or the bare `-` gh reads as stdin.
            invocation.positionals.push(argument.clone());
            continue;
        };
        for (offset, short) in cluster.char_indices() {
            let tail = cluster.get(offset + short.len_utf8()..).unwrap_or("");
            let Some(Spec {
                long: name, kind, ..
            }) = grammar.lookup_short(short)
            else {
                match grammar.strictness {
                    Strictness::Complete => invocation.unknown.push(argument.clone()),
                    Strictness::ValuedOnly => {}
                }
                break;
            };
            match *kind {
                Kind::Switch => {
                    // pflag: a switch shorthand followed by `=` takes what
                    // follows as its (boolean) value and ends the cluster.
                    if let Some(value) = tail.strip_prefix('=') {
                        invocation.flags.push(Flag {
                            name,
                            value: Some(value.to_owned()),
                        });
                        break;
                    }
                    invocation.flags.push(Flag { name, value: None });
                }
                Kind::Valued => {
                    // pflag: `-Hv` and `-H=v` give `v`; `-H=` alone gives `=`;
                    // an empty tail takes the next argument.
                    let value = if tail.is_empty() {
                        index += 1;
                        rest.get(index - 1).cloned()
                    } else if tail.len() > 1 {
                        Some(tail.strip_prefix('=').unwrap_or(tail).to_owned())
                    } else {
                        Some(tail.to_owned())
                    };
                    invocation.flags.push(Flag { name, value });
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|a| (*a).to_owned()).collect()
    }

    fn heads(invocation: &GhInvocation) -> Vec<&str> {
        invocation.values("head").collect()
    }

    #[test]
    fn the_verb_is_found_past_flags_and_their_values_and_aliases_are_normalised() {
        for (argv, verb) in [
            (vec!["pr", "create"], "create"),
            (vec!["pr", "new"], "create"),
            (vec!["pr", "co", "12"], "checkout"),
            (vec!["pr", "-R", "o/r", "create"], "create"),
            (vec!["pr", "--repo", "o/r", "create"], "create"),
            (vec!["pr", "--repo=o/r", "create"], "create"),
            (vec!["pr", "-Ro/r", "create"], "create"),
            (vec!["pr", "-H", "feat/x", "create"], "create"),
            (vec!["pr", "--head=feat/x", "create"], "create"),
            (vec!["pr", "--title", "t", "create"], "create"),
            (vec!["pr", "--help", "create"], "create"),
            (vec!["pr", "-h", "view"], "view"),
            (vec!["pr", "-dHfeat/x", "create"], "create"),
        ] {
            let parsed = GhInvocation::parse(&args(&argv));
            assert_eq!(parsed.verb(), Some(verb), "{argv:?}");
        }
        // cobra treats an unlisted switch before the verb as valued and eats
        // the verb; gh then errors. `-R` with no value likewise names no verb.
        for argv in [
            vec!["pr", "--draft", "create"],
            vec!["pr", "-R", "create"],
            vec!["pr", "-R", "o/r"],
            vec!["pr", "--", "create"],
        ] {
            assert_eq!(GhInvocation::parse(&args(&argv)).verb(), None, "{argv:?}");
        }
        assert_eq!(
            GhInvocation::parse(&args(&["issue", "list"]))
                .command
                .as_deref(),
            Some("issue")
        );
        assert_eq!(GhInvocation::parse(&args(&["--version"])).command, None);
    }

    #[test]
    fn heads_are_read_as_pflag_reads_them() {
        for argv in [
            vec!["pr", "create", "--head", "feat/x"],
            vec!["pr", "create", "--head=feat/x"],
            vec!["pr", "create", "-H", "feat/x"],
            vec!["pr", "create", "-H=feat/x"],
            vec!["pr", "create", "-Hfeat/x"],
            vec!["pr", "create", "--title", "t", "-Hfeat/x", "--body", "b"],
            vec!["pr", "create", "-dHfeat/x"],
            vec!["pr", "create", "-fHfeat/x", "--title", "t"],
            vec!["pr", "create", "-dH", "feat/x"],
            vec!["pr", "create", "-wdH=feat/x"],
            vec!["pr", "-R", "o/r", "create", "-H", "feat/x"],
            // Before the verb: cobra hands the child every flag.
            vec!["pr", "-H", "feat/x", "create"],
            vec!["pr", "--head=feat/x", "create"],
            vec!["pr", "-H=feat/x", "create", "-R", "o/r"],
            vec!["pr", "--title", "t", "new", "-H", "feat/x"],
            vec!["pr", "create", "-d=true", "-H", "feat/x"],
        ] {
            let parsed = GhInvocation::parse(&args(&argv));
            assert_eq!(heads(&parsed), ["feat/x"], "{argv:?}");
            assert!(parsed.unknown.is_empty(), "{argv:?}: {:?}", parsed.unknown);
        }
        // Every occurrence, in order; the last is gh's.
        let two = GhInvocation::parse(&args(&["pr", "create", "--head", "feat/x", "-Hfeat/y"]));
        assert_eq!(heads(&two), ["feat/x", "feat/y"]);
        assert_eq!(two.last("head"), Some("feat/y"));
        // A valued flag's value is never a head, whatever it looks like.
        for argv in [
            vec!["pr", "create", "--title", "t", "--body", "-Hfeat/x"],
            vec!["pr", "create", "--body=-Hfeat/x"],
            vec!["pr", "create", "-b", "-Hfeat/x"],
            vec!["pr", "create", "-b-Hfeat/x"],
            vec!["pr", "create", "-l", "--head", "feat/x"],
            vec!["pr", "create", "--recover", "--head=feat/x"],
            vec!["pr", "create", "--", "--head", "feat/x"],
        ] {
            let parsed = GhInvocation::parse(&args(&argv));
            assert!(heads(&parsed).is_empty(), "{argv:?}: {:?}", parsed.flags);
        }
        // `-H` alone at the end has no value; `--head=` an empty one; `-H=`
        // the literal `=`, as pflag reads it.
        assert_eq!(
            GhInvocation::parse(&args(&["pr", "create", "-H"])).flags,
            [Flag {
                name: "head",
                value: None
            }]
        );
        assert_eq!(
            heads(&GhInvocation::parse(&args(&["pr", "create", "--head="]))),
            [""]
        );
        assert_eq!(
            heads(&GhInvocation::parse(&args(&["pr", "create", "-H="]))),
            ["="]
        );
    }

    #[test]
    fn a_flag_the_table_lacks_is_kept_by_name_where_the_table_is_complete() {
        let parsed =
            GhInvocation::parse(&args(&["pr", "create", "--mystery", "x", "-H", "feat/x"]));
        assert_eq!(parsed.unknown, ["--mystery"]);
        let parsed = GhInvocation::parse(&args(&["pr", "create", "-dZ", "-H", "feat/x"]));
        assert_eq!(parsed.unknown, ["-dZ"]);
        let parsed = GhInvocation::parse(&args(&["api", "--nope", "repos/o/r"]));
        assert_eq!(parsed.unknown, ["--nope"]);
        // A `pr view` switch nobody listed is a switch, not an unknown: nothing
        // is gated there, and the positional must still be told from a value.
        let parsed = GhInvocation::parse(&args(&["pr", "view", "--web", "123"]));
        assert!(parsed.unknown.is_empty());
        assert_eq!(parsed.positionals, ["123"]);
        let parsed = GhInvocation::parse(&args(&["pr", "view", "--json", "title"]));
        assert!(parsed.positionals.is_empty());
    }

    #[test]
    fn the_repository_flag_is_last_wins_in_every_spelling_before_or_after_the_verb() {
        for argv in [
            vec!["pr", "list", "-R", "acme/work"],
            vec!["pr", "list", "--repo", "acme/work"],
            vec!["pr", "list", "--repo=acme/work"],
            vec!["pr", "list", "-R=acme/work"],
            vec!["pr", "list", "-Racme/work"],
            vec!["pr", "-R", "acme/work", "create"],
            vec!["issue", "list", "-R", "acme/work"],
            vec!["pr", "create", "-R", "zz/yy", "-R", "acme/work"],
            vec!["pr", "create", "--body", "-R", "--repo", "acme/work"],
        ] {
            assert_eq!(
                GhInvocation::parse(&args(&argv)).last("repo"),
                Some("acme/work"),
                "{argv:?}"
            );
        }
        assert_eq!(
            GhInvocation::parse(&args(&["pr", "list"])).last("repo"),
            None
        );
        assert_eq!(
            GhInvocation::parse(&args(&["pr", "list", "-R"])).last("repo"),
            None
        );
    }

    #[test]
    fn api_positionals_are_the_endpoint_and_flag_values_are_not() {
        let parsed = GhInvocation::parse(&args(&[
            "api",
            "-X",
            "POST",
            "-t",
            "repos/zz/yy/pulls",
            "--jq",
            "repos/zz/yy/pulls",
            "repos/o/r/pulls",
            "-f",
            "head=feat/x",
            "-Fbase=main",
            "--input",
            "repos/zz/yy/pulls",
        ]));
        assert_eq!(parsed.positionals, ["repos/o/r/pulls"]);
        assert_eq!(parsed.last("method"), Some("POST"));
        assert_eq!(parsed.fields("head").collect::<Vec<_>>(), ["feat/x"]);
        assert_eq!(parsed.fields("base").collect::<Vec<_>>(), ["main"]);
        assert!(parsed.has("input"));
        assert!(parsed.unknown.is_empty());
        // The method is last-wins, and the attached spelling is read.
        let parsed = GhInvocation::parse(&args(&[
            "api",
            "-X",
            "GET",
            "-X",
            "POST",
            "repos/o/r/pulls",
        ]));
        assert_eq!(parsed.last("method"), Some("POST"));
        let parsed = GhInvocation::parse(&args(&["api", "-XGET", "repos/o/r/pulls"]));
        assert_eq!(parsed.last("method"), Some("GET"));
        let parsed = GhInvocation::parse(&args(&["api", "--method=get", "repos/o/r/pulls"]));
        assert_eq!(parsed.last("method"), Some("get"));
    }
}
