//! Properties of `knives gh`'s argument routing, checked on every argument list.
//!
//! The input bytes, split on `\0`, are the argument list after `gh`; each word
//! is read as UTF-8 with invalid bytes replaced. A property that does not hold
//! panics, and libFuzzer reports the input that broke it.
//!
//! Every property calls its routing functions on every input, so the first
//! property, that no routing function panics, is the target not crashing.
//!
//! Each property names the routing fix it guards (sjawhar/knives):
//! #115 routes `gh search` by its `--owner` flag; #116 stops owner scans at
//! the `--` terminator; #117 stops PR subcommand scans at the `--` terminator;
//! #118 reads `--` after a value flag as its value, not a terminator; #126
//! routes `gh repo` subcommands by their repository positional, scp-form specs
//! included; #127 routes by a PR or issue selector URL. "The login rule" is
//! the fix that refuses a target whose owner is no GitHub login (`gh api
//! repos/a:b/x` routed to owner `b`).
#![no_main]

use knives::commands::gh::fuzzing::{
    Named, before_terminator, command_words, is_owner_shaped, normalize_url, owner_from_api_args,
    positional_after, repo_flag, selector_url_target, takes_value, url_from_spec,
};
use libfuzzer_sys::fuzz_target;

/// The App's host, assembled from fragments as the unit tests assemble it, so
/// this source names no forge URL: the rule `tests/no_hardcoded_identity.rs`
/// keeps for the shipped source.
const HOST: &str = concat!("github", ".com");

fuzz_target!(|data: &[u8]| {
    let args: Vec<String> = data
        .split(|byte| *byte == 0)
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect();
    terminator_ends_the_flags(&args);
    owner_scans_read_flags_only(&args);
    subcommand_scans_stop_at_the_terminator(&args);
    routing_names_one_owner(&args);
    search_routes_by_its_owner_flag(&args);
    a_selector_url_routes_to_its_repository(&args);
    an_scp_spec_routes_to_its_host(&args);
    normalized_urls_are_fixed_points(&args);
    a_target_naming_no_login_routes_nothing(&args);
});

/// `before_terminator` keeps exactly the words gh parses as flags: a prefix of
/// the arguments that holds no `--` except as the value of a value-taking
/// flag, cut only at a `--` that gh takes as the terminator.
///
/// Guards #118 (`gh pr comment 5 --body -- -R acme/w` still targets acme/w),
/// and the cut that #116 and #117 stop their scans at.
fn terminator_ends_the_flags(args: &[String]) {
    let flags = before_terminator(args);
    assert!(
        args.starts_with(flags),
        "the flags are not a prefix of {args:?}"
    );
    let mut value_next = false;
    for word in flags {
        if std::mem::take(&mut value_next) {
            continue;
        }
        assert_ne!(word, "--", "a terminator among the flags of {args:?}");
        value_next = takes_value(word);
    }
    if let Some(cut) = args.get(flags.len()) {
        assert!(
            cut == "--" && !value_next,
            "the flags end at {cut:?}, not at a terminator: {args:?}"
        );
    }
}

/// Words after the terminator never route: `gh search issues -- --owner acme`
/// searches for the words `--owner acme`. Dropping them changes neither the
/// repository flag nor the owner, except a `gh api` endpoint path, which gh
/// reads as a positional wherever it sits (`gh api -- repos/acme/x`).
///
/// Guards #116.
fn owner_scans_read_flags_only(args: &[String]) {
    let flags = before_terminator(args);
    let Some(through_terminator) = args.get(..=flags.len()) else {
        return;
    };
    assert_eq!(
        repo_flag(args),
        repo_flag(through_terminator),
        "a word after the terminator changed the repository flag: {args:?}"
    );
    if !is_api_with_endpoint(args) {
        assert_eq!(
            owner_from_api_args(args),
            owner_from_api_args(through_terminator),
            "a word after the terminator changed the owner: {args:?}"
        );
    }
}

/// The subcommand scans stop at the terminator: the command words come from
/// the flags alone (`gh pr -- view` names no subcommand), and the positional
/// after a subcommand is the first one among the flags, or else the first word
/// after `--` (`gh pr view -- 123`). A word after `--` is never a subcommand
/// with a positional of its own.
///
/// Guards #117.
fn subcommand_scans_stop_at_the_terminator(args: &[String]) {
    let flags = before_terminator(args);
    let words = command_words(args);
    assert_eq!(
        words,
        command_words(flags),
        "a word after the terminator changed the command words: {args:?}"
    );
    if let Some((_, at, subcommand)) = words {
        assert_eq!(
            flags.get(at).map(String::as_str),
            Some(subcommand),
            "the subcommand is not the flag word at its index: {args:?}"
        );
    }
    let after_terminator = args.get(flags.len() + 1).map(String::as_str);
    for at in 0..=args.len() {
        let expected = if at < flags.len() {
            positional_after(flags, at).or(after_terminator)
        } else {
            None
        };
        assert_eq!(
            positional_after(args, at),
            expected,
            "the positional after index {at}: {args:?}"
        );
    }
}

/// Each routing function names at most one owner, and that owner is a GitHub
/// login: the owner `owner_from_api_args` names, the `<owner>` of the one
/// `<owner>/<repo>` `selector_url_target` names, and the owner that the spec
/// `repo_flag` names routes to. That spec is a repository flag's value among
/// the flags.
///
/// Guards the login rule, and #116's `repo_flag` half (a value after the
/// terminator is no flag's value). `None` names no owner, so this property
/// cannot see a revert that stops a function from naming one (#115, #127).
fn routing_names_one_owner(args: &[String]) {
    if let Some(Named::Target(owner)) = owner_from_api_args(args) {
        assert!(
            is_owner_shaped(&owner),
            "owner {owner:?} is not a login: {args:?}"
        );
    }
    if let Some(Named::Target(target)) = selector_url_target(args) {
        let one_repository = !target.contains(['?', '#'])
            && target.split_once('/').is_some_and(|(owner, repository)| {
                is_owner_shaped(owner) && !repository.is_empty() && !repository.contains('/')
            });
        assert!(
            one_repository,
            "selector target {target:?} is not one <login>/<repo>: {args:?}"
        );
    }
    if let Some(Named::Target(spec)) = repo_flag(args) {
        let flags = before_terminator(args);
        let flagged = flags.iter().enumerate().any(|(index, word)| {
            if word == "-R" || word == "--repo" {
                flags.get(index + 1) == Some(&spec)
            } else {
                word.strip_prefix("--repo=")
                    .or_else(|| word.strip_prefix("-R="))
                    == Some(spec.as_str())
            }
        });
        assert!(
            flagged,
            "repository {spec:?} is no repository flag's value: {args:?}"
        );
        assert!(
            routed_owner(&spec).as_deref().is_some_and(is_owner_shaped),
            "repository {spec:?} routes to no login: {args:?}"
        );
    }
}

/// `gh search` routes by its `--owner` flag: `--owner X` or `--owner=X` right
/// after `search` names owner X for every login X, whatever follows; and so
/// does `--owner X` after other flags and their values, whatever those values
/// are (`gh search issues --label --owner --owner acme` filters by `acme`,
/// with the label `--owner`).
///
/// Guards #115, and the search scan's reading of flag values.
fn search_routes_by_its_owner_flag(args: &[String]) {
    let Some((owner, rest)) = args.split_first() else {
        return;
    };
    if !is_owner_shaped(owner) {
        return;
    }
    let leading = |flag: Vec<String>| -> Vec<String> {
        std::iter::once("search".to_owned())
            .chain(flag)
            .chain(rest.iter().cloned())
            .collect()
    };
    // Each word of the rest becomes another flag's value, a `--flag=value`
    // word, or sits beside a boolean flag, before the owner flag.
    let trailing = ["search".to_owned(), "issues".to_owned()]
        .into_iter()
        .chain(
            rest.iter()
                .enumerate()
                .flat_map(|(index, value)| match index % 4 {
                    0 => vec!["--label".to_owned(), value.clone()],
                    1 => vec!["-L".to_owned(), value.clone()],
                    2 => vec![format!("--sort={value}")],
                    _ => vec!["--web".to_owned()],
                }),
        )
        .chain(["--owner".to_owned(), owner.clone()])
        .collect();
    for call in [
        leading(vec!["--owner".to_owned(), owner.clone()]),
        leading(vec![format!("--owner={owner}")]),
        trailing,
    ] {
        assert_eq!(
            owner_from_api_args(&call),
            Some(Named::Target(owner.clone())),
            "{call:?}"
        );
    }
}

/// A `gh pr` or `gh issue` selector URL routes the call to the URL's
/// repository: `gh pr <subcommand> https://<host>/<owner>/<repo>/pull/7`
/// targets `<owner>/<repo>` for every login and repository segment and every
/// subcommand, also after a leading `-R <value>`, which gh reads before the
/// command words. An owner segment that is no login refuses the call.
///
/// Guards #127, and the login rule for selector URLs.
fn a_selector_url_routes_to_its_repository(args: &[String]) {
    let is_segment = |word: &str| !word.is_empty() && !word.contains(['/', '?', '#']);
    let [owner, repository, rest @ ..] = args else {
        return;
    };
    if !is_segment(owner) || !is_segment(repository) {
        return;
    }
    let subcommand = rest
        .first()
        .filter(|word| !word.starts_with('-'))
        .map_or("view", String::as_str);
    let flag_value = rest.get(1).map_or("x", String::as_str);
    let expected = || {
        Some(if is_owner_shaped(owner) {
            Named::Target(format!("{owner}/{repository}"))
        } else {
            Named::Refused
        })
    };
    for (group, kind) in [("pr", "pull"), ("issue", "issues"), ("issue", "pull")] {
        let selector = format!("https://{HOST}/{owner}/{repository}/{kind}/7");
        for call in [
            vec![group, subcommand, selector.as_str()],
            vec!["-R", flag_value, group, subcommand, selector.as_str()],
        ] {
            let call: Vec<String> = call.into_iter().map(str::to_owned).collect();
            assert_eq!(selector_url_target(&call), expected(), "{call:?}");
        }
    }
}

/// An scp-form spec `[user@]host:path`, read as git and `gh repo clone` read
/// it, routes to `https://host/path` with a `.git` suffix, both as a
/// repository spec (`gh repo clone host:owner/repo`) and as a remote URL.
///
/// Guards #126.
fn an_scp_spec_routes_to_its_host(args: &[String]) {
    let [host, path, rest @ ..] = args else {
        return;
    };
    if host.is_empty() || host.contains(['/', ':', '@']) || path.is_empty() {
        return;
    }
    let spec = rest
        .first()
        .filter(|user| !user.contains(['/', ':']))
        .map_or_else(
            || format!("{host}:{path}"),
            |user| format!("{user}@{host}:{path}"),
        );
    if spec.contains("://") {
        return;
    }
    let mut expected = format!("https://{host}/{path}");
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "The canonical remote suffix is the literal lowercase .git."
    )]
    if !expected.ends_with(".git") {
        expected.push_str(".git");
    }
    assert_eq!(
        url_from_spec(&spec).as_ref(),
        Some(&expected),
        "spec {spec:?}"
    );
    assert_eq!(
        normalize_url(&spec).as_ref(),
        Some(&expected),
        "remote {spec:?}"
    );
}

/// `normalize_url` reaches its canonical form in one step: normalizing a
/// normalized URL changes nothing, for every argument read as a URL and every
/// URL `url_from_spec` builds. One shape is outside it: a bare `host:` (scp
/// form with no path, which names no repository) becomes `host:.git`, which
/// then reads as scp form.
///
/// Guards #126: an scp reading that fires on its own output (one that missed
/// a `://`) breaks the fixed point.
fn normalized_urls_are_fixed_points(args: &[String]) {
    for word in args {
        let bare_scp_host = word.find(':').is_some_and(|colon| colon + 1 == word.len());
        if let Some(url) = normalize_url(word).filter(|_| !bare_scp_host) {
            assert_eq!(normalize_url(&url).as_ref(), Some(&url), "url {word:?}");
        }
        if let Some(url) = url_from_spec(word) {
            assert_eq!(normalize_url(&url).as_ref(), Some(&url), "spec {word:?}");
        }
    }
}

/// A target whose owner is no GitHub login routes nothing, and is refused, so
/// no later source (`GH_REPO`, the checkout) stands in for it: `gh api
/// repos/<o>/<r>` and `-R <o>/<r>` name owner `<o>`, which routes when it is
/// a login and is refused otherwise. A `-R` value that starts like a URL
/// (`git@`, `https:`, …) is a URL to gh, whose owner sits elsewhere; the
/// owner property checks those.
///
/// Guards the login rule.
fn a_target_naming_no_login_routes_nothing(args: &[String]) {
    let [owner, repository, ..] = args else {
        return;
    };
    if owner.is_empty()
        || owner.contains(['/', '?', '{'])
        || repository.is_empty()
        || repository.contains('/')
    {
        return;
    }
    let login = is_owner_shaped(owner);
    let endpoint = vec!["api".to_owned(), format!("repos/{owner}/{repository}")];
    let expected = if login {
        Named::Target(owner.clone())
    } else {
        Named::Refused
    };
    assert_eq!(
        owner_from_api_args(&endpoint),
        Some(expected),
        "{endpoint:?}"
    );
    let spec = format!("{owner}/{repository}");
    if GH_URL_PREFIXES
        .iter()
        .any(|prefix| spec.starts_with(prefix))
    {
        return;
    }
    let flag = vec![
        "pr".to_owned(),
        "list".to_owned(),
        "-R".to_owned(),
        spec.clone(),
    ];
    let expected = if login {
        Named::Target(spec)
    } else {
        Named::Refused
    };
    assert_eq!(repo_flag(&flag), Some(expected), "{flag:?}");
}

/// The prefixes that make gh read a `-R` value as a URL (gh 2.98, measured
/// with `gh browse -R <value> -n`).
const GH_URL_PREFIXES: [&str; 7] = [
    "git@",
    "ssh:",
    "git+ssh:",
    "git:",
    "http:",
    "git+https:",
    "https:",
];

/// The owner a spec routes to: the first path segment of the URL
/// `url_from_spec` builds, which the credential request names.
fn routed_owner(spec: &str) -> Option<String> {
    let url = url_from_spec(spec)?;
    let (_, path) = url.strip_prefix("https://")?.split_once('/')?;
    path.split('/').next().map(str::to_owned)
}

/// Whether this is a `gh api` call naming a REST endpoint whose path carries
/// an owner (`repos/…`, `orgs/…`, `users/…`, with or without a leading `/`).
fn is_api_with_endpoint(args: &[String]) -> bool {
    args.first().is_some_and(|word| word == "api")
        && args.iter().skip(1).any(|word| {
            let bare = word.strip_prefix('/').unwrap_or(word);
            !word.starts_with('-')
                && ["repos/", "orgs/", "users/"]
                    .iter()
                    .any(|prefix| bare.starts_with(prefix))
        })
}
