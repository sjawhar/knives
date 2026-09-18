//! Fork-aware `gh` passthrough.
//!
//! This command executes `gh` directly, so the usual render/run split does not apply:
//! there is no knives result to render.
//!
//! Every question asked of the arguments — the repository a `pr create`
//! targets, the head it states, the endpoint a `gh api` call addresses — is a
//! lookup on the one [`GhInvocation`] built at the top of [`run`], read the way
//! cobra and pflag read gh's command line (see `gh_args`): flags anywhere after
//! the command, string flags last-wins, shorthand clusters expanded, gh's own
//! verb aliases normalised, a flag the tables do not define kept by name so
//! the gate refuses rather than guesses. Nothing here scans argv twice.
// allow: SIZE_OK: 2362 lines - single passthrough pipeline; splitting would separate resolution steps that read as one procedure.
use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::{
    fs::PermissionsExt as _,
    process::{CommandExt as _, ExitStatusExt as _},
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::gh_args::GhInvocation;

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
    let invocation = GhInvocation::parse(args);
    if is_auth_command(&invocation) {
        // Verbatim, and before the PR scanner: `gh auth token --hostname pr --user view`
        // would otherwise read as `gh pr view` and die on a bookmark it never needed.
        gh.args(args);
        std::process::exit(gh_exit_code(&mut gh));
    }
    let cwd = std::env::current_dir()?;
    // Before minting: an upstream pull request the placement verdict does not
    // allow is refused without spending a token on it.
    if let Some(refusal) = upstream_pull_refusal(&invocation, &cwd)? {
        eprintln!("knives gh: {refusal}");
        std::process::exit(crate::cli::Exit::Usage.code().into());
    }
    let token = if std::env::var_os("GH_TOKEN").is_some() {
        None
    } else {
        match resolve_target_url(&invocation, &cwd)
            .as_deref()
            .map(mint_token)
        {
            Some(Mint::Token(token)) => Some(token),
            Some(Mint::Refused(code)) => std::process::exit(code),
            Some(Mint::Unrouted) | None => None,
        }
    };
    if let Some(token) = token {
        gh.env("GH_TOKEN", token);
    }

    let Some((subcommand, verb_index)) = invocation.verb.clone() else {
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
        // A head the caller stated — in any spelling gh reads — is the head;
        // adding the current bookmark behind it would be the one gh honours.
        "create" if !invocation.has("head") => {
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
            if invocation.positionals.is_empty() =>
        {
            let Some(bookmark) = bookmark.as_deref() else {
                die_no_bookmark();
            };
            inject_positional(args, verb_index, bookmark)
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

/// Inserts `bookmark` directly after the verb at `verb_index` (shim lines 388-403).
pub(crate) fn inject_positional(args: &[String], verb_index: usize, bookmark: &str) -> Vec<String> {
    let mut injected = Vec::with_capacity(args.len() + 1);
    for (index, argument) in args.iter().enumerate() {
        injected.push(argument.clone());
        if index == verb_index {
            injected.push(bookmark.to_owned());
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
    // `https://host/owner/repo/` is the same repository to gh; without this the
    // suffix would make it `…/repo/.git`, which matches nothing.
    while url.ends_with('/') {
        url.pop();
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
pub(crate) fn is_auth_command(invocation: &GhInvocation) -> bool {
    invocation.command.as_deref() == Some("auth")
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
pub(crate) fn owner_from_api_args(invocation: &GhInvocation) -> Option<String> {
    if invocation.command.as_deref() != Some("api") {
        return None;
    }
    if let Some(endpoint) = api_endpoint(invocation) {
        // A repository addressed by numeric id names no owner: no token is
        // minted for it, as none is for a placeholder path.
        if endpoint.starts_with("repositories/") {
            return None;
        }
        for prefix in ["repos/", "orgs/", "users/"] {
            if let Some(rest) = endpoint.strip_prefix(prefix) {
                let owner = rest.split('/').next().unwrap_or("");
                if !owner.is_empty() && !is_api_placeholder(owner) {
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
    let joined = graphql_text(invocation);
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
        Some((_, GraphqlValue::Variable(name))) => invocation
            .fields(&name)
            .find(|value| is_owner_shaped(value))
            .map(str::to_owned),
        None => None,
    }
}

/// The GraphQL text a `gh api` call carries in its arguments: every field
/// value, in order (`-f query=…` is where a document travels). A document gh
/// reads from a file (`--input`, `-F query=@file`) is not opened.
fn graphql_text(invocation: &GhInvocation) -> String {
    invocation
        .flags
        .iter()
        .filter(|flag| matches!(flag.name, "raw-field" | "field"))
        .filter_map(|flag| flag.value.as_deref())
        .collect::<Vec<_>>()
        .join(" ")
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

/// The REST endpoint a `gh api` call addresses, as gh reads it: its first
/// positional, less a leading `/`, less an absolute `https://<host>/` gh sends
/// verbatim, and less its query string and any `#fragment` (which Go's client
/// never sends). A flag's value is never the endpoint, whatever it looks like.
fn api_endpoint(invocation: &GhInvocation) -> Option<&str> {
    let argument = invocation.positionals.first()?;
    let path = argument
        .strip_prefix("https://")
        .or_else(|| argument.strip_prefix("http://"))
        .and_then(|rest| rest.split_once('/'))
        .map_or(argument.as_str(), |(_, path)| path);
    let path = path.strip_prefix('/').unwrap_or(path);
    Some(path.split(['?', '#']).next().unwrap_or(path))
}

/// Whether a `gh api` call addresses a repository by GitHub's numeric id
/// (`repositories/<id>/…`), which names no owner knives can read.
fn names_repository_by_id(invocation: &GhInvocation) -> bool {
    invocation.command.as_deref() == Some("api")
        && api_endpoint(invocation).is_some_and(|endpoint| endpoint.starts_with("repositories/"))
}

/// The https URL targeted by this invocation (shim lines 75-179).
///
/// A `gh api repositories/<numeric id>/…` call names a repository knives
/// cannot map to an owner without asking GitHub, so it has no target here:
/// no token is minted for it, and the checkout's own remotes — the fallback a
/// path with no owner (`api user`) resolves through — are not consulted,
/// since the call is about whichever repository the id names, not this one.
pub(crate) fn resolve_target_url(invocation: &GhInvocation, cwd: &Path) -> Option<String> {
    if names_repository_by_id(invocation) {
        return None;
    }
    let api_owner = owner_from_api_args(invocation);
    let repo_spec = invocation
        .last("repo")
        .map(str::to_owned)
        .or_else(gh_repo_environment);
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

/// How an invocation would open a pull request, and on which head.
#[derive(Debug, PartialEq, Eq)]
enum PullOpening {
    /// `gh pr create` (or its alias `pr new`); every head `--head`/`-H` states,
    /// in order, as gh's parser reads them.
    Create { heads: Vec<String> },
    /// `gh api repos/{owner}/{repo}/pulls` with a body: REST creation. The
    /// owner and repo are as written, gh placeholders (`{owner}`) included.
    Rest {
        owner: String,
        repo: String,
        head: Option<String>,
    },
    /// A GraphQL `createPullRequest` mutation, which names its repository by
    /// node id and so carries no owner knives can read.
    Graphql { head: Option<String> },
    /// `gh api repositories/<numeric id>/pulls` with a body: the same REST
    /// creation addressed by GitHub's repository id, which names no owner.
    RestById { id: String },
}

/// Whether this invocation opens a pull request, and how (`None`: it does not).
///
/// The placement verdict governs opening a pull request upstream, so this is
/// deliberately narrow: `pr create`, the REST endpoint that creates one, and
/// the GraphQL mutation that does. Comments, reviews and edits on a pull
/// request that already exists are maintenance of work already open and pass
/// as before — `createPullRequestReview` is one of those, so the mutation name
/// is matched as a token, not a substring. A GraphQL document gh reads from a
/// file (`--input <file>`, `-f query=@file`) is not opened: only the arguments
/// are read.
fn pull_opening(invocation: &GhInvocation) -> Option<PullOpening> {
    match invocation.command.as_deref() {
        Some("pr") => {
            return (invocation.verb() == Some("create")).then(|| PullOpening::Create {
                heads: invocation
                    .values("head")
                    .map(|head| strip_head_owner(head).to_owned())
                    .collect(),
            });
        }
        Some("api") => {}
        _ => return None,
    }
    let endpoint = api_endpoint(invocation)?;
    if endpoint == "graphql" && names_mutation(&graphql_text(invocation), "createPullRequest") {
        return Some(PullOpening::Graphql {
            head: invocation
                .fields("headRefName")
                .find(|value| !value.is_empty())
                .map(str::to_owned),
        });
    }
    // A request with fields is a POST in gh's own default; `-X GET` on the
    // pulls endpoint lists them and opens nothing. The method is the last one
    // given, as gh keeps it, in any case.
    let creates = invocation.last("method").map_or_else(
        || {
            ["raw-field", "field", "input"]
                .iter()
                .any(|flag| invocation.has(flag))
        },
        |method| method.eq_ignore_ascii_case("POST"),
    );
    if !creates {
        return None;
    }
    // GitHub serves every repository endpoint under `repos/<owner>/<repo>/…`
    // and, by numeric id, `repositories/<id>/…`; both are read.
    if let Some(rest) = endpoint.strip_prefix("repositories/") {
        let mut segments = rest.split('/');
        let id = segments.next()?;
        return (segments.next() == Some("pulls") && segments.next().is_none())
            .then(|| PullOpening::RestById { id: id.to_owned() });
    }
    let mut segments = endpoint.strip_prefix("repos/")?.split('/');
    let (owner, repo) = (segments.next()?, segments.next()?);
    (segments.next() == Some("pulls") && segments.next().is_none()).then(|| PullOpening::Rest {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        head: invocation
            .fields("head")
            .find(|value| !value.is_empty())
            .map(|head| strip_head_owner(head).to_owned()),
    })
}

/// Whether a path segment is one gh fills in from the current repository:
/// `{owner}` (any `{…}`) or `:owner` (gh's placeholder regex takes both).
fn is_api_placeholder(segment: &str) -> bool {
    segment.contains('{') || segment.starts_with(':')
}

/// Whether `text` names the GraphQL mutation `name` as a token: followed by
/// its argument list, its selection set, or what GraphQL treats as
/// insignificant before either — whitespace, a comma, a `#` comment — so
/// `createPullRequest(` matches and `createPullRequestReview(` does not.
fn names_mutation(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        let before = text.get(..at).and_then(|head| head.chars().next_back());
        let after = text
            .get(at + name.len()..)
            .and_then(|tail| tail.chars().next());
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && after.is_none_or(|c| matches!(c, '(' | '{' | ',' | '#') || c.is_whitespace())
    })
}

/// `owner:branch` names a fork's branch from the base repository's side; the
/// branch is what the ledger knows.
fn strip_head_owner(head: &str) -> &str {
    head.rsplit_once(':').map_or(head, |(_, branch)| branch)
}

/// Why this invocation may not open the pull request it is about to, or `None`
/// when it opens none, opens it somewhere other than a registered fork's
/// upstream, or opens it for a branch whose recorded placement verdict is
/// `UPSTREAM`.
///
/// Every registry entry names the upstream it forks. A pull request opened
/// there is the one act a fork branch's placement verdict exists to govern:
/// the branch was started with the red-team's ruling (`knives start
/// --placement`), and only `UPSTREAM` says upstream wants the change. Pull
/// requests on the fork's own origin — a review branch, a release — are not
/// upstream's business and pass. No registry file at all means no fork is
/// registered and nothing is gated; since the gate guards policy, stderr says
/// so once and the command passes through. A registry file that is present
/// and cannot be read is an error, not an absence: nothing is let through on
/// a ledger the tool cannot read.
fn upstream_pull_refusal(invocation: &GhInvocation, cwd: &Path) -> anyhow::Result<Option<String>> {
    // A flag gh's table does not define leaves the whole command unreadable:
    // whether it takes a value decides which arguments are values and which
    // the endpoint or a head, so nothing about a `pr create` or `gh api` with
    // one can be told — the target included. Refused before anything is read.
    let table = match (invocation.command.as_deref(), invocation.verb()) {
        (Some("pr"), Some("create")) => Some(("pr create", "PR_CREATE_FLAGS")),
        (Some("api"), _) => Some(("api", "API_FLAGS")),
        _ => None,
    };
    if let Some((command, table)) = table
        && let Some(refusal) = unknown_flag_refusal(invocation, command, table)
    {
        return Ok(Some(refusal));
    }
    let Some(opening) = pull_opening(invocation) else {
        return Ok(None);
    };
    // No registry file: nothing is registered, so nothing is gated — said
    // once, so a silent pass is never mistaken for a verdict. A registry that
    // is present but unreadable is an error: nothing passes on a ledger the
    // tool cannot read.
    let path = crate::config::default_config_path();
    if !path.exists() {
        eprintln!("knives: no registry at {}; nothing to gate", path.display());
        return Ok(None);
    }
    let registry = crate::config::load(&path)?;
    let (repo, head) = match opening {
        PullOpening::Create { heads } => {
            let Some(target) = resolve_target_url(invocation, cwd) else {
                return Ok(None);
            };
            let Some(repo) = upstream_of(&registry, &target) else {
                return Ok(None);
            };
            // gh keeps the last head it is given; a command that states two
            // is refused rather than read either way.
            if let [first, .., last] = heads.as_slice() {
                return Ok(Some(format!(
                    "an upstream pull request for {repo} states {} heads ({}); gh would open \
                     {last} while a reader expects {first}: state one",
                    heads.len(),
                    heads.join(", ")
                )));
            }
            // `--head=` with nothing after it makes gh fall back to the current
            // branch, which that spelling gives knives no way to certify.
            if heads.iter().any(String::is_empty) {
                return Ok(Some(format!(
                    "an upstream pull request for {repo} states an empty head (`--head=`): gh \
                     would open the current branch, which knives cannot certify from that \
                     spelling; state the branch"
                )));
            }
            // The head gh will use when none is stated: the bookmark on `@`
            // (what knives adds for `pr create` in a jj checkout), else git's
            // checked-out branch (gh's own default, in a plain clone). With
            // neither, nothing verifiable is let through.
            let Some(head) = heads
                .into_iter()
                .next()
                .or_else(|| current_bookmark(cwd))
                .or_else(|| git_head_branch(cwd))
            else {
                return Ok(Some(format!(
                    "an upstream pull request for {repo} needs a head branch to check its \
                     placement verdict: state one (`--head <branch>`), or run from a checkout \
                     with a bookmark on @ or a git branch checked out"
                )));
            };
            (repo, head)
        }
        PullOpening::Rest { owner, repo, head } => {
            // `repos/{owner}/{repo}/pulls` and `repos/:owner/:repo/pulls` are
            // gh's own spellings for "the current directory's base
            // repository", which in a fork checkout is the upstream; resolved
            // the way `pr create` without `-R` is.
            let target = if is_api_placeholder(&owner) || is_api_placeholder(&repo) {
                resolve_target_url(invocation, cwd)
            } else {
                url_from_spec(&format!("{owner}/{repo}"))
            };
            let Some(target) = target else {
                return Ok(None);
            };
            let Some(repo) = upstream_of(&registry, &target) else {
                return Ok(None);
            };
            // A REST creation with no head field is gh's error to give, but the
            // head may also travel in a body file knives does not read
            // (`--input`); either way its verdict cannot be checked, and
            // nothing unverifiable is let through.
            let Some(head) = head else {
                return Ok(Some(format!(
                    "an upstream pull request for {repo} needs a head branch (`-f head=<branch>`) \
                     to check its placement verdict"
                )));
            };
            (repo, head)
        }
        PullOpening::RestById { id } => {
            // A numeric id maps to an owner only through a network round-trip
            // knives does not make, so the creation cannot be checked against
            // the registry anywhere; the same call is one `repos/` spelling
            // away, so nothing is lost by refusing it outright.
            return Ok(Some(format!(
                "a pull request creation by numeric repository id ({id}) cannot be checked \
                 against the registry: state the repository as repos/<owner>/<repo>"
            )));
        }
        PullOpening::Graphql { head } => {
            // No owner to read: inside a registered fork the mutation is
            // refused rather than guessed at, since `gh pr create` says where
            // it goes; outside one there is no fork whose rule applies.
            let Ok(fork) = crate::bind::here(&registry, cwd) else {
                return Ok(None);
            };
            return Ok(Some(format!(
                "a GraphQL createPullRequest names its repository by node id, so knives cannot \
                 tell whether it targets {}'s upstream; open it with `gh pr create` (head {})",
                fork.name,
                head.as_deref().unwrap_or("<branch>")
            )));
        }
    };
    let entries = crate::ledger::Ledger::for_repo(&repo).entries()?;
    Ok(crate::placement::upstream_pull_refusal(&entries, &head)?)
}

/// The refusal for a dash-argument gh's table for `command` does not define,
/// or `None` when every flag is known. Whether the argument takes a value
/// decides which arguments are values, so with one unknown nothing about the
/// command can be told; the remedy is one an operator can follow.
fn unknown_flag_refusal(invocation: &GhInvocation, command: &str, table: &str) -> Option<String> {
    let unknown = invocation.unknown.first()?;
    Some(format!(
        "knives does not know gh's flag {unknown} for {command}: if gh accepts it, add it to \
         {table} in src/commands/gh_args.rs; otherwise remove it"
    ))
}

/// The branch git has checked out at the checkout `cwd` is inside, if any:
/// what gh defaults a pull request's head to outside jj. A detached HEAD — a
/// colocated jj checkout's usual state — has none.
fn git_head_branch(cwd: &Path) -> Option<String> {
    let root = crate::bind::checkout_root(cwd)?;
    let output = crate::bind::git(&root)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = std::str::from_utf8(&output.stdout).ok()?.trim();
    (!branch.is_empty()).then(|| branch.to_owned())
}

/// The registered repository whose `upstream` is `target`, if any.
fn upstream_of(registry: &crate::config::Registry, target: &str) -> Option<crate::ids::RepoName> {
    registry
        .repos
        .iter()
        .find(|(_, entry)| crate::remote_url::same_remote(&entry.upstream, target))
        .map(|(name, _)| crate::ids::RepoName::new(name))
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

    /// The one reading every reader is a lookup on.
    fn parsed(args: &[String]) -> GhInvocation {
        GhInvocation::parse(args)
    }

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
            owner_from_api_args(&parsed(&args(&["api", "repos/acme/work/pulls"]))).as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "/orgs/acme/teams"]))).as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "users/someone"]))).as_deref(),
            Some("someone")
        );
        // Query strings on bare segments are stripped (shim line 99).
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "orgs/acme?page=2"]))).as_deref(),
            Some("acme")
        );
        // Placeholders expand from the current repo: no owner signal.
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "repos/{owner}/{repo}/pulls"]))),
            None
        );
        // A flag and its value are read as such; the path is the positional.
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                "repos/acme/work/issues"
            ])))
            .as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "repos/"]))),
            None
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "orgs/?page=2"]))),
            None
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["pr", "list", "repos/acme/work"]))),
            None
        );
    }

    #[test]
    fn the_first_path_shaped_argument_ends_the_rest_scan() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "repos/{owner}/{repo}",
                "-f",
                r#"query=query { repository(owner: "acme") { id } }"#,
            ]))),
            None
        );
    }

    #[test]
    fn graphql_bodies_yield_their_first_owner() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(owner: "acme", name: "work") { id } }"#,
            ])))
            .as_deref(),
            Some("acme")
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                "query=query { organization(login: 'acme') { id } }",
            ])))
            .as_deref(),
            Some("acme")
        );
        // Pure node-id mutations carry no owner signal (line 84).
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                "query=mutation { addProjectV2ItemById(input: {}) { item { id } } }",
            ]))),
            None
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository (owner: "x") { id } }"#,
            ])))
            .as_deref(),
            Some("x")
        );
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(owner: "ac me") { id } }"#,
            ]))),
            None
        );
    }

    #[test]
    fn the_leftmost_graphql_owner_wins_across_both_patterns() {
        // Shim parity: its single alternation regex takes the FIRST match in
        // the text, whichever pattern it is.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { organization(login: "first") { id } repository(owner: "second", name: "x") { id } }"#,
            ])))
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
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(name: "x", owner: "a") { id } organization(login: "b") { id } }"#,
            ])))
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
                owner_from_api_args(&parsed(&args(&argv))).as_deref(),
                Some("acme"),
                "argv {argv:?}"
            );
        }
        // Variable names are matched whole: `$owner` is not bound by `owner_id=`.
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                "owner_id=acme",
                "-f",
                query
            ]))),
            None
        );
        // An unbound variable is no signal; gh would reject the query anyway.
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", "graphql", "-f", query]))),
            None
        );
        // A binding gh reads from a file or that is not an owner-shaped value is no signal.
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-F",
                "owner=@owner.txt",
                "-f",
                query
            ]))),
            None
        );
        // Leftmost still wins when the earlier field is variable-bound.
        assert_eq!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                "org=first",
                "-f",
                r#"query=query($org: String!) { organization(login: $org) { id } repository(owner: "second", name: "x") { id } }"#,
            ])))
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
                resolve_target_url(&parsed(&args(&["pr", "list"])), scratch.path()),
                Some(format!("https://{host}/acme/work.git"))
            );
            assert_eq!(
                resolve_target_url(
                    &parsed(&args(&["api", "repos/{owner}/{repo}/pulls"])),
                    scratch.path()
                ),
                Some(format!("https://{host}/acme/work.git"))
            );
            assert_eq!(
                resolve_target_url(
                    &parsed(&args(&["pr", "list", "-R", "other/repo"])),
                    scratch.path()
                ),
                Some(format!("https://{host}/other/repo.git"))
            );
            // A path literal is the request's real target and still wins.
            assert_eq!(
                resolve_target_url(
                    &parsed(&args(&["api", "repos/literal/repo/pulls"])),
                    scratch.path()
                ),
                Some(format!("https://{host}/literal/gh-api.git"))
            );
        });
        with_env(&[("GH_REPO", "")], || {
            assert_eq!(
                resolve_target_url(&parsed(&args(&["pr", "list"])), scratch.path()),
                None
            );
        });
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
    fn a_rest_pull_creation_is_seen_whatever_precedes_its_path() {
        // `gh api -X POST repos/o/r/pulls` is the documented spelling; a valued
        // flag before the path must not be taken for the path.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        let rest = Some(PullOpening::Rest {
            owner: "o".to_owned(),
            repo: "r".to_owned(),
            head: Some("feat/x".to_owned()),
        });
        for argv in [
            vec!["api", "repos/o/r/pulls", "-f", "head=feat/x"],
            vec!["api", "-X", "POST", "repos/o/r/pulls", "-f", "head=feat/x"],
            vec![
                "api",
                "--method",
                "POST",
                "repos/o/r/pulls",
                "-f",
                "head=feat/x",
            ],
            vec![
                "api",
                "-H",
                "Accept: x",
                "repos/o/r/pulls",
                "-f",
                "head=feat/x",
            ],
            vec![
                "api",
                "--hostname",
                "example.test",
                "/repos/o/r/pulls",
                "-f",
                "head=feat/x",
            ],
            vec![
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls",
                "--input",
                "pr.json",
                "-f",
                "head=feat/x",
            ],
        ] {
            assert_eq!(pull_opening(&parsed(&args(&argv))), rest, "{argv:?}");
        }
        // gh sends an absolute URL verbatim; the path is the same endpoint.
        let absolute = format!("https://api.{DEFAULT_HOST}/repos/o/r/pulls?x=1");
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                &absolute,
                "-f",
                "head=feat/x"
            ]))),
            rest,
            "{absolute}"
        );
        // A GET lists; a path that is not the pulls endpoint opens nothing.
        assert_eq!(
            pull_opening(&parsed(&args(&["api", "repos/o/r/pulls"]))),
            None
        );
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "-X",
                "GET",
                "repos/o/r/pulls",
                "-f",
                "state=open"
            ]))),
            None
        );
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls/1/comments",
                "-f",
                "body=hi"
            ]))),
            None
        );
    }

    #[test]
    fn attached_fields_are_a_body_and_a_fragment_is_not_part_of_the_path() {
        // gh infers POST from any field, attached to its flag or not, and Go's
        // client never sends a `#fragment`, so neither hides the endpoint.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        let rest = Some(PullOpening::Rest {
            owner: "o".to_owned(),
            repo: "r".to_owned(),
            head: Some("feat/x".to_owned()),
        });
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "repos/o/r/pulls",
                "-ftitle=x",
                "-fhead=feat/x"
            ]))),
            rest,
            "attached fields"
        );
        assert_eq!(
            pull_opening(&parsed(&args(&["api", "repos/o/r/pulls", "-Fhead=feat/x"]))),
            rest,
            "attached typed field"
        );
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls#x",
                "-f",
                "head=feat/x"
            ]))),
            rest,
            "fragment"
        );
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls?a=1#x",
                "-f",
                "head=feat/x"
            ]))),
            rest,
            "query and fragment"
        );
    }

    #[test]
    fn a_creation_by_numeric_repository_id_is_read_and_routes_no_token() {
        // GitHub serves `repositories/<id>/pulls` as the same creation
        // endpoint; the id names no owner, so the gate refuses it and token
        // routing mints nothing for it.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        let by_id = Some(PullOpening::RestById {
            id: "1318902388".to_owned(),
        });
        let absolute = format!("https://api.{DEFAULT_HOST}/repositories/1318902388/pulls");
        for argv in [
            vec![
                "api",
                "-X",
                "POST",
                "repositories/1318902388/pulls",
                "-f",
                "head=feat/x",
            ],
            vec![
                "api",
                "repositories/1318902388/pulls",
                "-ftitle=t",
                "-fhead=feat/x",
            ],
            vec!["api", "-X", "POST", absolute.as_str(), "-f", "head=feat/x"],
        ] {
            assert_eq!(pull_opening(&parsed(&args(&argv))), by_id, "{argv:?}");
        }
        // A GET lists; a sibling endpoint opens nothing.
        assert_eq!(
            pull_opening(&parsed(&args(&["api", "repositories/1318902388/pulls"]))),
            None
        );
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                "repositories/1318902388/issues",
                "-f",
                "title=t"
            ]))),
            None
        );
        for argv in [
            vec!["api", "repositories/1318902388/pulls"],
            vec![
                "api",
                "-X",
                "POST",
                "repositories/1318902388/pulls",
                "-f",
                "head=feat/x",
            ],
        ] {
            assert_eq!(owner_from_api_args(&parsed(&args(&argv))), None, "{argv:?}");
        }
    }

    #[test]
    fn one_path_reader_serves_the_gate_and_token_routing() {
        // gh's placeholders, `{owner}` and `:owner` alike, are carried as
        // written for the gate to resolve, and route no token; an absolute URL
        // is the same endpoint to both.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        // The two placeholder spellings gh's regex accepts: `{owner}` and `:owner`.
        for (open, close) in [("{", "}"), (":", "")] {
            let owner = format!("{open}owner{close}");
            let repo = format!("{open}repo{close}");
            let placeholder = format!("repos/{owner}/{repo}/pulls");
            assert_eq!(
                pull_opening(&parsed(&args(&["api", "-X", "POST", &placeholder]))),
                Some(PullOpening::Rest {
                    owner: owner.clone(),
                    repo,
                    head: None,
                }),
                "{placeholder}"
            );
            assert_eq!(
                owner_from_api_args(&parsed(&args(&["api", &placeholder]))),
                None
            );
            assert!(is_api_placeholder(&owner), "{owner}");
        }
        let absolute = format!("https://api.{DEFAULT_HOST}/repos/o/r/pulls");
        assert_eq!(
            owner_from_api_args(&parsed(&args(&["api", &absolute]))).as_deref(),
            Some("o")
        );
        assert!(!is_api_placeholder("routed-a"));
    }

    #[test]
    fn pr_create_reads_the_short_head_flag_too() {
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        let head = Some(PullOpening::Create {
            heads: vec!["feat/x".to_owned()],
        });
        assert_eq!(
            pull_opening(&parsed(&args(&["pr", "create", "-H", "feat/x"]))),
            head
        );
        assert_eq!(
            pull_opening(&parsed(&args(&["pr", "create", "--head", "o:feat/x"]))),
            head
        );
        assert_eq!(
            pull_opening(&parsed(&args(&["pr", "create", "--head=feat/x"]))),
            head
        );
        assert_eq!(
            pull_opening(&parsed(&args(&["pr", "create"]))),
            Some(PullOpening::Create { heads: Vec::new() })
        );
        assert_eq!(
            pull_opening(&parsed(&args(&["pr", "view", "-H", "feat/x"]))),
            None
        );
    }

    #[test]
    fn the_create_mutation_is_matched_as_a_token_not_a_prefix() {
        // Reviews and review threads are maintenance of an open pull request
        // and pass; only the mutation that opens one is caught.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        for query in [
            "mutation { createPullRequest(input:{repositoryId:\"R\",headRefName:\"feat/x\"}) { clientMutationId } }",
            "mutation{createPullRequest (input:$i){clientMutationId}}",
            "mutation { createPullRequest {\n clientMutationId } }",
            "mutation Open { createPullRequest\n(input: $input) { clientMutationId } }",
            // GraphQL treats a comma and a `#` comment as insignificant.
            "mutation { createPullRequest,(input: $i) { clientMutationId } }",
            "mutation { createPullRequest#c\n(input: $i) { clientMutationId } }",
        ] {
            assert!(
                matches!(
                    pull_opening(&parsed(&args(&[
                        "api",
                        "graphql",
                        "-f",
                        &format!("query={query}")
                    ]))),
                    Some(PullOpening::Graphql { .. })
                ),
                "{query}"
            );
        }
        for query in [
            "mutation { createPullRequestReview(input:{pullRequestId:\"P\",event:COMMENT,body:\"hi\"}) { clientMutationId } }",
            "mutation { createPullRequestReviewThread(input:{pullRequestId:\"P\",body:\"hi\"}) { clientMutationId } }",
            "mutation { createPullRequestReviewComment(input:{pullRequestReviewId:\"V\",body:\"hi\"}) { clientMutationId } }",
        ] {
            assert_eq!(
                pull_opening(&parsed(&args(&[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}")
                ]))),
                None,
                "{query}"
            );
        }
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
        let command = args(&["pr", "view", "--json", "title"]);
        let (_, verb_index) = parsed(&command).verb.expect("a pr verb");
        let injected = inject_positional(&command, verb_index, "feat/x");

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
