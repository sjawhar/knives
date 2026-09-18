//! The flag tables `knives gh` reads a command line by are gh's own, and gh
//! moves: a new `pr create` or `api` flag knives does not know is a hole the
//! gate refuses through rather than a hole it guesses through, and this test
//! is where such a hole shows up before an operator meets it. The two
//! `--help` texts under `tests/fixtures` are gh 2.98.0's, captured verbatim;
//! upgrading the fixture is how a gh upgrade is acknowledged, and the table
//! moves with it.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

use knives::commands::gh_args::{API_FLAGS, Kind, PR_CREATE_FLAGS, Spec};

/// One flag as `gh <command> --help` prints it: `  -H, --head branch   …` or
/// `      --dry-run   …`. A flag with a value placeholder after its long name
/// is valued; one followed directly by two spaces is a switch.
#[derive(Debug, PartialEq, Eq)]
struct HelpFlag {
    long: String,
    short: Option<char>,
    kind: Kind,
}

fn flags_in(help: &str) -> Vec<HelpFlag> {
    let flags = help
        .split("\nFLAGS\n")
        .nth(1)
        .expect("a FLAGS section")
        .split("\n\n")
        .next()
        .expect("the FLAGS section ends at a blank line");
    flags
        .lines()
        .filter(|line| line.contains("--"))
        .map(|line| {
            let line = line.trim_start();
            let (short, rest) = match line.strip_prefix('-') {
                Some(rest) if !rest.starts_with('-') => {
                    let short = rest.chars().next().expect("a shorthand letter");
                    let rest = rest.trim_start_matches(short).trim_start_matches(", ");
                    (Some(short), rest)
                }
                _ => (None, line),
            };
            let rest = rest.strip_prefix("--").expect("a long flag");
            let (long, after) = rest.split_once(' ').expect("text after the flag");
            // A value placeholder is one word before the two-space gap to the
            // description; a switch's description follows the gap directly.
            let kind = if after.starts_with(' ') {
                Kind::Switch
            } else {
                Kind::Valued
            };
            HelpFlag {
                long: long.to_owned(),
                short,
                kind,
            }
        })
        .collect()
}

fn assert_table_matches(table: &[Spec], fixture: &str) {
    let help = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture),
    )
    .expect("read the fixture");
    let documented = flags_in(&help);
    assert!(!documented.is_empty(), "{fixture}: no flags parsed");
    for flag in &documented {
        let spec = table
            .iter()
            .find(|spec| spec.long == flag.long)
            .unwrap_or_else(|| panic!("{fixture}: `--{}` is not in the table", flag.long));
        assert_eq!(
            spec.short, flag.short,
            "{fixture}: `--{}` shorthand differs",
            flag.long
        );
        assert_eq!(
            spec.kind, flag.kind,
            "{fixture}: `--{}` takes a value in gh's help but not in the table, or the reverse",
            flag.long
        );
    }
    for spec in table {
        assert!(
            documented.iter().any(|flag| flag.long == spec.long),
            "{fixture}: the table lists `--{}`, which gh's help does not",
            spec.long
        );
    }
}

#[test]
fn the_pr_create_table_is_gh_pr_create_help() {
    assert_table_matches(PR_CREATE_FLAGS, "gh-pr-create-help.txt");
}

#[test]
fn the_api_table_is_gh_api_help() {
    assert_table_matches(API_FLAGS, "gh-api-help.txt");
}
