//! Fork-aware `gh` passthrough.
//!
//! This command executes `gh` directly, so the usual render/run split does not apply:
//! there is no knives result to render.
// allow: SIZE_OK: 1657 lines - single passthrough pipeline; splitting would separate resolution steps that read as one procedure.
use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::{
    fs::PermissionsExt as _,
    process::{CommandExt as _, ExitStatusExt as _},
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_HOST: &str = "github.com";

const GIT_WRAPPER: &str = r#"#!/bin/bash
case "$1" in
    symbolic-ref)
        # Only intercept reads (HEAD is last arg), not writes (HEAD followed by ref).
        if [[ "${!#}" == "HEAD" ]]; then
            if [[ " $* " == *" --short "* ]]; then
                printf '%s\n' "$_JJ_BOOKMARK"
            else
                printf 'refs/heads/%s\n' "$_JJ_BOOKMARK"
            fi
            exit 0
        fi
        ;;
    rev-parse)
        if [[ "${!#}" == "HEAD" && " $* " == *" --abbrev-ref "* ]]; then
            printf '%s\n' "$_JJ_BOOKMARK"
            exit 0
        fi
        ;;
    branch)
        if [[ " $* " == *" --show-current "* ]]; then
            printf '%s\n' "$_JJ_BOOKMARK"
            exit 0
        fi
        ;;
esac
# Pass through every other invocation so gh's writes use the real git unchanged.
# Unlike the shim, do not use an inherited wrapper-dir variable: stacked gh shims
# overwrite it, causing wrappers to select each other forever. Skip the current wrapper.
_self_dir="$(cd "$(dirname "$0")" && pwd)"
_after_self=false
IFS=':' read -ra _path_dirs <<< "$PATH"
for _d in "${_path_dirs[@]}"; do
    if [[ "$_d" == "$_self_dir" ]]; then
        _after_self=true
        continue
    fi
    [[ "$_after_self" == true ]] || continue
    [[ -x "$_d/git" ]] && exec "$_d/git" "$@"
done
echo "error: git not found" >&2
exit 127
"#;

const DETACHED_BOOKMARK: &str = "__jj_detached__";

/// Re-enters the gh shim when invoked directly, otherwise mints an app token when
/// routed, compensates for jj's detached HEAD on `gh pr`, and relays gh's inherited
/// terminal I/O and exit code unchanged.
///
/// Every successful execution path exits the process, making the `Infallible` success type
/// compiler-checked.
pub fn run(args: &[String]) -> anyhow::Result<std::convert::Infallible> {
    if let Some(shim) = reentry_shim() {
        // The shim applies the agent-context rules and the routing include, then
        // re-enters knives with the depth marker and KNIVES_REAL_GH set, so that pass
        // mints normally. Recursion is bounded by construction; the shim's depth
        // guard is the backstop.
        let error = Command::new(&shim).args(args).exec();
        eprintln!("knives gh: cannot exec {}: {error}", shim.display());
        std::process::exit(126);
    }
    let Ok(real_gh) = real_gh() else {
        eprintln!("knives gh: real gh not found");
        std::process::exit(127);
    };
    let mut gh = Command::new(real_gh);
    if is_auth_command(args) {
        // Verbatim, and before the PR scanner: `gh auth token --hostname pr --user view`
        // would otherwise read as `gh pr view` and die on a bookmark it never needed.
        gh.args(args);
        std::process::exit(gh_exit_code(&mut gh));
    }
    let cwd = std::env::current_dir()?;
    let token = if std::env::var_os("GH_TOKEN").is_some() {
        None
    } else {
        match resolve_target_url(args, &cwd).as_deref().map(mint_token) {
            Some(Mint::Token(token)) => Some(token),
            Some(Mint::Refused(code)) => std::process::exit(code),
            Some(Mint::Unrouted) | None => None,
        }
    };
    if let Some(token) = token {
        gh.env("GH_TOKEN", token);
    }

    let Some((subcommand, _)) = pr_subcommand(args) else {
        gh.args(args);
        std::process::exit(gh_exit_code(&mut gh));
    };
    // Unlike the shim, non-PR calls bypass this read-only probe: only PR calls use
    // its result, so delaying it preserves gh arguments while avoiding needless work.
    // `--ignore-working-copy`: a snapshot would take the repository-wide jj lock.
    let in_jj_repo = Command::new("jj")
        .current_dir(&cwd)
        .args(["root", "--ignore-working-copy"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !in_jj_repo {
        gh.args(args);
        std::process::exit(gh_exit_code(&mut gh));
    }

    let bookmark = current_bookmark(&cwd);
    let arguments = match subcommand.as_str() {
        "create" if !has_head_flag(args) => {
            let Some(bookmark) = bookmark.as_deref() else {
                die_no_bookmark();
            };
            let mut arguments = args.to_vec();
            arguments.push("--head".to_owned());
            arguments.push(bookmark.to_owned());
            arguments
        }
        "view" | "checks" | "diff" | "merge" | "checkout" | "edit" | "comment" | "ready"
        | "review" | "update-branch"
            if !has_positional_target(&subcommand, args) =>
        {
            let Some(bookmark) = bookmark.as_deref() else {
                die_no_bookmark();
            };
            inject_positional(args, &subcommand, bookmark)
        }
        _ => args.to_vec(),
    };
    let exit_code = {
        let wrapper = tempfile::tempdir()?;
        std::fs::set_permissions(wrapper.path(), std::fs::Permissions::from_mode(0o700))?;
        let git = wrapper.path().join("git");
        std::fs::write(&git, GIT_WRAPPER)?;
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755))?;
        let mut path = wrapper.path().as_os_str().to_os_string();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        gh.args(&arguments).env("PATH", path).env(
            "_JJ_BOOKMARK",
            bookmark.as_deref().unwrap_or(DETACHED_BOOKMARK),
        );
        gh_exit_code(&mut gh)
    };
    std::process::exit(exit_code);
}

/// Waits for gh while preserving its interactive terminal ownership and exit code.
fn gh_exit_code(gh: &mut Command) -> i32 {
    gh.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_or_else(
            |_| {
                // This maps every spawn failure to 127; unlike bash, it does not
                // distinguish an inaccessible executable with exit code 126.
                eprintln!("knives gh: real gh not found");
                127
            },
            exit_code,
        )
}

/// The shell's view of a child's status: its exit code, or 128 + the signal that killed it.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

fn die_no_bookmark() -> ! {
    eprintln!("Error: No jj bookmark at current change (@)");
    eprintln!();
    eprintln!("Create one with:");
    eprintln!("  jj bookmark create <name>");
    eprintln!();
    eprintln!("Or push and create in one step:");
    eprintln!("  jj git push --named=<name>=@");
    std::process::exit(1);
}

/// The first safe bookmark on `@`, if jj can provide one (shim lines 225-237).
/// Deliberately unlike the shim, whose character class is locale-dependent and accepts
/// non-ASCII under UTF-8 locales: ASCII-only matches the charset the shim comment intends.
///
/// `--ignore-working-copy`: bookmarks ride on `@` through a snapshot, so the answer
/// is the same without one, and a snapshot would take the repository-wide jj lock.
/// A `git checkout` made behind jj's back is not seen until the next jj command.
pub(crate) fn current_bookmark(cwd: &Path) -> Option<String> {
    let output = Command::new("jj")
        .current_dir(cwd)
        .args([
            "--ignore-working-copy",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "self.bookmarks()",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bookmark = std::str::from_utf8(&output.stdout)
        .ok()?
        .split_whitespace()
        .next()?;
    bookmark
        .chars()
        .all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | '-')
        })
        .then(|| bookmark.to_owned())
}

/// The `gh pr <subcommand>` this invocation is, skipping flags (shim lines 308-329).
pub(crate) fn pr_subcommand(args: &[String]) -> Option<(String, usize)> {
    let mut found_pr = false;
    for (index, argument) in args.iter().enumerate() {
        if found_pr {
            if !argument.starts_with('-') {
                return Some((argument.clone(), index));
            }
        } else if argument == "pr" {
            found_pr = true;
        }
    }
    None
}

/// Whether a positional target follows `subcommand` (shim lines 343-384).
pub(crate) fn has_positional_target(subcommand: &str, args: &[String]) -> bool {
    let mut found_subcommand = false;
    let mut previous_was_value_flag = false;
    for argument in args {
        if previous_was_value_flag {
            previous_was_value_flag = false;
            continue;
        }
        if found_subcommand {
            if argument.starts_with('-') {
                if !argument.contains('=')
                    && matches!(
                        argument.as_str(),
                        "-R" | "--repo"
                            | "-q"
                            | "--jq"
                            | "-t"
                            | "--template"
                            | "--json"
                            | "-b"
                            | "--body"
                            | "-F"
                            | "--body-file"
                            | "--branch"
                            | "-c"
                            | "--comment"
                            | "-r"
                            | "--reason"
                            | "--color"
                            | "-i"
                            | "--interval"
                            | "--subject"
                            | "--match-head-commit"
                            | "--author-email"
                            | "-A"
                            | "-l"
                            | "--label"
                            | "-m"
                            | "--milestone"
                            | "-p"
                            | "--project"
                            | "--reviewer"
                            | "--assignee"
                            | "-T"
                            | "--title"
                            | "--recover"
                    )
                {
                    previous_was_value_flag = true;
                }
                continue;
            }
            return true;
        }
        if argument == subcommand {
            found_subcommand = true;
        }
    }
    false
}

/// Whether the caller already supplied `--head` (shim lines 332-339).
pub(crate) fn has_head_flag(args: &[String]) -> bool {
    args.iter()
        .any(|argument| argument == "--head" || argument.starts_with("--head="))
}

/// Inserts `bookmark` directly after the first `subcommand` (shim lines 388-403).
pub(crate) fn inject_positional(args: &[String], subcommand: &str, bookmark: &str) -> Vec<String> {
    let mut injected = Vec::with_capacity(args.len() + 1);
    let mut inserted = false;
    for argument in args {
        injected.push(argument.clone());
        if !inserted && argument == subcommand {
            injected.push(bookmark.to_owned());
            inserted = true;
        }
    }
    injected
}

/// The caller's explicit real-gh choice: `KNIVES_REAL_GH` when set and non-empty.
/// The shim sets it on every re-entry; a hand-set value chooses the binary and nothing
/// more (whether this is the shim's inner pass is [`reentry_shim`]'s question).
fn real_gh_override() -> Option<PathBuf> {
    std::env::var_os("KNIVES_REAL_GH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Finds the real `gh`, never returning a marker-bearing shim (shim lines 206-215).
///
/// The `KNIVES_REAL_GH` override is trusted only while it is not provably the
/// shim: a poisoned override pointing back at the shim sustained an unbounded
/// knives<->shim fork chain (2026-08-27, ~300k processes). An unreadable
/// override stays trusted — the spawn fails loudly with exit 127 — because the
/// override never promised to be scannable, only to be the caller's choice.
pub(crate) fn real_gh() -> anyhow::Result<PathBuf> {
    if let Some(candidate) = real_gh_override()
        && shim_marker(&candidate) != Some(true)
    {
        return Ok(candidate);
    }
    first_executable_gh(|candidate| shim_marker(candidate) == Some(false))
        .ok_or_else(|| anyhow::anyhow!("real gh not found"))
}

/// The shim to hand a direct `knives gh` invocation back to: the first executable `gh`
/// on PATH when it carries the shim marker and `KNIVES_GH_SHIM_DEPTH` is unset.
///
/// The shim is what applies the agent-context rules (no keyring login, the routing
/// include) before re-entering knives; a direct invocation from an environment that lost
/// `GIT_CONFIG_*` would otherwise leave gh on the user's own login (measured 2026-09-14:
/// `knives gh -- api user` from an agent session answered the user). The shim sets the
/// depth marker on every pass before re-entering knives, so its presence is the inner
/// pass and there is nothing to hand back. `KNIVES_REAL_GH` does not decide: it is only
/// the binary choice, and a hand-set override with the shim first on PATH still
/// re-enters, so the override is not a way around the shim (a depth marker set but
/// empty is treated as unset for the same reason). With no shim first on PATH nothing
/// is re-entered.
pub(crate) fn reentry_shim() -> Option<PathBuf> {
    if std::env::var_os("KNIVES_GH_SHIM_DEPTH").is_some_and(|depth| !depth.is_empty()) {
        return None;
    }
    let first = first_executable_gh(|_| true)?;
    (shim_marker(&first) == Some(true)).then_some(first)
}

/// The first executable regular file named `gh` on PATH that `accept` takes.
fn first_executable_gh(accept: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join("gh"))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }) && accept(candidate)
        })
}

/// Whether the file's first 512 bytes carry the shim marker; None when the
/// path is not a readable regular file. Sniffing only regular files keeps a
/// FIFO or device override from blocking resolution on open; such a path
/// stays trusted and fails loudly at spawn instead. The sniff-to-spawn race
/// is accepted: exploiting it needs write access to the resolved path.
fn shim_marker(candidate: &Path) -> Option<bool> {
    let marker = b"knives-gh-shim";
    if !std::fs::metadata(candidate).ok()?.is_file() {
        return None;
    }
    // read_to_end, not one read(): a single read may legally return short and
    // miss a marker that sits later in the prefix.
    let mut prefix = Vec::with_capacity(512);
    std::fs::File::open(candidate)
        .and_then(|file| file.take(512).read_to_end(&mut prefix))
        .ok()?;
    Some(prefix.windows(marker.len()).any(|window| window == marker))
}

/// Normalize a remote URL to https form with a trailing .git (shim lines 44-57).
pub(crate) fn normalize_url(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let mut url = url.to_owned();
    // SSH scp-form git@host:owner/repo -> https://host/owner/repo
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
        && !host.is_empty()
        && !path.is_empty()
    {
        url = format!("https://{host}/{path}");
    }
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        url = format!("https://{rest}");
    }
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "The canonical remote suffix is the literal lowercase .git."
    )]
    if !url.ends_with(".git") {
        url.push_str(".git");
    }
    Some(url)
}

/// An https URL from a gh repo spec: URL, host/owner/repo, or owner/repo (lines 59-73).
pub(crate) fn url_from_spec(spec: &str) -> Option<String> {
    if spec.is_empty() {
        return None;
    }
    if spec.contains("://") {
        return normalize_url(spec);
    }
    let slashes = spec.matches('/').count();
    if slashes >= 2 {
        normalize_url(&format!("https://{spec}"))
    } else {
        normalize_url(&format!("https://{DEFAULT_HOST}/{spec}"))
    }
}

/// The `path` part a credential request wants: everything after the host (line 192).
/// Unlike the shim, malformed or pathless URLs return None instead of a nonsensical path.
pub(crate) fn credential_path(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let (_, path) = rest.split_once('/')?;
    Some(path)
}

/// The credential helper's answer for one target.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mint {
    /// The helper answered with this token; gh runs with it.
    Token(String),
    /// No helper is routed for the host, or it answered without a password: gh runs
    /// on its own auth.
    Unrouted,
    /// The helper refused — a non-zero exit, a `quit=1` answer, an answer that is not
    /// UTF-8, or it could not run — and has already said why on stderr: knives exits
    /// with this code instead of running gh.
    Refused(i32),
}

/// `gh auth …` is about the user's own login, never a routed App: `gh auth status` and
/// `gh auth token` report who the user is. Minting here would make `gh auth status`
/// report the cwd repo's App as a login. In an agent session the shim has already
/// refused every `gh auth` verb but `status` before knives runs; this keeps the one
/// that reaches knives honest.
pub(crate) fn is_auth_command(args: &[String]) -> bool {
    args.first().map(String::as_str) == Some("auth")
}

/// Asks git's routed credential helper for the token gh should run with.
///
/// Reads git's own credential config and speaks the credential-helper protocol
/// to gh-app-token, which routes by the request path (shim lines 181-204).
/// The routing table stays in gh-app-routes.gitconfig — single source of truth for
/// git and knives alike; knives reads, never owns. The helper answers every
/// owner-bearing request on its own or quits: an owner the App covers gets an
/// installation token, any other owner the profile's fallback secret, and an
/// owner it cannot serve a `quit=1` with the reason on stderr. That stderr is
/// relayed as knives' own, and a refusal stops knives before gh runs: the
/// alternative, gh on its own auth, is a "run gh auth login" hint in an agent
/// session and the user's keyring login in an interactive one.
///
/// The exit status is checked and there is an empty-password guard: trusting a
/// failed process's output is wrong even while today's helper never prints a
/// password on a failing path. Only an exit-0 UTF-8 answer with no password is
/// [`Mint::Unrouted`], like a host no helper is configured for; an exit-0 answer
/// knives cannot read is a refusal, not a fall-through to gh's own auth.
pub(crate) fn mint_token(target_url: &str) -> Mint {
    let Some(path) = credential_path(target_url) else {
        return Mint::Unrouted;
    };
    let helper_key = format!("credential.https://{DEFAULT_HOST}/.helper");
    // NO .current_dir()/ -C on purpose: this is config, not repo state; unlike
    // gh_resolved_remote, adding a cwd would break the GIT_CONFIG_GLOBAL test override.
    let Ok(helpers) = Command::new("git")
        .args(["config", "--get-all"])
        .arg(helper_key)
        .output()
    else {
        return Mint::Unrouted;
    };
    let Some(profile) = std::str::from_utf8(&helpers.stdout)
        .ok()
        .filter(|_| helpers.status.success())
        .and_then(|helpers| {
            helpers
                .lines()
                .find_map(|helper| helper.strip_prefix("!gh-app-token "))
        })
    else {
        return Mint::Unrouted;
    };
    let request = format!("protocol=https\nhost={DEFAULT_HOST}\npath={path}\n\n");
    let mut child = match Command::new("gh-app-token")
        .args([profile, "get"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("knives gh: cannot run gh-app-token {profile}: {error}");
            return Mint::Refused(127);
        }
    };
    // A helper that decides before reading its request closes stdin early; its
    // answer, not the broken pipe, is what counts.
    if let Some(mut input) = child.stdin.take() {
        let _ = std::io::Write::write_all(&mut input, request.as_bytes());
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!("knives gh: gh-app-token {profile}: {error}");
            return Mint::Refused(1);
        }
    };
    if !output.status.success() {
        return Mint::Refused(exit_code(output.status));
    }
    let Ok(answer) = std::str::from_utf8(&output.stdout) else {
        eprintln!("knives gh: gh-app-token {profile} answered non-UTF-8");
        return Mint::Refused(1);
    };
    if answer
        .lines()
        .any(|line| line == "quit=1" || line == "quit=true")
    {
        return Mint::Refused(1);
    }
    answer
        .lines()
        .find_map(|line| line.strip_prefix("password=").map(str::to_owned))
        .filter(|token| !token.is_empty())
        .map_or(Mint::Unrouted, Mint::Token)
}

/// The owner a `gh api` invocation targets, or None when it carries no signal.
///
/// These invocations often run outside the target repo's checkout and carry no
/// -R, so remote-based resolution would route the token to the wrong owner —
/// that is the failure this exists for (shim lines 80-86). Pure node-id
/// GraphQL mutations genuinely have no signal; `gh-app-token` honors
/// `GH_APP_OWNER` for those, which is out of knives' hands.
pub(crate) fn owner_from_api_args(args: &[String]) -> Option<String> {
    if args.first().map(String::as_str) != Some("api") {
        return None;
    }
    for argument in args.iter().skip(1) {
        if argument.starts_with('-') {
            continue;
        }
        let bare = argument.strip_prefix('/').unwrap_or(argument);
        for prefix in ["repos/", "orgs/", "users/"] {
            if let Some(rest) = bare.strip_prefix(prefix) {
                let owner = rest.split('/').next().unwrap_or("");
                let owner = owner.split('?').next().unwrap_or("");
                if !owner.is_empty() && !owner.contains('{') {
                    return Some(owner.to_owned());
                }
                return None;
            }
        }
    }
    // Deliberate, unreachable divergences: the shim's independent `['"]` classes
    // accept `owner:"acme'`, while we require matching quotes (mismatched quotes are
    // invalid GraphQL); its sequential prefix stripping maps `repos/orgs/foo` to
    // `foo`, while we yield `orgs` (those apparent path owners are GitHub-reserved).
    let joined = args.join(" ");
    // LEFTMOST match wins across BOTH patterns — the shim's single alternation
    // regex returns the first match in the text, so a query naming
    // organization(login:"a") before repository(owner:"b") routes to "a".
    // Checking one keyword fully before the other would invert that.
    let candidates = [("repository", "owner"), ("organization", "login")]
        .into_iter()
        .filter_map(|(keyword, field)| graphql_field(&joined, keyword, field))
        .min_by_key(|(offset, _)| *offset);
    match candidates {
        Some((_, GraphqlValue::Literal(owner))) => Some(owner),
        // The query names the owner through a variable; its value travels as a
        // separate `-f owner=acme` field argument (how knives' own forge queries
        // and gh's documentation write it).
        Some((_, GraphqlValue::Variable(name))) => field_argument(args, &name),
        None => None,
    }
}

/// The value bound to GraphQL variable `name` by a `gh api` field argument, in
/// every spelling gh's flag parser accepts: `-f name=value`, `-fname=value`,
/// `-f=name=value`, the same three for `-F`, and `--raw-field`/`--field` as
/// `--flag name=value` or `--flag=name=value`. A value gh reads from a file
/// (`-F name=@path`) or that is not shaped like a login is no signal; `-F`
/// coercions such as `true` or `42` pass the charset and route like the login
/// they spell, which is what `-f` would have sent anyway.
fn field_argument(args: &[String], name: &str) -> Option<String> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        let assignment = match argument.as_str() {
            "-f" | "-F" | "--raw-field" | "--field" => {
                index += 1;
                args.get(index).map(String::as_str)
            }
            _ => argument
                .strip_prefix("--raw-field=")
                .or_else(|| argument.strip_prefix("--field="))
                .or_else(|| argument.strip_prefix("-f"))
                .or_else(|| argument.strip_prefix("-F"))
                .map(|attached| attached.strip_prefix('=').unwrap_or(attached)),
        };
        if let Some((key, value)) = assignment.and_then(|assignment| assignment.split_once('='))
            && key == name
            && is_owner_shaped(value)
        {
            return Some(value.to_owned());
        }
        index += 1;
    }
    None
}

/// The characters a GitHub login or organization name may carry.
fn is_owner_shaped(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The repository a command targets when its arguments carry no `-R`: gh's own
/// `GH_REPO` override, applied whenever it is set and `-R` is absent, inside a
/// checkout or not (shim parity has no equivalent; the shim predates scripts
/// that target by environment).
fn gh_repo_environment() -> Option<String> {
    std::env::var("GH_REPO")
        .ok()
        .filter(|spec| !spec.is_empty())
}

/// The value of the first explicit repository flag (shim lines 137-150).
pub(crate) fn repo_flag(args: &[String]) -> Option<String> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        match argument.as_str() {
            "-R" | "--repo" => {
                // A missing value means this flag is necessarily last, so ending
                // the scan is equivalent to the shim falling through.
                return args.get(index + 1).cloned();
            }
            _ => {
                if let Some(repo) = argument
                    .strip_prefix("--repo=")
                    .or_else(|| argument.strip_prefix("-R="))
                {
                    return Some(repo.to_owned());
                }
            }
        }
        index += 1;
    }
    None
}

/// The https URL targeted by this invocation (shim lines 75-179).
pub(crate) fn resolve_target_url(args: &[String], cwd: &Path) -> Option<String> {
    let api_owner = owner_from_api_args(args);
    let repo_spec = repo_flag(args).or_else(gh_repo_environment);
    let needs_git_inputs = api_owner.is_none() && repo_spec.is_none();
    let resolved_remote = needs_git_inputs.then(|| gh_resolved_remote(cwd)).flatten();
    let registry = needs_git_inputs
        .then(|| crate::config::load(&crate::config::default_config_path()).ok())
        .flatten();
    let bound = registry
        .as_ref()
        .and_then(|registry| crate::bind::here(registry, cwd).ok());
    let registered_entry = bound.as_ref().map(|fork| fork.entry);
    let requires_remotes = needs_git_inputs
        && (resolved_remote
            .as_ref()
            .is_some_and(|resolved| resolved.value == "base")
            || registered_entry.is_none());
    let remotes = if requires_remotes {
        bound
            .as_ref()
            .map(|fork| fork.checkout.remotes.clone())
            .or_else(|| {
                crate::bind::checkout_root(cwd).and_then(|root| crate::bind::remotes(&root).ok())
            })
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };

    resolve_from_inputs(TargetInputs {
        api_owner: api_owner.as_deref(),
        repo_spec: repo_spec.as_deref(),
        resolved_remote: resolved_remote.as_ref().map(|resolved| ResolvedRemote {
            name: &resolved.name,
            value: &resolved.value,
        }),
        registered_entry,
        remotes: &remotes,
    })
}

/// A `gh-resolved` marker owned after parsing git config at the process boundary.
struct OwnedResolvedRemote {
    name: String,
    value: String,
}

/// A borrowed `gh-resolved` marker supplied to the pure target resolver.
struct ResolvedRemote<'a> {
    name: &'a str,
    value: &'a str,
}

/// The borrowed candidates for the pure target-resolution seam.
struct TargetInputs<'a> {
    api_owner: Option<&'a str>,
    repo_spec: Option<&'a str>,
    resolved_remote: Option<ResolvedRemote<'a>>,
    registered_entry: Option<&'a crate::config::RepoEntry>,
    remotes: &'a BTreeMap<String, String>,
}

/// Resolves steps 0–3 in shim order: API owner, explicit repo, marker, then remotes.
///
/// Steps 0–2 are terminal when their inputs exist; only their absence advances to the
/// next step (shim lines 123-178).
fn resolve_from_inputs(inputs: TargetInputs<'_>) -> Option<String> {
    if let Some(owner) = inputs.api_owner {
        return url_from_spec(&format!("{owner}/gh-api"));
    }
    if let Some(spec) = inputs.repo_spec {
        return url_from_spec(spec);
    }
    if let Some(resolved) = inputs.resolved_remote {
        return if resolved.value == "base" {
            inputs
                .remotes
                .get(resolved.name)
                .and_then(|url| normalize_url(url))
        } else {
            url_from_spec(resolved.value)
        };
    }
    preferred_remote_url(inputs.registered_entry, inputs.remotes)
}

/// The first `gh repo set-default` marker, if git reports one (shim lines 151-164).
fn gh_resolved_remote(cwd: &Path) -> Option<OwnedResolvedRemote> {
    // `bind::git` forbids discovery above the directory it is given, so it
    // must be handed the repository root, not a subdirectory of it.
    let root = crate::bind::checkout_root(cwd)?;
    let output = crate::bind::git(&root)
        .args(["config", "--get-regexp", "^remote\\..*\\.gh-resolved$"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = std::str::from_utf8(&output.stdout).ok()?.lines().next()?;
    // Unlike the shim, a valueless marker remains terminal with an empty target, so it mints no token instead of passing a garbage URL onward.
    let (key_with_value, value) = line.rsplit_once(char::is_whitespace).unwrap_or((line, ""));
    let (key, _) = key_with_value.rsplit_once(".gh-resolved")?;
    let name = key.strip_prefix("remote.")?;
    Some(OwnedResolvedRemote {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// Prefers configured fork roles before the shim's ordered remote fallback (shim lines 165-178).
///
/// Role-first selection is our intentional divergence: the shim has no registry concept,
/// and a registered fork with nonstandard remote names would otherwise misroute.
fn preferred_remote_url(
    registered_entry: Option<&crate::config::RepoEntry>,
    remotes: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(entry) = registered_entry {
        return [crate::config::Role::Upstream, crate::config::Role::Origin]
            .into_iter()
            .find_map(|role| normalize_url(entry.remote(role)));
    }

    ["upstream", "github", "origin"]
        .into_iter()
        .find_map(|name| remotes.get(name).and_then(|url| normalize_url(url)))
        .or_else(|| {
            remotes
                .iter()
                .filter(|(name, _)| !matches!(name.as_str(), "upstream" | "github" | "origin"))
                .find_map(|(_, url)| normalize_url(url))
        })
}

/// How a GraphQL query names an owner: inline, or through a variable whose value
/// arrives as a separate field argument.
enum GraphqlValue {
    Literal(String),
    Variable(String),
}

/// First `keyword ( ... field : "value" ... )` or `keyword ( ... field : $var ... )`
/// in the text, quote-agnostic, returned with the byte offset of the match so
/// callers can pick the leftmost across several keywords (shim parity: one
/// regex, first match wins). Shim line 113 requires the field to be first inside
/// the parentheses.
fn graphql_field(text: &str, keyword: &str, field: &str) -> Option<(usize, GraphqlValue)> {
    let mut search = text;
    let mut consumed = 0usize; // byte offset of `search` within `text`
    while let Some(at) = search.find(keyword) {
        let match_offset = consumed + at;
        let after = &search[at + keyword.len()..];
        let after_ws = after.trim_start();
        if let Some(body) = after_ws.strip_prefix('(') {
            // Shim line 113 requires the field first, preventing a later field
            // from minting a token for the wrong owner.
            if let Some(rest) = body.trim_start().strip_prefix(field) {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix(':') {
                    let rest = rest.trim_start();
                    if let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) {
                        let value: String =
                            rest.chars().skip(1).take_while(|c| *c != quote).collect();
                        if is_owner_shaped(&value) {
                            return Some((match_offset, GraphqlValue::Literal(value)));
                        }
                    } else if let Some(variable) = rest.strip_prefix('$') {
                        let name: String = variable
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .collect();
                        if !name.is_empty() {
                            return Some((match_offset, GraphqlValue::Variable(name)));
                        }
                    }
                }
            }
        }
        consumed += at + keyword.len();
        search = &search[at + keyword.len()..];
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;
    use std::collections::BTreeMap;

    fn with_env<T>(vars: &[(&'static str, &str)], run: impl FnOnce() -> T) -> T {
        let _lock = crate::config::test_support::environment_lock();
        let names: Vec<&'static str> = vars.iter().map(|(name, _)| *name).collect();
        let guard = crate::config::test_support::EnvironmentGuard::capture(&names);
        for (name, value) in vars {
            guard.set(name, value);
        }
        run()
        // `guard` restores every captured variable when it drops.
    }

    /// [`with_env`] with `PATH` prefixed by `directory`. The prefix is applied
    /// under the lock: another test may hold `PATH` at a scratch value while
    /// this one prepares, and a `PATH` read outside the lock would carry that
    /// value in — and lose `git`.
    fn with_path_prefix<T>(
        directory: &Path,
        vars: &[(&'static str, &str)],
        run: impl FnOnce() -> T,
    ) -> T {
        let _lock = crate::config::test_support::environment_lock();
        let mut names: Vec<&'static str> = vars.iter().map(|(name, _)| *name).collect();
        names.push("PATH");
        let guard = crate::config::test_support::EnvironmentGuard::capture(&names);
        let path = format!(
            "{}:{}",
            directory.display(),
            std::env::var("PATH").expect("PATH")
        );
        guard.set("PATH", &path);
        for (name, value) in vars {
            guard.set(name, value);
        }
        run()
    }

    #[test]
    fn remote_urls_normalize_to_https_with_a_git_suffix() {
        // The shim's stable matching form: https, trailing .git (lines 44-57).
        let host = concat!("github", ".com");
        assert_eq!(
            normalize_url(&format!("git@{host}:acme/work.git")).unwrap(),
            format!("https://{host}/acme/work.git")
        );
        assert_eq!(
            normalize_url(&format!("git@{host}:acme/work")).unwrap(),
            format!("https://{host}/acme/work.git")
        );
        assert_eq!(
            normalize_url(&format!("ssh://git@{host}/acme/work.git")).unwrap(),
            format!("https://{host}/acme/work.git")
        );
        assert_eq!(
            normalize_url(&format!("https://{host}/acme/work")).unwrap(),
            format!("https://{host}/acme/work.git")
        );
        assert_eq!(
            normalize_url(&format!("git@{host}:")).unwrap(),
            format!("git@{host}:.git")
        );
        assert_eq!(
            normalize_url("git@:acme/work").unwrap(),
            "git@:acme/work.git"
        );
        assert_eq!(
            normalize_url(&format!("https://{host}/acme/work.GIT")).unwrap(),
            format!("https://{host}/acme/work.GIT.git")
        );
        assert_eq!(normalize_url(""), None);
    }

    #[test]
    fn a_repo_spec_becomes_a_url_whatever_its_shape() {
        let host = concat!("github", ".com");
        // owner/repo defaults onto the default host (line 71).
        assert_eq!(
            url_from_spec("acme/work").unwrap(),
            format!("https://{host}/acme/work.git")
        );
        // host/owner/repo keeps its host (line 69).
        assert_eq!(
            url_from_spec("forge.example/acme/work").unwrap(),
            "https://forge.example/acme/work.git"
        );
        assert_eq!(
            url_from_spec("forge.example/acme/work/extra").unwrap(),
            "https://forge.example/acme/work/extra.git"
        );
        // A full URL passes through normalization (line 64).
        assert_eq!(
            url_from_spec(&format!("https://{host}/acme/work.git")).unwrap(),
            format!("https://{host}/acme/work.git")
        );
        assert_eq!(url_from_spec(""), None);
    }

    #[test]
    fn the_credential_path_is_everything_after_the_host() {
        let host = concat!("github", ".com");
        assert_eq!(
            credential_path(&format!("https://{host}/acme/work.git")).unwrap(),
            "acme/work.git"
        );
        assert_eq!(credential_path("not a url"), None);
        assert_eq!(credential_path(&format!("https://{host}")), None);
        assert_eq!(credential_path("http://forge.example/acme/work.git"), None);
    }

    /// Runs [`mint_token`] for `acme/work` with git routing the default host to a
    /// fake `gh-app-token` whose body is `script`; the scratch dir is returned so
    /// a test can read what the fake recorded.
    fn mint_through(script: &str) -> (tempfile::TempDir, Mint) {
        let dir = tempfile::tempdir().expect("scratch");
        let fake = dir.path().join("gh-app-token");
        std::fs::write(&fake, script).expect("write fake helper");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let gitconfig = dir.path().join("gitconfig");
        let host = concat!("github", ".com");
        std::fs::write(
            &gitconfig,
            format!("[credential \"https://{host}/\"]\n\thelper = !gh-app-token acme\n"),
        )
        .expect("write gitconfig");
        let gitconfig_path = gitconfig.display().to_string();
        let mint = with_path_prefix(
            dir.path(),
            &[
                ("GIT_CONFIG_GLOBAL", &gitconfig_path),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_COUNT", "0"),
            ],
            || mint_token(&format!("https://{host}/acme/work.git")),
        );
        (dir, mint)
    }

    #[test]
    fn a_routed_target_gets_a_minted_token() {
        // Given: a fake gh-app-token that echoes a password when fed a path.
        // When: minting for a target under the routed host.
        let (dir, mint) = mint_through(
            "#!/bin/sh\ncat > \"$0.request\"\nprintf 'username=x-access-token\\npassword=tok-%s\\n' \"$1\"\n",
        );

        // Then: the token comes back and the helper saw the credential request.
        assert_eq!(mint, Mint::Token("tok-acme".to_owned()));
        let request = std::fs::read_to_string(dir.path().join("gh-app-token.request"))
            .expect("request captured");
        assert!(request.contains("path=acme/work.git"), "{request}");
    }

    #[test]
    fn a_routed_target_with_an_empty_password_leaves_gh_on_its_own_auth() {
        // Given: a routed helper that responds with an empty password and exit 0.
        // When: the helper provides no token value.
        let (_dir, mint) = mint_through("#!/bin/sh\ncat > /dev/null\nprintf 'password=\\n'\n");

        // Then: gh runs on its own auth, as for a host no helper is configured for.
        assert_eq!(mint, Mint::Unrouted);
    }

    #[test]
    fn a_helper_that_exits_non_zero_refuses_with_its_code() {
        // Given: a routed helper that fails the way gh-app-token does for an owner it
        // cannot serve: quit=1 on stdout, the reason on stderr, exit 1. A password it
        // printed anyway must not be trusted.
        // When: minting.
        let (_dir, mint) = mint_through(
            "#!/bin/sh\ncat > /dev/null\necho 'gh-app-token: no App installation for owner acme' >&2\nprintf 'quit=1\\npassword=tok-stale\\n'\nexit 3\n",
        );

        // Then: the refusal carries the helper's exit code; knives must not run gh.
        assert_eq!(mint, Mint::Refused(3));
    }

    #[test]
    fn a_quit_answer_with_exit_zero_refuses() {
        // Given: a helper that answers quit=1 but exits 0, as git's protocol allows.
        // When: minting.
        let (_dir, mint) = mint_through("#!/bin/sh\ncat > /dev/null\nprintf 'quit=1\\n'\n");

        // Then: quit is a refusal, not a fall-through to gh's own auth.
        assert_eq!(mint, Mint::Refused(1));
    }

    #[test]
    fn a_non_utf8_answer_with_exit_zero_refuses() {
        // Given: a helper that exits 0 with a password knives cannot read as UTF-8.
        // When: minting.
        let (_dir, mint) =
            mint_through("#!/bin/sh\ncat > /dev/null\nprintf 'password=\\377\\376\\n'\n");

        // Then: an unreadable answer is a refusal, not a fall-through to gh's own auth.
        assert_eq!(mint, Mint::Refused(1));
    }

    #[test]
    fn a_helper_that_decides_before_reading_its_request_is_still_heard() {
        // Given: a helper that exits without consuming stdin (knives' write hits a
        // closed pipe).
        // When: minting.
        let (_dir, mint) = mint_through("#!/bin/sh\nexit 2\n");

        // Then: its exit status, not the broken pipe, decides the outcome.
        assert_eq!(mint, Mint::Refused(2));
    }

    #[test]
    fn an_unrouted_target_leaves_gh_on_its_own_auth() {
        // Given: Git has no credential helper for the default host.
        // GIT_CONFIG_GLOBAL/NOSYSTEM mask global+system, not a repo-local helper; this checkout has none.
        let dir = tempfile::tempdir().expect("scratch");
        let gitconfig = dir.path().join("gitconfig");
        std::fs::write(&gitconfig, "").expect("write gitconfig");
        let gitconfig_path = gitconfig.display().to_string();
        let host = concat!("github", ".com");

        // When: minting a token for the default host.
        let mint = with_env(
            &[
                ("GIT_CONFIG_GLOBAL", &gitconfig_path),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_COUNT", "0"),
            ],
            || mint_token(&format!("https://{host}/acme/work.git")),
        );

        // Then: nothing is routed, so gh runs on its own auth.
        assert_eq!(mint, Mint::Unrouted);
    }

    #[test]
    fn rest_paths_yield_their_owner_segment() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&args(&["api", "repos/acme/work/pulls"])).as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&args(&["api", "/orgs/acme/teams"])).as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&args(&["api", "users/someone"])).as_deref(),
            Some("someone")
        );
        // Query strings on bare segments are stripped (shim line 99).
        assert_eq!(
            owner_from_api_args(&args(&["api", "orgs/acme?page=2"])).as_deref(),
            Some("acme")
        );
        // Placeholders expand from the current repo: no owner signal (line 101).
        assert_eq!(
            owner_from_api_args(&args(&["api", "repos/{owner}/{repo}/pulls"])),
            None
        );
        // Flags are skipped while scanning for the path (line 92).
        assert_eq!(
            owner_from_api_args(&args(&["api", "-X", "POST", "repos/acme/work/issues"])).as_deref(),
            Some("acme")
        );
        assert_eq!(owner_from_api_args(&args(&["api", "repos/"])), None);
        assert_eq!(owner_from_api_args(&args(&["api", "orgs/?page=2"])), None);
        assert_eq!(
            owner_from_api_args(&args(&["pr", "list", "repos/acme/work"])),
            None
        );
    }

    #[test]
    fn the_first_path_shaped_argument_ends_the_rest_scan() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "repos/{owner}/{repo}",
                "-f",
                r#"query=query { repository(owner: "acme") { id } }"#,
            ])),
            None
        );
    }

    #[test]
    fn graphql_bodies_yield_their_first_owner() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(owner: "acme", name: "work") { id } }"#,
            ]))
            .as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                "query=query { organization(login: 'acme') { id } }",
            ]))
            .as_deref(),
            Some("acme")
        );
        // Pure node-id mutations carry no owner signal (line 84).
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                "query=mutation { addProjectV2ItemById(input: {}) { item { id } } }",
            ])),
            None
        );
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository (owner: "x") { id } }"#,
            ]))
            .as_deref(),
            Some("x")
        );
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(owner: "ac me") { id } }"#,
            ])),
            None
        );
    }

    #[test]
    fn the_leftmost_graphql_owner_wins_across_both_patterns() {
        // Shim parity: its single alternation regex takes the FIRST match in
        // the text, whichever pattern it is.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { organization(login: "first") { id } repository(owner: "second", name: "x") { id } }"#,
            ]))
            .as_deref(),
            Some("first")
        );
    }

    #[test]
    fn the_owner_must_be_the_first_field_inside_the_parens() {
        // Shim parity (line 113): `repository(name:.., owner:..)` does NOT match —
        // the field must come first — so the later organization(login:) wins.
        // Field-anywhere scanning minted a token for the WRONG owner here.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(name: "x", owner: "a") { id } organization(login: "b") { id } }"#,
            ]))
            .as_deref(),
            Some("b")
        );
    }

    #[test]
    fn a_variable_bound_graphql_owner_resolves_through_its_field_argument() {
        // knives' own forge queries bind the target as `-f owner=… -f name=…`
        // and reference `$owner` in the query, so the query text alone names no
        // owner. A run from a fork checkout asking about a *different* repo
        // (the consumer-head query) then routed by cwd to the fork's owner and
        // answered "Could not resolve to a Repository" for the private consumer.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let query = "query=query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id } }";
        for argv in [
            vec![
                "api",
                "graphql",
                "-f",
                "owner=acme",
                "-f",
                "name=work",
                "-f",
                query,
            ],
            vec!["api", "graphql", "-F", "owner=acme", "-f", query],
            vec!["api", "graphql", "--raw-field", "owner=acme", "-f", query],
            vec!["api", "graphql", "--raw-field=owner=acme", "-f", query],
            vec!["api", "graphql", "--field", "owner=acme", "-f", query],
            vec!["api", "graphql", "--field=owner=acme", "-f", query],
            // pflag's attached short spellings.
            vec!["api", "graphql", "-fowner=acme", "-f", query],
            vec!["api", "graphql", "-f=owner=acme", "-f", query],
            vec!["api", "graphql", "-Fowner=acme", "-f", query],
            // The query may precede its bindings.
            vec!["api", "graphql", "-f", query, "-f", "owner=acme"],
        ] {
            assert_eq!(
                owner_from_api_args(&args(&argv)).as_deref(),
                Some("acme"),
                "argv {argv:?}"
            );
        }
        // Variable names are matched whole: `$owner` is not bound by `owner_id=`.
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                "owner_id=acme",
                "-f",
                query
            ])),
            None
        );
        // An unbound variable is no signal; gh would reject the query anyway.
        assert_eq!(
            owner_from_api_args(&args(&["api", "graphql", "-f", query])),
            None
        );
        // A binding gh reads from a file or that is not an owner-shaped value is no signal.
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-F",
                "owner=@owner.txt",
                "-f",
                query
            ])),
            None
        );
        // Leftmost still wins when the earlier field is variable-bound.
        assert_eq!(
            owner_from_api_args(&args(&[
                "api",
                "graphql",
                "-f",
                "org=first",
                "-f",
                r#"query=query($org: String!) { organization(login: $org) { id } repository(owner: "second", name: "x") { id } }"#,
            ]))
            .as_deref(),
            Some("first")
        );
    }

    #[test]
    fn gh_repo_in_the_environment_targets_like_the_repo_flag() {
        // gh's own `GH_REPO` override selects the repository when the command is
        // not run inside its checkout; -R beats it, as in gh.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let host = DEFAULT_HOST;
        let scratch = tempfile::tempdir().expect("tempdir");
        with_env(&[("GH_REPO", "acme/work")], || {
            assert_eq!(
                resolve_target_url(&args(&["pr", "list"]), scratch.path()),
                Some(format!("https://{host}/acme/work.git"))
            );
            assert_eq!(
                resolve_target_url(
                    &args(&["api", "repos/{owner}/{repo}/pulls"]),
                    scratch.path()
                ),
                Some(format!("https://{host}/acme/work.git"))
            );
            assert_eq!(
                resolve_target_url(&args(&["pr", "list", "-R", "other/repo"]), scratch.path()),
                Some(format!("https://{host}/other/repo.git"))
            );
            // A path literal is the request's real target and still wins.
            assert_eq!(
                resolve_target_url(&args(&["api", "repos/literal/repo/pulls"]), scratch.path()),
                Some(format!("https://{host}/literal/gh-api.git"))
            );
        });
        with_env(&[("GH_REPO", "")], || {
            assert_eq!(
                resolve_target_url(&args(&["pr", "list"]), scratch.path()),
                None
            );
        });
    }

    #[test]
    fn the_repo_flag_is_found_in_all_four_spellings() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        for argv in [
            vec!["pr", "list", "-R", "acme/work"],
            vec!["pr", "list", "--repo", "acme/work"],
            vec!["pr", "list", "--repo=acme/work"],
            vec!["pr", "list", "-R=acme/work"],
        ] {
            assert_eq!(
                repo_flag(&args(&argv)).as_deref(),
                Some("acme/work"),
                "spelling {argv:?}"
            );
        }
        assert_eq!(repo_flag(&args(&["pr", "list"])), None);
        // A dangling -R with no value is not a target.
        assert_eq!(repo_flag(&args(&["pr", "list", "-R"])), None);
    }

    #[test]
    fn registry_roles_beat_literal_remote_names_during_target_resolution() {
        let host = DEFAULT_HOST;
        let entry = crate::config::RepoEntry::new(
            format!("git@{host}:registered/upstream"),
            format!("git@{host}:registered/origin"),
        );
        let remotes = BTreeMap::from([
            (
                "upstream".to_owned(),
                format!("https://{host}/wrong/remote"),
            ),
            (
                "legacy-primary".to_owned(),
                format!("https://{host}/also/wrong"),
            ),
        ]);

        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                repo_spec: None,
                resolved_remote: None,
                registered_entry: Some(&entry),
                remotes: &remotes,
            }),
            Some(format!("https://{host}/registered/upstream.git"))
        );
    }

    #[test]
    fn a_dangling_base_marker_does_not_fall_through_to_other_targets() {
        // Shim lines 151-164: a gh-resolved marker ends resolution even if its
        // named base remote can no longer produce a URL.
        let host = DEFAULT_HOST;
        let entry = crate::config::RepoEntry::new(
            format!("git@{host}:registered/upstream"),
            format!("git@{host}:registered/origin"),
        );
        let remotes = BTreeMap::from([(
            "origin".to_owned(),
            format!("https://{host}/fallback/repository"),
        )]);

        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                repo_spec: None,
                resolved_remote: Some(ResolvedRemote {
                    name: "missing",
                    value: "base",
                }),
                registered_entry: Some(&entry),
                remotes: &remotes,
            }),
            None
        );
    }

    #[test]
    fn an_empty_explicit_repo_spec_does_not_fall_through_to_remotes() {
        // Shim lines 137-150: --repo= invokes the URL conversion and returns,
        // even when its value is empty.
        let host = DEFAULT_HOST;
        let remotes = BTreeMap::from([(
            "origin".to_owned(),
            format!("https://{host}/fallback/repository"),
        )]);

        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                repo_spec: Some(""),
                resolved_remote: None,
                registered_entry: None,
                remotes: &remotes,
            }),
            None
        );
    }

    #[test]
    fn higher_priority_target_inputs_win_before_registry_and_remotes() {
        let host = DEFAULT_HOST;
        let remotes = BTreeMap::from([(
            "origin".to_owned(),
            format!("https://{host}/fallback/repository"),
        )]);

        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: Some("api-owner"),
                repo_spec: Some("explicit/repository"),
                resolved_remote: Some(ResolvedRemote {
                    name: "origin",
                    value: "configured/repository",
                }),
                registered_entry: None,
                remotes: &remotes,
            }),
            Some(format!("https://{host}/api-owner/gh-api.git"))
        );
    }

    #[test]
    fn configured_and_fallback_remotes_follow_the_shim_preference_order() {
        let host = DEFAULT_HOST;
        let remotes = BTreeMap::from([
            (
                "zebra".to_owned(),
                format!("https://{host}/zebra/repository"),
            ),
            (
                "github".to_owned(),
                format!("https://{host}/github/repository"),
            ),
            (
                "alpha".to_owned(),
                format!("https://{host}/alpha/repository"),
            ),
        ]);

        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                repo_spec: None,
                resolved_remote: Some(ResolvedRemote {
                    name: "zebra",
                    value: "base",
                }),
                registered_entry: None,
                remotes: &remotes,
            }),
            Some(format!("https://{host}/zebra/repository.git"))
        );
        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                repo_spec: None,
                resolved_remote: None,
                registered_entry: None,
                remotes: &remotes,
            }),
            Some(format!("https://{host}/github/repository.git"))
        );

        let remotes = BTreeMap::from([
            (
                "zebra".to_owned(),
                format!("https://{host}/zebra/repository"),
            ),
            (
                "alpha".to_owned(),
                format!("https://{host}/alpha/repository"),
            ),
        ]);
        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                repo_spec: None,
                resolved_remote: None,
                registered_entry: None,
                remotes: &remotes,
            }),
            Some(format!("https://{host}/alpha/repository.git"))
        );
    }

    #[test]
    fn the_pr_subcommand_is_found_past_intervening_flags() {
        // Given: gh arguments with and without a pull-request command.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };

        // When: locating the pull-request subcommand.
        let direct = pr_subcommand(&args(&["pr", "view", "123"]));
        let after_flag = pr_subcommand(&args(&["pr", "-R", "create"]));

        // Then: flags between `pr` and the subcommand are skipped (shim line 317).
        assert_eq!(
            direct.map(|(subcommand, _)| subcommand),
            Some("view".to_owned())
        );
        assert_eq!(
            after_flag.map(|(subcommand, _)| subcommand),
            Some("create".to_owned())
        );
        assert_eq!(pr_subcommand(&args(&["issue", "list"])), None);
        assert_eq!(pr_subcommand(&args(&["api", "repos/a/b"])), None);
    }

    #[test]
    fn positional_targets_are_detected_through_value_taking_flags() {
        // Given: pull-request view invocations with flags, values, and targets.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };

        // When: checking for a positional target after the subcommand.
        let only_repo_value =
            has_positional_target("view", &args(&["pr", "view", "--repo", "acme/work"]));
        let direct_target = has_positional_target("view", &args(&["pr", "view", "123"]));
        let target_after_repo =
            has_positional_target("view", &args(&["pr", "view", "--repo", "acme/work", "123"]));
        let inline_json = has_positional_target("view", &args(&["pr", "view", "--json=title"]));

        // Then: flag values do not become targets, including `--flag=value` forms.
        assert!(!only_repo_value);
        assert!(direct_target);
        assert!(target_after_repo);
        assert!(!inline_json);
    }

    #[test]
    fn head_flags_include_the_inline_value_form() {
        // Given: commands without a head, with a separate head, and with an inline head.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };

        // When: looking for the `--head` control flag.
        let absent = has_head_flag(&args(&["pr", "create"]));
        let separate = has_head_flag(&args(&["pr", "create", "--head", "feat/x"]));
        let inline = has_head_flag(&args(&["pr", "create", "--head=feat/x"]));

        // Then: both supported spellings prevent automatic injection.
        assert!(!absent);
        assert!(separate);
        assert!(inline);
    }

    #[test]
    fn the_bookmark_lands_directly_after_the_subcommand() {
        // Given: a view invocation whose flags follow the subcommand.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };

        // When: inserting the current bookmark as the default pull-request target.
        let injected =
            inject_positional(&args(&["pr", "view", "--json", "title"]), "view", "feat/x");

        // Then: the bookmark is the argument immediately after the subcommand.
        assert_eq!(injected, args(&["pr", "view", "feat/x", "--json", "title"]));
    }

    /// A PATH of two directories: a marked shim `gh` first, then a clean `gh`.
    fn shim_then_real_path(scratch: &Path) -> (String, PathBuf, PathBuf) {
        let shim_dir = scratch.join("shim");
        let real_dir = scratch.join("real");
        std::fs::create_dir(&shim_dir).expect("create shim directory");
        std::fs::create_dir(&real_dir).expect("create real directory");
        let shim = shim_dir.join("gh");
        let real = real_dir.join("gh");
        std::fs::write(&shim, "#!/bin/sh\n# knives-gh-shim\n").expect("write shim");
        std::fs::write(&real, "#!/bin/sh\n").expect("write real gh");
        for executable in [&shim, &real] {
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
                .expect("chmod executable");
        }
        let path = format!("{}:{}", shim_dir.display(), real_dir.display());
        (path, shim, real)
    }

    const REENTRY_ENV: [&str; 3] = ["KNIVES_GH_SHIM_DEPTH", "KNIVES_REAL_GH", "PATH"];

    #[test]
    fn a_direct_invocation_re_enters_the_shim_first_on_path() {
        // Given: no depth marker (knives was not invoked by the shim) and a PATH whose
        // first gh is the marked shim.
        let scratch = tempfile::tempdir().expect("scratch");
        let (path, shim, _) = shim_then_real_path(scratch.path());
        let _lock = crate::config::test_support::environment_lock();
        let guard = crate::config::test_support::EnvironmentGuard::capture(&REENTRY_ENV);
        guard.remove("KNIVES_GH_SHIM_DEPTH");
        guard.remove("KNIVES_REAL_GH");
        guard.set("PATH", &path);

        // When: deciding whether to hand the call back to the shim.
        let reentry = reentry_shim();

        // Then: the shim gets the call, so its agent-context rules apply before any minting.
        assert_eq!(reentry, Some(shim));
    }

    #[test]
    fn a_hand_set_override_without_the_depth_marker_still_re_enters() {
        // Given: KNIVES_REAL_GH set by hand (not by the shim, which also sets the depth
        // marker) and the same shim-first PATH.
        let scratch = tempfile::tempdir().expect("scratch");
        let (path, shim, real) = shim_then_real_path(scratch.path());
        let real_value = real.display().to_string();
        let _lock = crate::config::test_support::environment_lock();
        let guard = crate::config::test_support::EnvironmentGuard::capture(&REENTRY_ENV);
        guard.remove("KNIVES_GH_SHIM_DEPTH");
        guard.set("KNIVES_REAL_GH", &real_value);
        guard.set("PATH", &path);

        // When / Then: the override is not a way around the shim; it is re-entered.
        assert_eq!(reentry_shim(), Some(shim));
    }

    #[test]
    fn a_shim_re_entry_does_not_re_enter_again() {
        // Given: the depth marker and KNIVES_REAL_GH set, as the shim sets both on every
        // pass into knives, and the same shim-first PATH.
        let scratch = tempfile::tempdir().expect("scratch");
        let (path, _, real) = shim_then_real_path(scratch.path());
        let real_value = real.display().to_string();
        let _lock = crate::config::test_support::environment_lock();
        let guard = crate::config::test_support::EnvironmentGuard::capture(&REENTRY_ENV);
        guard.set("KNIVES_GH_SHIM_DEPTH", "1");
        guard.set("KNIVES_REAL_GH", &real_value);
        guard.set("PATH", &path);

        // When: deciding whether to re-enter.
        let reentry = reentry_shim();

        // Then: nothing to hand back; this pass mints and runs the real gh.
        assert_eq!(reentry, None);
        assert_eq!(real_gh().expect("use override"), real);
    }

    #[test]
    fn no_shim_on_path_means_no_re_entry() {
        // Given: no override and a PATH whose only gh is clean (a machine without the shim).
        let scratch = tempfile::tempdir().expect("scratch");
        let real = scratch.path().join("gh");
        std::fs::write(&real, "#!/bin/sh\n").expect("write real gh");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
            .expect("chmod real gh");
        let path = scratch.path().display().to_string();
        let _lock = crate::config::test_support::environment_lock();
        let guard = crate::config::test_support::EnvironmentGuard::capture(&REENTRY_ENV);
        guard.remove("KNIVES_GH_SHIM_DEPTH");
        guard.remove("KNIVES_REAL_GH");
        guard.set("PATH", &path);

        // When / Then: today's behaviour stands; PATH gh is the real gh.
        assert_eq!(reentry_shim(), None);
        assert_eq!(real_gh().expect("find real gh"), real);
    }

    #[test]
    fn real_gh_skips_a_path_candidate_marked_as_the_knives_shim() {
        // Given: a PATH where the first executable gh carries the shim marker.
        let scratch = tempfile::tempdir().expect("scratch");
        let (path, _, real) = shim_then_real_path(scratch.path());
        let _lock = crate::config::test_support::environment_lock();
        let guard =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_REAL_GH", "PATH"]);
        guard.remove("KNIVES_REAL_GH");
        guard.set("PATH", &path);

        // When: resolving the actual gh executable.
        let selected = real_gh().expect("find unmarked gh");

        // Then: recursion through the knives shim is impossible.
        assert_eq!(selected, real);
    }

    #[test]
    fn real_gh_uses_the_explicit_override_before_scanning_path() {
        // Given: an explicit real-gh override and a PATH containing only a marked shim.
        let scratch = tempfile::tempdir().expect("scratch");
        let shim = scratch.path().join("gh");
        let override_path = scratch.path().join("provided-gh");
        std::fs::write(&shim, "#!/bin/sh\n# knives-gh-shim\n").expect("write shim");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim");
        let path = scratch.path().display().to_string();
        let override_value = override_path.display().to_string();
        let _lock = crate::config::test_support::environment_lock();
        let guard =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_REAL_GH", "PATH"]);
        guard.set("KNIVES_REAL_GH", &override_value);
        guard.set("PATH", &path);

        // When: resolving the actual gh executable.
        let selected = real_gh().expect("use override");

        // Then: the shim does not need to be scanned or executable at the override path.
        assert_eq!(selected, override_path);
    }

    #[test]
    fn a_marker_bearing_override_is_rejected_in_favor_of_the_path_scan() {
        // Given: KNIVES_REAL_GH pointing at a marked shim (a mis-resolved
        // environment), and a PATH that holds a clean gh.
        let scratch = tempfile::tempdir().expect("scratch");
        let shim = scratch.path().join("gh-shim");
        let real_dir = scratch.path().join("real");
        std::fs::create_dir(&real_dir).expect("create real directory");
        let real = real_dir.join("gh");
        std::fs::write(&shim, "#!/bin/sh\n# knives-gh-shim\n").expect("write shim");
        std::fs::write(&real, "#!/bin/sh\n").expect("write real gh");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
            .expect("chmod real gh");
        let path = real_dir.display().to_string();
        let override_value = shim.display().to_string();
        let _lock = crate::config::test_support::environment_lock();
        let guard =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_REAL_GH", "PATH"]);
        guard.set("KNIVES_REAL_GH", &override_value);
        guard.set("PATH", &path);

        // When: resolving the actual gh executable.
        let selected = real_gh().expect("fall back to the scan");

        // Then: the poisoned override cannot re-enter the shim.
        assert_eq!(selected, real);
    }

    #[test]
    fn a_marker_bearing_override_with_no_clean_gh_errors() {
        // Given: a marked override and a PATH holding only another marked shim.
        let scratch = tempfile::tempdir().expect("scratch");
        let override_shim = scratch.path().join("gh-shim");
        let path_dir = scratch.path().join("shims");
        std::fs::create_dir(&path_dir).expect("create shim directory");
        let path_shim = path_dir.join("gh");
        std::fs::write(&override_shim, "#!/bin/sh\n# knives-gh-shim\n").expect("write override");
        std::fs::write(&path_shim, "#!/bin/sh\n# knives-gh-shim\n").expect("write path shim");
        std::fs::set_permissions(&path_shim, std::fs::Permissions::from_mode(0o755))
            .expect("chmod path shim");
        let path = path_dir.display().to_string();
        let override_value = override_shim.display().to_string();
        let _lock = crate::config::test_support::environment_lock();
        let guard =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_REAL_GH", "PATH"]);
        guard.set("KNIVES_REAL_GH", &override_value);
        guard.set("PATH", &path);

        // When: resolving the actual gh executable with nothing clean to fall back to.
        let selected = real_gh();

        // Then: a marker-bearing shim is never returned, even under failure pressure.
        assert!(selected.is_err(), "must not return a shim: {selected:?}");
    }
}
