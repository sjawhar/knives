//! Fork-aware `gh` passthrough.
//!
//! This command executes `gh` directly, so the usual render/run split does not apply:
//! there is no knives result to render.
//!
//! Every question asked of the arguments — the repository a `pr create`
//! targets, the head it states, the endpoint a `gh api` call addresses — is a
//! lookup on the one [`GhInvocation`] built at the top of [`run`], read the way
//! cobra and pflag read gh's command line (see `gh_args`): flags anywhere in
//! the line, before or after the command word, shorthand clusters expanded,
//! gh's own verb aliases normalised, a switch's `=value` a Go bool, a flag the
//! tables do not define kept by name so the gate refuses rather than guesses.
//! Nothing here scans argv twice.
//!
//! What those readings are compared against is never normalised the way gh,
//! go-gh or GitHub would normalise it (see `gh_canon`): a repository is read
//! only as `OWNER/REPO` or `HOST/OWNER/REPO` (two parts on gh's default
//! host: `GH_HOST`, else the one host in gh's configuration — `config.yml`'s
//! `hosts:` block, else `hosts.yml` — else github.com), a
//! head only as `<fork-owner>:<branch>` — gh reads a bare branch as the
//! base repository's own — an endpoint only as `repos/OWNER/REPO/pulls` on
//! the host the request goes to (an absolute URL's own, else `--hostname`,
//! else the default) with its body in `-f` fields (with `--input` gh moves
//! the fields to the query string and knives does not read the file; a
//! `?query` on the endpoint is refused too), and
//! every other spelling toward a registered upstream — a URL of any form,
//! an empty or second `-R`, a bare or another owner's head, a percent-
//! escape, a marker carrying a host, a `--hostname` disagreeing with the
//! URL, a body file — is refused with the canonical spelling. The one
//! comparison rule folds `www.` off both sides and any subdomain of the
//! registered host onto it (`remote_url::same_host`), a superset of gh's
//! own fold. The checkout's remotes are git's effective URLs (`git remote
//! -v`, `insteadOf` applied, a push URL when the fetch URL names no
//! repository) with ssh aliases resolved as go-gh resolves them
//! (`bind::remotes`); gh's configured hosts are read as YAML keys at the
//! map's own indent, and a shape knives cannot read is refused, never
//! read as zero hosts. With no head stated the gate states `<fork-owner>:<branch in hand>`
//! itself, so gh never resolves one knives did not read. A token is routed
//! only for a canonical owner on a host that folds to the default host.
// allow: SIZE_OK: 3534 lines - single passthrough pipeline; splitting would separate resolution steps that read as one procedure.
use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::{
    fs::PermissionsExt as _,
    process::{CommandExt as _, ExitStatusExt as _},
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::gh_args::GhInvocation;
use super::gh_canon::{self, Repo};

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
    // allow is refused without spending a token on it. One it allows and the
    // caller left gh to resolve gets its head stated, below, in the one
    // spelling that names the fork's branch.
    let upstream_head = match upstream_pull_refusal(&invocation, &cwd)? {
        Gate::Refuse(refusal) => {
            eprintln!("knives gh: {refusal}");
            std::process::exit(crate::cli::Exit::Usage.code().into());
        }
        Gate::Pass => None,
        Gate::PassStating(head) => Some(head),
    };
    if let Some(token) = routed_token(&invocation, &cwd) {
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
    if !in_jj_repo && upstream_head.is_none() {
        gh.args(args);
        std::process::exit(gh_exit_code(&mut gh));
    }

    let bookmark = in_jj_repo.then(|| current_bookmark(&cwd)).flatten();
    let arguments = match subcommand.as_str() {
        // A head the caller stated — in any spelling gh reads — is the head;
        // adding one behind it would be the one gh honours. Toward a
        // registered upstream the gate has read the head and names the
        // fork's branch as `OWNER:BRANCH` — gh would otherwise resolve one
        // knives did not read, from git configuration or as the upstream's
        // own branch — in a jj checkout and a plain git clone alike; toward
        // any other repository, in a jj checkout, the bookmark on `@` stands
        // in for git's detached HEAD.
        "create" if !invocation.has("head") => {
            let head = match (&upstream_head, bookmark.as_deref()) {
                (Some(head), _) => head.clone(),
                (None, Some(bookmark)) => bookmark.to_owned(),
                (None, None) => die_no_bookmark(),
            };
            let mut arguments = args.to_vec();
            arguments.push("--head".to_owned());
            arguments.push(head);
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
    if !in_jj_repo {
        // A plain git clone: gh reads git's own HEAD; only the head was added.
        gh.args(&arguments);
        std::process::exit(gh_exit_code(&mut gh));
    }
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

/// The token gh runs with, when one is routed: none when the caller already
/// set `GH_TOKEN`, when the target names no repository, or when the
/// repository is stated in a spelling knives does not compare — that last
/// said once on stderr, so a missing token is never a mystery. A helper
/// refusal exits here with the helper's code.
fn routed_token(invocation: &GhInvocation, cwd: &Path) -> Option<String> {
    if std::env::var_os("GH_TOKEN").is_some() {
        return None;
    }
    match target(invocation, cwd) {
        Target::Repo(url) => match mint_token(&url) {
            Mint::Token(token) => Some(token),
            Mint::Refused(code) => std::process::exit(code),
            Mint::Unrouted => None,
        },
        Target::Absent => None,
        Target::Unreadable(why) => {
            eprintln!("knives gh: no token routed: {why}");
            None
        }
    }
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
    // A query string or `#fragment` is never part of a repository's
    // identity; Go's URL client drops both before routing, so cut them here
    // before anything below treats what follows as part of the path.
    let mut url = url.split(['?', '#']).next().unwrap_or(url).to_owned();
    if url.is_empty() {
        return None;
    }
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
    // The App token is a credential for the default host: a target on any
    // other host — one gh would send to without `GH_TOKEN` — mints nothing.
    // The host is compared by the one rule (`same_host`): a subdomain
    // spelling of the default host is the default host, as gh folds it.
    if !crate::remote_url::remote_host(target_url)
        .is_some_and(|host| crate::remote_url::same_host(host, DEFAULT_HOST))
    {
        return Mint::Unrouted;
    }
    // The helper routes by the owner segment; one that is not a canonical
    // segment — empty after a `//`, a percent-escape, anything GitHub would
    // read differently from the helper — routes nothing rather than a token
    // for a decoy.
    if !path.split('/').next().is_some_and(gh_canon::is_segment) {
        return Mint::Unrouted;
    }
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

/// The owner a `gh api` invocation targets: `Ok(None)` when it carries no
/// signal, `Err` when it carries one knives does not read.
///
/// These invocations often run outside the target repo's checkout and carry no
/// -R, so remote-based resolution would route the token to the wrong owner —
/// that is the failure this exists for (shim lines 80-86). Pure node-id
/// GraphQL mutations genuinely have no signal; `gh-app-token` honors
/// `GH_APP_OWNER` for those, which is out of knives' hands. An owner is read
/// only as a canonical segment: a percent-escape, an empty segment after
/// `//`, a GraphQL literal outside the grammar is a signal knives cannot
/// route on, and falls through to nothing rather than to the checkout's
/// remotes — a token for the checkout's own owner would be a token for a
/// call about some other repository.
pub(crate) fn owner_from_api_args(invocation: &GhInvocation) -> Result<Option<String>, String> {
    if invocation.command.as_deref() != Some("api") {
        return Ok(None);
    }
    let unreadable = |what: &str, owner: &str| {
        format!(
            "the owner {what} names, {owner:?}, is outside the grammar knives routes by (one \
             segment of [{}]): state it that way",
            gh_canon::SEGMENT_CHARS
        )
    };
    if let Some(endpoint) = api_endpoint(invocation) {
        // A repository addressed by numeric id names no owner: no token is
        // minted for it, as none is for a placeholder path.
        if endpoint.path.starts_with("repositories/") {
            return Ok(None);
        }
        for prefix in ["repos/", "orgs/", "users/"] {
            if let Some(rest) = endpoint.path.strip_prefix(prefix) {
                let owner = rest.split('/').next().unwrap_or("");
                if is_placeholder(owner, "owner") {
                    return Ok(None);
                }
                return if gh_canon::is_segment(owner) {
                    Ok(Some(owner.to_owned()))
                } else {
                    Err(unreadable(
                        &format!("the endpoint {:?}", endpoint.path),
                        owner,
                    ))
                };
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
        Some((_, GraphqlValue::Literal(owner))) if gh_canon::is_segment(&owner) => Ok(Some(owner)),
        Some((_, GraphqlValue::Literal(owner))) => Err(unreadable("the GraphQL document", &owner)),
        // The query names the owner through a variable; its value travels as a
        // separate `-f owner=acme` field argument (how knives' own forge queries
        // and gh's documentation write it).
        Some((_, GraphqlValue::Variable(name))) => Ok(invocation
            .fields(&name)
            .find(|value| gh_canon::is_segment(value))
            .map(str::to_owned)),
        None => Ok(None),
    }
}

/// The GraphQL text a `gh api` call carries in its arguments: every field
/// value, in order (`-f query=…` is where a document travels). A document gh
/// reads from a file (`--input`, `-F query=@file`; `-f` expands no `@file`) is not opened.
fn graphql_text(invocation: &GhInvocation) -> String {
    invocation
        .flags
        .iter()
        .filter(|flag| matches!(flag.name, "raw-field" | "field"))
        .filter_map(|flag| flag.value.as_deref())
        .collect::<Vec<_>>()
        .join(" ")
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

/// The REST endpoint a `gh api` call addresses: its first positional, less
/// one leading `/`, less its query string and any `#fragment` (which Go's
/// client never sends). An absolute URL — which gh sends verbatim, whatever
/// `--hostname` or `GH_HOST` say — is read only as `https://api.<default
/// host>/<path>`, spelled exactly so, and then states its host
/// (`url_host`); any other is `foreign`, a host knives does not compare. A
/// flag's value is never the endpoint, whatever it looks like.
struct ApiPath<'a> {
    path: &'a str,
    /// The host the URL itself states, when the endpoint is an absolute URL
    /// knives reads.
    url_host: Option<&'static str>,
    foreign: bool,
    /// The endpoint carried a `?query` string: parameters GitHub may read
    /// alongside — or over — the body, which knives does not compare.
    query: bool,
}

fn api_endpoint(invocation: &GhInvocation) -> Option<ApiPath<'_>> {
    let argument = invocation.positionals.first()?;
    if argument.contains("://") {
        let path = argument
            .strip_prefix("https://api.")
            .and_then(|rest| rest.strip_prefix(DEFAULT_HOST))
            .and_then(|rest| rest.strip_prefix('/'));
        return Some(path.map_or_else(
            || ApiPath {
                path: without_query(argument),
                url_host: None,
                foreign: true,
                query: has_query(argument),
            },
            |path| ApiPath {
                path: without_query(path),
                url_host: Some(DEFAULT_HOST),
                foreign: false,
                query: has_query(path),
            },
        ));
    }
    let path = argument.strip_prefix('/').unwrap_or(argument);
    Some(ApiPath {
        path: without_query(path),
        url_host: None,
        foreign: false,
        query: has_query(path),
    })
}

/// Whether `path` carries a `?query` string before any `#fragment`.
fn has_query(path: &str) -> bool {
    path.split('#')
        .next()
        .is_some_and(|before| before.contains('?'))
}

/// The host gh addresses when none is stated, read as go-gh's `DefaultHost`
/// reads it (`pkg/auth/auth.go`): `GH_HOST` when set and non-empty (read
/// verbatim — a literal, nothing to normalise); else the one host gh is
/// configured for when there is exactly one — the `hosts:` block inside
/// `config.yml` first (the pre-multi-account layout gh still reads and
/// keeps ahead of the other file, `config.go` `load`), else `hosts.yml`;
/// else the default host — with several hosts configured gh uses the
/// default host whichever they are (measured: `ghe.test` + `other.test` →
/// github.com). A host outside the grammar knives compares is refused rather
/// than read, as is a file knives cannot read — gh itself errors on invalid
/// YAML there, and knives refuses rather than reproduces what gh would do
/// with any other shape.
fn default_host() -> Result<String, String> {
    if let Ok(host) = std::env::var("GH_HOST")
        && !host.is_empty()
    {
        return canonical_host("GH_HOST", &host);
    }
    let (source, hosts) = configured_hosts()?;
    match hosts.as_slice() {
        [host] => canonical_host(&format!("the one host in gh's {source}"), host),
        _ => Ok(DEFAULT_HOST.to_owned()),
    }
}

/// `host` when it is one canonical segment — a `HOST:PORT` key, which gh
/// reads as a host with a port, is read as its host; the refusal naming
/// `source` otherwise.
fn canonical_host(source: &str, host: &str) -> Result<String, String> {
    let bare = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    gh_canon::is_segment(bare)
        .then(|| bare.to_owned())
        .ok_or_else(|| {
            format!(
                "{source}, {host:?}, is not a host knives compares (one segment of [{}]): state it \
                 that way, or state the host in the command (-R HOST/OWNER/REPO, --hostname)",
                gh_canon::SEGMENT_CHARS
            )
        })
}

/// gh's configuration directory, by go-gh's `ConfigDir` precedence
/// (`pkg/config/config.go`): `GH_CONFIG_DIR`, `$XDG_CONFIG_HOME/gh`,
/// `$HOME/.config/gh` (the Windows `AppData` step has no Unix reading).
fn gh_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GH_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(xdg).join("gh"));
    }
    std::env::home_dir().map(|home| home.join(".config").join("gh"))
}

/// The hosts gh is configured for, in file order, with the file they came
/// from: `config.yml`'s `hosts:` block when it has one, else `hosts.yml`'s
/// top-level keys; none when neither file has any. Each is read as a YAML
/// block map the way gh reads the shapes it writes and the hand-edited
/// shapes it tolerates: a key is a `key:` line — surrounding whitespace, one
/// layer of matching quotes and a trailing ` # comment` stripped — with
/// nothing else after the colon (its value is the deeper-indented map
/// below); the map's indent is its first content line's, at any level, and
/// every key sits at that level. Any other shape — a flow map, a scalar
/// after the colon, a list item, a line with no colon, a mixed indent (gh
/// errors on that one too) — or a file that cannot be read, is a refusal
/// rather than a guess at what gh would do with it; a block with content
/// but no readable key is never zero hosts.
fn configured_hosts() -> Result<(&'static str, Vec<String>), String> {
    let Some(dir) = gh_config_dir() else {
        return Ok(("hosts.yml", Vec::new()));
    };
    let general = read_gh_file(&dir.join("config.yml"))?;
    if let Some(block) = hosts_block_of_config(&general) {
        return hosts_in(&dir.join("config.yml"), &block).map(|hosts| ("config.yml", hosts));
    }
    let hosts = read_gh_file(&dir.join("hosts.yml"))?;
    hosts_in(&dir.join("hosts.yml"), &hosts).map(|hosts| ("hosts.yml", hosts))
}

/// A gh configuration file's text, a leading UTF-8 byte-order mark dropped;
/// empty when absent; the refusal when it cannot be read.
fn read_gh_file(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.trim_start_matches('\u{feff}').to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!(
            "gh's {} cannot be read ({error}), so the default host cannot be told: state the host \
             in the command (-R HOST/OWNER/REPO, --hostname) or set GH_HOST",
            path.display()
        )),
    }
}

/// A YAML line that carries no key: blank, a `#` comment, `---`, `{}`.
fn is_yaml_noise(line: &str) -> bool {
    let content = line.trim();
    content.is_empty() || content.starts_with('#') || content == "---" || content == "{}"
}

/// The key a YAML block-map line states, when the line is `key:` and
/// nothing else: surrounding whitespace, one layer of matching quotes and a
/// trailing ` # comment` stripped. `None` for a line with a value after the
/// colon, a list item, or no colon.
fn yaml_key(line: &str) -> Option<&str> {
    let content = line.trim();
    // A ` #` outside quotes begins a comment; inside a quoted key it is text.
    let content = match content.find(" #") {
        Some(at) if content[..at].matches(['"', '\'']).count().is_multiple_of(2) => {
            content[..at].trim_end()
        }
        _ => content,
    };
    let key = content.strip_suffix(':')?.trim_end();
    let key = match (key.chars().next(), key.chars().last()) {
        (Some('"'), Some('"')) | (Some('\''), Some('\'')) if key.len() >= 2 => {
            &key[1..key.len() - 1]
        }
        _ => key,
    };
    (!key.is_empty() && !key.starts_with('-')).then_some(key)
}

/// Whether a column-0 line is a `hosts:` key with a value on the same line
/// (`hosts: {…}`, `"hosts": ~`) — gh reads the value, knives refuses it.
fn names_hosts_with_a_value(line: &str) -> bool {
    let content = line.trim();
    ["hosts:", "\"hosts\":", "'hosts':", "hosts :"]
        .iter()
        .any(|spelling| {
            content.strip_prefix(spelling).is_some_and(|rest| {
                let rest = rest.trim();
                !rest.is_empty() && !rest.starts_with('#')
            })
        })
}

/// The lines under a column-0 `hosts:` key in `config.yml` — every line
/// after it up to the next column-0 content line — or `None` when the file
/// has no such key. The heading is matched as a YAML key (`"hosts":`,
/// `hosts :`, `hosts:  # comment` are all it, as gh reads them); a `hosts:`
/// heading with a value on the line (a flow map, `~`) is returned as that
/// one line for the block reader to refuse.
fn hosts_block_of_config(text: &str) -> Option<String> {
    let mut lines = text.lines();
    let heading = lines.find(|line| {
        !line.starts_with(char::is_whitespace)
            && (yaml_key(line) == Some("hosts") || names_hosts_with_a_value(line))
    })?;
    if yaml_key(heading) != Some("hosts") {
        return Some(heading.to_owned());
    }
    Some(
        lines
            .take_while(|line| is_yaml_noise(line) || line.starts_with(char::is_whitespace))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// The host keys in `text`, a YAML block map at whatever indent its first
/// content line sits at: the `key:` lines at exactly that indent; lines
/// indented deeper (a host's own map) and noise are skipped; anything else
/// — a key-less line, a shallower or otherwise inconsistent indent — is
/// refused naming `path` and the line.
fn hosts_in(path: &Path, text: &str) -> Result<Vec<String>, String> {
    let mut hosts = Vec::new();
    let mut indent: Option<usize> = None;
    // The indent a host's own map sits at, once seen: a line between the
    // keys' level and it is a mixed indent (gh errors on it too).
    let mut child: Option<usize> = None;
    for line in text.lines() {
        if is_yaml_noise(line) {
            continue;
        }
        let depth = line.len() - line.trim_start().len();
        let level = *indent.get_or_insert(depth);
        if depth > level {
            if hosts.is_empty() || child.is_some_and(|child| depth < child) {
                return Err(hosts_refusal(path, line.trim_end()));
            }
            child.get_or_insert(depth);
            continue;
        }
        if depth < level {
            return Err(hosts_refusal(path, line.trim_end()));
        }
        let Some(key) = yaml_key(line) else {
            return Err(hosts_refusal(path, line.trim_end()));
        };
        hosts.push(key.to_owned());
        child = None;
    }
    Ok(hosts)
}

fn hosts_refusal(path: &Path, line: &str) -> String {
    format!(
        "gh's {} is not a hosts map knives can read (line {line:?}), so the default host cannot be \
         told: state the host in the command (-R HOST/OWNER/REPO, --hostname) or set GH_HOST",
        path.display()
    )
}

/// `path` less its query string and any `#fragment`.
fn without_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

/// Whether a `gh api` call addresses a repository by GitHub's numeric id
/// (`repositories/<id>/…`), which names no owner knives can read.
fn names_repository_by_id(invocation: &GhInvocation) -> bool {
    invocation.command.as_deref() == Some("api")
        && api_endpoint(invocation)
            .is_some_and(|endpoint| !endpoint.foreign && endpoint.path.starts_with("repositories/"))
}

/// The repository an invocation addresses, read in canonical form or not
/// at all. Both the token routing and the placement gate read it: the
/// routing mints for [`Target::Repo`] and nothing else; the gate compares
/// [`Target::Repo`] against the registry and refuses [`Target::Unreadable`].
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A repository, as the https URL the registry is compared by
    /// (`remote_url::same_remote`: host and path, each case-insensitively).
    Repo(String),
    /// Nothing states one and the checkout offers none: gh's own error to
    /// give.
    Absent,
    /// One is stated in a spelling knives does not compare; the remedy
    /// names the canonical spelling.
    Unreadable(String),
}

/// The repository the arguments or the environment state, read once:
/// `-R`/`--repo` (every occurrence — two are refused rather than read
/// last-wins), else gh's own `GH_REPO` override. `None` when neither is
/// given; `Err` when what is given is not `OWNER/REPO` or
/// `HOST/OWNER/REPO` — a URL of any scheme, an empty value, a percent-
/// escape — which knives refuses rather than reads the way gh would.
fn stated_repository(invocation: &GhInvocation) -> Option<Result<Repo, String>> {
    let stated: Vec<&str> = invocation
        .flags
        .iter()
        .filter(|flag| flag.name == "repo")
        .map(|flag| flag.value.as_deref().unwrap_or(""))
        .collect();
    // A two-part spec is on gh's default host, which `GH_HOST` overrides.
    let read = |source: &str, spec: &str| -> Result<Repo, String> {
        let host = default_host()?;
        Repo::parse(spec, &host).ok_or_else(|| gh_canon::repo_refusal(source, spec))
    };
    match stated.as_slice() {
        [] => {
            let spec = gh_repo_environment()?;
            Some(read("GH_REPO", &spec))
        }
        [spec] => Some(read("-R", spec)),
        several => Some(Err(format!(
            "states {} repositories (-R {}); gh would use the last while a reader expects the \
             first: state one",
            several.len(),
            several
                .iter()
                .map(|spec| format!("{spec:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The repository this invocation addresses (shim lines 75-179).
///
/// A `gh api repositories/<numeric id>/…` call names a repository knives
/// cannot map to an owner without asking GitHub, so it has no target here:
/// no token is minted for it, and the checkout's own remotes — the fallback a
/// path with no owner (`api user`) resolves through — are not consulted,
/// since the call is about whichever repository the id names, not this one.
fn target(invocation: &GhInvocation, cwd: &Path) -> Target {
    if names_repository_by_id(invocation) {
        return Target::Absent;
    }
    // An owner read from a `gh api` endpoint is on the host the request goes
    // to: the URL's, `--hostname`'s, or the default (`GH_HOST`, else
    // github.com).
    let api_owner = match owner_from_api_args(invocation) {
        Ok(Some(owner)) => {
            let url_host = api_endpoint(invocation).and_then(|endpoint| endpoint.url_host);
            match rest_host(invocation, url_host) {
                Ok(host) => Some((host, owner)),
                Err(why) => return Target::Unreadable(format!("the gh api call {why}")),
            }
        }
        Ok(None) => None,
        Err(refusal) => return Target::Unreadable(refusal),
    };
    let stated = stated_repository(invocation);
    let needs_git_inputs = api_owner.is_none() && stated.is_none();
    let resolved_remote = needs_git_inputs.then(|| gh_resolved_remote(cwd)).flatten();
    let registry = needs_git_inputs
        .then(|| crate::config::load(&crate::config::default_config_path()).ok())
        .flatten();
    let bound = registry
        .as_ref()
        .and_then(|registry| crate::bind::here(registry, cwd).ok());
    let registered_entry = bound.as_ref().map(|fork| fork.entry);
    // A marker of either kind needs the marked remote's URL: `base` takes
    // the repository from it, `OWNER/REPO` takes the host from it.
    let requires_remotes =
        needs_git_inputs && (resolved_remote.is_some() || registered_entry.is_none());
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
        api_owner: api_owner
            .as_ref()
            .map(|(host, owner)| (host.as_str(), owner.as_str())),
        stated,
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
    /// The host and owner a `gh api` endpoint names, canonical segments.
    api_owner: Option<(&'a str, &'a str)>,
    /// `-R`/`GH_REPO` as [`stated_repository`] read it.
    stated: Option<Result<Repo, String>>,
    resolved_remote: Option<ResolvedRemote<'a>>,
    registered_entry: Option<&'a crate::config::RepoEntry>,
    remotes: &'a BTreeMap<String, String>,
}

/// Resolves steps 0–3 in shim order: API owner, explicit repo, marker, then remotes.
///
/// Steps 0–2 are terminal when their inputs exist; only their absence advances to the
/// next step (shim lines 123-178). A `gh repo set-default` marker is read as gh
/// writes it and nothing wider: `base` names the marked remote's own repository;
/// `OWNER/REPO` names that repository on the marked remote's host — gh takes the
/// host from the remote whatever the value says, so a value carrying one is
/// refused rather than read with either host. The remotes themselves are the
/// checkout's own configuration and are normalised ([`normalize_url`]).
fn resolve_from_inputs(inputs: TargetInputs<'_>) -> Target {
    if let Some((host, owner)) = inputs.api_owner {
        return Target::Repo(
            Repo {
                host: host.to_owned(),
                owner: owner.to_owned(),
                name: "gh-api".to_owned(),
            }
            .url(),
        );
    }
    match inputs.stated {
        Some(Ok(repo)) => return Target::Repo(repo.url()),
        Some(Err(refusal)) => return Target::Unreadable(refusal),
        None => {}
    }
    if let Some(resolved) = inputs.resolved_remote {
        let source = format!("remote.{}.gh-resolved", resolved.name);
        let remote = inputs.remotes.get(resolved.name);
        if resolved.value == "base" {
            return remote.and_then(|url| normalize_url(url)).map_or_else(
                || {
                    Target::Unreadable(format!(
                        "{source} = base names a remote with no URL: give it one, or unset the \
                         marker (git config --unset {source})"
                    ))
                },
                Target::Repo,
            );
        }
        let Some(host) = remote.and_then(|url| crate::remote_url::remote_host(url)) else {
            return Target::Unreadable(format!(
                "{source} = {:?} names a remote with no URL to take the host from: give it one, \
                 or unset the marker (git config --unset {source})",
                resolved.value
            ));
        };
        return Repo::parse_on(resolved.value, host).map_or_else(
            || {
                Target::Unreadable(format!(
                    "{source} is read only as `base` or `OWNER/REPO` (the host is the remote's, \
                     as gh reads it); {:?} is neither: run `gh repo set-default`, which writes \
                     one of those, or unset the marker (git config --unset {source})",
                    resolved.value
                ))
            },
            |repo| Target::Repo(repo.url()),
        );
    }
    preferred_remote_url(inputs.registered_entry, inputs.remotes)
        .map_or(Target::Absent, Target::Repo)
}

/// gh's own remote-preference score (`context.remoteNameSortScore`): named
/// remotes rank above every other name, which all tie at zero and keep
/// git's own listing order — a stable pick of the first among those, below.
fn resolved_remote_rank(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "upstream" => 3,
        "github" => 2,
        "origin" => 1,
        _ => 0,
    }
}

/// The highest-ranked `gh repo set-default` marker, if git reports one (shim
/// lines 151-164). gh's own `Remotes.Sort` ranks named remotes
/// (`upstream` > `github` > `origin` > everything else, `resolved_remote_rank`)
/// and then takes the first with any resolved value at all
/// (`Remotes.ResolvedRemote`); two markers on differently-named remotes are
/// not a disagreement to gh, only a config carrying more than one — this
/// reads all of them and ranks them the same way, rather than picking
/// whichever git config happens to list first.
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
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let mut best: Option<(u8, OwnedResolvedRemote)> = None;
    for line in text.lines() {
        // Unlike the shim, a valueless marker remains terminal with an empty
        // target, so it mints no token instead of passing a garbage URL onward.
        let (key_with_value, value) = line.rsplit_once(char::is_whitespace).unwrap_or((line, ""));
        let Some((key, _)) = key_with_value.rsplit_once(".gh-resolved") else {
            continue;
        };
        let Some(name) = key.strip_prefix("remote.") else {
            continue;
        };
        let rank = resolved_remote_rank(name);
        if best.as_ref().is_none_or(|(best_rank, _)| rank > *best_rank) {
            best = Some((
                rank,
                OwnedResolvedRemote {
                    name: name.to_owned(),
                    value: value.to_owned(),
                },
            ));
        }
    }
    best.map(|(_, resolved)| resolved)
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
    /// `gh pr create` (or its alias `pr new`); every head `--head`/`-H`
    /// states, in order, as written (an empty string for one given no value).
    Create { heads: Vec<String> },
    /// `gh api repos/OWNER/REPO/pulls` with method POST: a REST creation at
    /// a repository named in canonical form, with every `head` field stated.
    Rest { repo: Repo, heads: Vec<String> },
    /// The same at gh's placeholders (`repos/{owner}/{repo}/pulls`,
    /// `repos/:owner/:repo/pulls`): the current directory's base repository.
    RestAtBase { heads: Vec<String> },
    /// A GraphQL `createPullRequest` mutation, which names its repository by
    /// node id and so carries no owner knives can read.
    Graphql { head: Option<String> },
    /// A creation knives does not read — a percent-escaped or foreign
    /// endpoint, two methods, a numeric repository id, a segment outside the
    /// grammar — with the refusal that names the canonical spelling.
    Unreadable(String),
}

/// Whether this invocation opens a pull request, and how (`None`: it does not).
///
/// The placement verdict governs opening a pull request upstream, so this is
/// deliberately narrow: `pr create`, the REST endpoint that creates one, and
/// the GraphQL mutation that does. Comments, reviews and edits on a pull
/// request that already exists are maintenance of work already open and pass
/// as before — `createPullRequestReview` is one of those, so the mutation name
/// is matched as a token, not a substring. A GraphQL document gh reads from a
/// file (`--input <file>`, `-F query=@file`) is not opened: only the arguments
/// are read.
fn pull_opening(invocation: &GhInvocation) -> Option<PullOpening> {
    // Help in effect makes gh print help and run nothing at all — `pr
    // create` and `gh api` alike — so nothing about the rest of the
    // invocation can open a pull request past it. Read as cobra reads it:
    // `--help=false` is a switch given false, and the command runs.
    if invocation.help_in_effect() {
        return None;
    }
    match invocation.command.as_deref() {
        Some("pr") => {
            return (invocation.verb() == Some("create")).then(|| PullOpening::Create {
                heads: stated_values(invocation, "head"),
            });
        }
        Some("api") => {}
        _ => return None,
    }
    let endpoint = api_endpoint(invocation)?;
    // A request with fields is a POST in gh's own default; `-X GET` on the
    // pulls endpoint lists them and opens nothing. Two methods are refused
    // rather than read last-wins.
    let methods = stated_values(invocation, "method");
    let creates = match methods.as_slice() {
        [] => ["raw-field", "field", "input"]
            .iter()
            .any(|flag| invocation.has(flag)),
        [method] => method.eq_ignore_ascii_case("POST"),
        several => {
            return Some(PullOpening::Unreadable(format!(
                "states {} methods (-X {}); gh would use the last while a reader expects the \
                 first: state one",
                several.len(),
                several.join(", -X ")
            )));
        }
    };
    if !creates {
        return None;
    }
    let path = endpoint.path;
    let canonical = format!(
        "repos/OWNER/REPO/pulls (each segment of [{}])",
        gh_canon::SEGMENT_CHARS
    );
    if endpoint.foreign {
        return Some(PullOpening::Unreadable(format!(
            "a POST to {path:?} addresses a host knives does not compare: state the endpoint \
             as {canonical}, or as https://api.{DEFAULT_HOST}/<that>"
        )));
    }
    // GitHub decodes a percent-escape anywhere in the path before routing;
    // knives reads the path as written, so an escaped POST is refused, not
    // compared unread.
    if path.contains('%') {
        return Some(PullOpening::Unreadable(format!(
            "a POST to {path:?} is percent-encoded, which GitHub decodes and knives does not: \
             state the endpoint unencoded, as {canonical}"
        )));
    }
    if path.eq_ignore_ascii_case("graphql") {
        return names_mutation(&graphql_text(invocation), "createPullRequest").then(|| {
            PullOpening::Graphql {
                head: invocation
                    .fields("headRefName")
                    .find(|value| !value.is_empty())
                    .map(str::to_owned),
            }
        });
    }
    // GitHub serves every repository endpoint under `repos/<owner>/<repo>/…`
    // and, by numeric id, `repositories/<id>/…`; the id maps to an owner
    // only through a network round-trip knives does not make.
    if let Some(rest) = path.strip_prefix("repositories/") {
        let mut segments = rest.split('/');
        let id = segments.next()?;
        return (segments.next() == Some("pulls") && segments.next().is_none()).then(|| {
            PullOpening::Unreadable(format!(
                "a pull request creation by numeric repository id ({id}) cannot be checked \
                 against the registry: state the repository as repos/<owner>/<repo>"
            ))
        });
    }
    // A `?query` string on the creation endpoint carries parameters GitHub
    // may read alongside — or over — the body knives certifies the head
    // from; nothing verifiable is let through with one. A `#fragment` is
    // never sent (Go's client drops it) and is not read either way.
    if endpoint.query {
        return Some(PullOpening::Unreadable(format!(
            "a POST to {path:?} carries a query string, which GitHub may read over the body: state \
             the endpoint as repos/OWNER/REPO/pulls and the fields as -f"
        )));
    }
    rest_opening(invocation, path, endpoint.url_host)
}

/// The REST creation a POST to `path` is, if it is one: `repos/OWNER/REPO/pulls`
/// at a repository named in canonical form, the same at gh's placeholders, or
/// unreadable — an empty segment, a segment outside the grammar, a host stated
/// outside it. `None` for any other endpoint under `repos/`.
fn rest_opening(
    invocation: &GhInvocation,
    path: &str,
    url_host: Option<&str>,
) -> Option<PullOpening> {
    let canonical = format!(
        "repos/OWNER/REPO/pulls (each segment of [{}])",
        gh_canon::SEGMENT_CHARS
    );
    let rest = path.strip_prefix("repos/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Some(PullOpening::Unreadable(format!(
            "a POST to {path:?} has an empty path segment: state the endpoint as {canonical}"
        )));
    }
    let [owner, name, "pulls"] = segments.as_slice() else {
        return None;
    };
    // With `--input`, gh takes the body from the file and puts every `-f`/`-F`
    // field on the query string (api.go:369-376); GitHub reads `head` for
    // this endpoint from the body only. knives does not read bodies, so a
    // creation whose body travels in a file is refused whatever fields sit
    // beside it — a `-f head=` there certifies nothing GitHub will use.
    if invocation.has("input") {
        return Some(PullOpening::Unreadable(format!(
            "a POST to {path:?} takes its body from --input, which knives does not read (gh puts \
             -f/-F fields on the query string, not in the body): state the head as -f \
             head=<fork-owner>:<branch> and the other fields as -f, without --input"
        )));
    }
    let heads = invocation
        .fields("head")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let placeholders = (is_placeholder(owner, "owner"), is_placeholder(name, "repo"));
    let segments = (gh_canon::is_segment(owner), gh_canon::is_segment(name));
    Some(match (placeholders, segments) {
        ((true, true), _) => PullOpening::RestAtBase { heads },
        (_, (true, true)) => match rest_host(invocation, url_host) {
            Ok(host) => PullOpening::Rest {
                repo: Repo {
                    host,
                    owner: (*owner).to_owned(),
                    name: (*name).to_owned(),
                },
                heads,
            },
            Err(why) => PullOpening::Unreadable(format!("a POST to {path:?} {why}")),
        },
        _ => PullOpening::Unreadable(format!(
            "a POST to {path:?} names its repository outside the grammar knives compares: state \
             the endpoint as {canonical}, or as repos/{{owner}}/{{repo}}/pulls for the current \
             directory's repository"
        )),
    })
}

/// Every value the flag `name` was given, in order, an empty string for an
/// occurrence given none (gh's own error, which the reader refuses as an
/// empty value rather than reads past).
fn stated_values(invocation: &GhInvocation, name: &str) -> Vec<String> {
    invocation
        .flags
        .iter()
        .filter(|flag| flag.name == name)
        .map(|flag| flag.value.clone().unwrap_or_default())
        .collect()
}

/// The host a `gh api` REST call addresses. An absolute URL states its own
/// host and gh sends to it verbatim, so that host wins; a `--hostname` that
/// disagrees with it is refused rather than read either way. A relative
/// path goes to `--hostname` when stated once and canonical, else to the
/// default host (`GH_HOST`, else `github.com`). Two hostnames, or one
/// outside the grammar, are refused.
fn rest_host(invocation: &GhInvocation, url_host: Option<&str>) -> Result<String, String> {
    let hostnames = stated_values(invocation, "hostname");
    let hostname = match hostnames.as_slice() {
        [] => None,
        [hostname] if gh_canon::is_segment(hostname) => Some(hostname.as_str()),
        [hostname] => {
            return Err(format!(
                "states its host outside the grammar knives compares (--hostname {hostname:?}): \
                 state one host, of [{}]",
                gh_canon::SEGMENT_CHARS
            ));
        }
        several => {
            return Err(format!(
                "states {} hosts (--hostname {}): state one",
                several.len(),
                several.join(", --hostname ")
            ));
        }
    };
    match (url_host, hostname) {
        (Some(url_host), Some(hostname)) if !crate::remote_url::same_host(hostname, url_host) => {
            Err(format!(
                "states two hosts (--hostname {hostname}, URL host {url_host}); gh sends to the \
                 URL's host whatever --hostname says: state one"
            ))
        }
        (Some(url_host), _) => Ok(url_host.to_owned()),
        (None, Some(hostname)) => Ok(hostname.to_owned()),
        (None, None) => default_host(),
    }
}

/// Whether a path segment is gh's placeholder `name`, in either spelling gh
/// fills from the current repository: `{name}` or `:name`, exactly.
fn is_placeholder(segment: &str, name: &str) -> bool {
    segment
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|inner| inner == name)
        || segment.strip_prefix(':') == Some(name)
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
/// a ledger the tool cannot read. A creation whose repository or head is
/// spelled outside the grammar knives compares is refused with the canonical
/// spelling, never read the way gh would read it.
fn upstream_pull_refusal(invocation: &GhInvocation, cwd: &Path) -> anyhow::Result<Gate> {
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
        && let Some(refusal) = unreadable_flag_refusal(invocation, command, table)
    {
        return Ok(Gate::Refuse(refusal));
    }
    let Some(opening) = pull_opening(invocation) else {
        return Ok(Gate::Pass);
    };
    // No registry file: nothing is registered, so nothing is gated — said
    // once, so a silent pass is never mistaken for a verdict. A registry that
    // is present but unreadable is an error: nothing passes on a ledger the
    // tool cannot read.
    let path = crate::config::default_config_path();
    if !path.exists() {
        eprintln!("knives: no registry at {}; nothing to gate", path.display());
        return Ok(Gate::Pass);
    }
    let registry = crate::config::load(&path)?;
    let subject = match opening_subject(opening, &registry, invocation, cwd) {
        Ok(subject) => subject,
        Err(Early::Pass) => return Ok(Gate::Pass),
        Err(Early::Refuse(refusal)) => return Ok(Gate::Refuse(refusal)),
    };
    let entries = crate::ledger::Ledger::for_repo(&subject.repo).entries()?;
    if let Some(refusal) = crate::placement::upstream_pull_refusal(&entries, &subject.branch)? {
        return Ok(Gate::Refuse(refusal));
    }
    Ok(subject.state.map_or(Gate::Pass, Gate::PassStating))
}

/// The gate's word on an invocation: run it, refuse it, or run it with the
/// head stated as `OWNER:BRANCH` — the fork's branch the gate certified,
/// which the caller left gh to resolve.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Pass,
    Refuse(String),
    PassStating(String),
}

/// What a creation toward a registered upstream is about: the registered
/// repository, the fork's branch whose verdict decides, and the `--head` to
/// add when the caller stated none.
struct Subject {
    repo: crate::ids::RepoName,
    branch: String,
    state: Option<String>,
}

/// The registered repository and head a creation is about, or why the gate
/// stops early: it passes (another repository, none at all, no fork here)
/// or it refuses (a spelling outside the grammar, an unreadable head).
fn opening_subject(
    opening: PullOpening,
    registry: &crate::config::Registry,
    invocation: &GhInvocation,
    cwd: &Path,
) -> Result<Subject, Early> {
    Ok(match opening {
        PullOpening::Unreadable(refusal) => return Err(Early::Refuse(refusal)),
        PullOpening::Create { heads } => {
            let fork = upstream_target(registry, invocation, cwd)?;
            if let Some(branch) = one_head(&fork, &heads, "--head")? {
                return Ok(Subject {
                    repo: fork.name,
                    branch,
                    state: None,
                });
            }
            // None stated: gh would read git configuration knives does not —
            // the branch's push target, which may be another branch of the
            // fork — or abort. knives certifies the branch in hand (the
            // bookmark on `@` in a jj checkout, git's checked-out branch in
            // a plain clone) and states it to gh as the fork's own; with
            // neither, nothing is let through.
            let Some(branch) = current_bookmark(cwd).or_else(|| git_head_branch(cwd)) else {
                return Err(Early::Refuse(format!(
                    "an upstream pull request for {} needs a head branch to check its placement \
                     verdict: state one (`--head {}:<branch>`), or run from a checkout with a \
                     bookmark on @ or a git branch checked out",
                    fork.name, fork.owner
                )));
            };
            let state = format!("{}:{branch}", fork.owner);
            Subject {
                repo: fork.name,
                branch,
                state: Some(state),
            }
        }
        PullOpening::Rest { repo: at, heads } => {
            let fork = Fork::of(registry, &at.url())
                .ok_or(Early::Pass)?
                .map_err(Early::Refuse)?;
            let branch = rest_head(&fork, &heads)?;
            Subject {
                repo: fork.name,
                branch,
                state: None,
            }
        }
        PullOpening::RestAtBase { heads } => {
            // `repos/{owner}/{repo}/pulls` and `repos/:owner/:repo/pulls` are
            // gh's own spellings for "the current directory's base
            // repository", which in a fork checkout is the upstream; resolved
            // the way `pr create` without `-R` is.
            let fork = upstream_target(registry, invocation, cwd)?;
            let branch = rest_head(&fork, &heads)?;
            Subject {
                repo: fork.name,
                branch,
                state: None,
            }
        }
        PullOpening::Graphql { head } => {
            // No owner to read: inside a registered fork's checkout — jj or a
            // plain git clone — the mutation is refused rather than guessed
            // at, since `gh pr create` says where it goes; outside one there
            // is no fork whose rule applies.
            let name = registered_fork_here(registry, cwd).ok_or(Early::Pass)?;
            return Err(Early::Refuse(format!(
                "a GraphQL createPullRequest names its repository by node id, so knives cannot \
                 tell whether it targets {name}'s upstream; open it with `gh pr create` (head {})",
                head.as_deref().unwrap_or("<branch>")
            )));
        }
    })
}

/// An early outcome of the gate: nothing to refuse, or the refusal.
#[derive(Debug)]
enum Early {
    Pass,
    Refuse(String),
}

/// The registered repository whose upstream this invocation addresses, or
/// why the gate stops: another repository or none at all (`Early::Pass` —
/// gh's own error to give), or a repository stated in a spelling knives does
/// not compare (`Early::Refuse`, naming the canonical one).
fn upstream_target(
    registry: &crate::config::Registry,
    invocation: &GhInvocation,
    cwd: &Path,
) -> Result<Fork, Early> {
    match target(invocation, cwd) {
        Target::Repo(url) => Fork::of(registry, &url)
            .ok_or(Early::Pass)?
            .map_err(Early::Refuse),
        Target::Absent => Err(Early::Pass),
        Target::Unreadable(refusal) => Err(Early::Refuse(refusal)),
    }
}

/// A registered fork whose upstream a creation addresses: its registry name
/// and the owner of its `origin`, the one owner whose `OWNER:BRANCH` head
/// names a branch the fork's ledger rules on.
struct Fork {
    name: crate::ids::RepoName,
    owner: String,
}

impl Fork {
    /// The registered fork whose `upstream` is `target`: `None` when none is,
    /// `Some(Err)` when one is but its `origin` names no owner knives can
    /// state a head with — the registry is trusted configuration, but a head
    /// qualified with an empty or non-canonical owner is one gh sends as the
    /// upstream's own branch, so the entry is refused with its remedy rather
    /// than read.
    fn of(registry: &crate::config::Registry, target: &str) -> Option<Result<Self, String>> {
        let (name, entry) = crate::bind::entry_for(registry, target)?;
        let origin = entry.remote(crate::config::Role::Origin);
        // The owner is read through the identity reader (`remote_slug`: a
        // host and an `owner/repo` path in any URL spelling `same_remote`
        // reads), so a spelling the registry binds by never names a
        // different owner here; a filesystem or `file://` origin names none.
        let owner = crate::remote_url::remote_slug(origin)
            .and_then(|slug| slug.split_once('/'))
            .map(|(owner, _)| owner)
            .filter(|owner| gh_canon::is_segment(owner));
        let Some(owner) = owner else {
            return Some(Err(format!(
                "the registry's origin for {name} ({origin}) names no owner knives can read: state \
                 it as https://<host>/OWNER/REPO in {}",
                crate::config::default_config_path().display()
            )));
        };
        Some(Ok(Self {
            name,
            owner: owner.to_owned(),
        }))
    }
}

impl std::fmt::Display for Fork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.name.fmt(f)
    }
}

/// The fork's branch the one stated head names, or none stated. To gh and
/// to GitHub's REST API alike a bare `BRANCH` is the base repository's own
/// branch and only `OWNER:BRANCH` names another repository's (measured
/// against gh 2.98.0: `--head feat/x` → `headRefName: "feat/x"` on the base;
/// `--head owner:feat/x` → `"owner:feat/x"`); so toward a registered
/// upstream a bare head is refused with the fork's spelling, `OWNER:BRANCH`
/// is read only when `OWNER` is the fork's own origin owner (the ledger
/// rules on no other repository's branches), and the branch must be one
/// knives compares. Two heads are refused rather than read last-wins; an
/// empty one likewise.
fn one_head(fork: &Fork, heads: &[String], spelling: &str) -> Result<Option<String>, Early> {
    let refuse = |text: String| Err(Early::Refuse(text));
    match heads {
        [] => Ok(None),
        [head] if head.is_empty() => refuse(format!(
            "an upstream pull request for {fork} states an empty head (`{spelling}=`): gh would \
             open the current branch, which knives cannot certify from that spelling; state \
             the branch as {}:<branch>",
            fork.owner
        )),
        [head] => match head.split_once(':') {
            None if gh_canon::is_branch(head) => refuse(format!(
                "an upstream pull request for {fork} states the head {spelling} {head:?} without \
                 an owner, which gh opens from the upstream's own branch, not this fork's: state \
                 it as {}:{head}",
                fork.owner
            )),
            None => refuse(gh_canon::head_refusal(&fork.to_string(), spelling, head)),
            Some((_, branch)) if !gh_canon::is_branch(branch) => {
                refuse(gh_canon::head_refusal(&fork.to_string(), spelling, head))
            }
            Some((owner, branch)) if owner.eq_ignore_ascii_case(&fork.owner) => {
                Ok(Some(branch.to_owned()))
            }
            Some((owner, branch)) => refuse(format!(
                "an upstream pull request for {fork} states the head {spelling} {head:?}, a branch \
                 of {owner}'s repository, which this fork's ledger never ruled on: state it as \
                 {}:{branch}, the fork's own branch (a cross-repository head cannot be checked \
                 here)",
                fork.owner
            )),
        },
        [first, .., last] => refuse(format!(
            "an upstream pull request for {fork} states {} heads ({}); gh would open {last} \
             while a reader expects {first}: state one",
            heads.len(),
            heads.join(", ")
        )),
    }
}

/// The fork's branch a REST creation states in its `head` field, by the same
/// rule as `--head` (GitHub's REST `head` is `owner:branch` for another
/// repository's branch too). Without one the creation is gh's error to
/// give; its verdict cannot be checked, and nothing unverifiable is let
/// through. (A body file, `--input`, was refused before this is reached.)
fn rest_head(fork: &Fork, heads: &[String]) -> Result<String, Early> {
    one_head(fork, heads, "-f head")?.ok_or_else(|| {
        Early::Refuse(format!(
            "an upstream pull request for {fork} needs a head branch (`-f head={}:<branch>`) to \
             check its placement verdict",
            fork.owner
        ))
    })
}

/// The registered fork whose checkout — colocated jj or plain git — `cwd`
/// is inside, by its `upstream` remote.
fn registered_fork_here(
    registry: &crate::config::Registry,
    cwd: &Path,
) -> Option<crate::ids::RepoName> {
    let root = crate::bind::checkout_root(cwd)?;
    let remotes = crate::bind::remotes(&root).ok()?;
    crate::bind::entry_for(registry, remotes.get("upstream")?).map(|(name, _)| name)
}

/// The refusal for a dash-argument gh's table for `command` does not define,
/// or a switch given an `=value` that is not a bool; `None` when every flag
/// is read. Whether an argument takes a value decides which arguments are
/// values, so with one unknown nothing about the command can be told; a
/// switch given `maybe` is gh's own `invalid argument` error, which knives
/// refuses rather than reads either way. The remedy is one an operator can
/// follow.
fn unreadable_flag_refusal(
    invocation: &GhInvocation,
    command: &str,
    table: &str,
) -> Option<String> {
    if let Some(unknown) = invocation.unknown.first() {
        return Some(format!(
            "knives does not know gh's flag {unknown} for {command}: if gh accepts it, add it to \
             {table} in src/commands/gh_args.rs; otherwise remove it"
        ));
    }
    let malformed = invocation.malformed.first()?;
    Some(format!(
        "{malformed} gives a switch a value that is not a bool (gh reads one of: {}); state one of \
         those, or drop the value",
        gh_canon::BOOL_SPELLINGS
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
                        return Some((match_offset, GraphqlValue::Literal(value)));
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

    /// The owner a `gh api` reading routes on, when it reads one at all.
    fn api_owner(invocation: &GhInvocation) -> Option<String> {
        owner_from_api_args(invocation).expect("an owner the grammar reads, or none")
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
            api_owner(&parsed(&args(&["api", "repos/acme/work/pulls"]))).as_deref(),
            Some("acme")
        );
        assert_eq!(
            api_owner(&parsed(&args(&["api", "/orgs/acme/teams"]))).as_deref(),
            Some("acme")
        );
        assert_eq!(
            api_owner(&parsed(&args(&["api", "users/someone"]))).as_deref(),
            Some("someone")
        );
        // Query strings on bare segments are stripped (shim line 99).
        assert_eq!(
            api_owner(&parsed(&args(&["api", "orgs/acme?page=2"]))).as_deref(),
            Some("acme")
        );
        // Placeholders expand from the current repo: no owner signal.
        assert_eq!(
            api_owner(&parsed(&args(&["api", "repos/{owner}/{repo}/pulls"]))),
            None
        );
        // A flag and its value are read as such; the path is the positional.
        assert_eq!(
            api_owner(&parsed(&args(&[
                "api",
                "-X",
                "POST",
                "repos/acme/work/issues"
            ])))
            .as_deref(),
            Some("acme")
        );
        // An empty owner segment is a signal knives cannot route on.
        for endpoint in ["repos/", "orgs/?page=2"] {
            assert!(
                owner_from_api_args(&parsed(&args(&["api", endpoint]))).is_err(),
                "{endpoint}"
            );
        }
        assert_eq!(
            api_owner(&parsed(&args(&["pr", "list", "repos/acme/work"]))),
            None
        );
    }

    #[test]
    fn the_first_path_shaped_argument_ends_the_rest_scan() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            api_owner(&parsed(&args(&[
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
            api_owner(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(owner: "acme", name: "work") { id } }"#,
            ])))
            .as_deref(),
            Some("acme")
        );
        assert_eq!(
            api_owner(&parsed(&args(&[
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
            api_owner(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                "query=mutation { addProjectV2ItemById(input: {}) { item { id } } }",
            ]))),
            None
        );
        assert_eq!(
            api_owner(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository (owner: "x") { id } }"#,
            ])))
            .as_deref(),
            Some("x")
        );
        // A literal outside the grammar is a signal knives cannot route on,
        // not a fall-through to the checkout's remotes.
        assert!(
            owner_from_api_args(&parsed(&args(&[
                "api",
                "graphql",
                "-f",
                r#"query=query { repository(owner: "ac me") { id } }"#,
            ])))
            .is_err()
        );
    }

    #[test]
    fn the_leftmost_graphql_owner_wins_across_both_patterns() {
        // Shim parity: its single alternation regex takes the FIRST match in
        // the text, whichever pattern it is.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            api_owner(&parsed(&args(&[
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
            api_owner(&parsed(&args(&[
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
                api_owner(&parsed(&args(&argv))).as_deref(),
                Some("acme"),
                "argv {argv:?}"
            );
        }
        // Variable names are matched whole: `$owner` is not bound by `owner_id=`.
        assert_eq!(
            api_owner(&parsed(&args(&[
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
            api_owner(&parsed(&args(&["api", "graphql", "-f", query]))),
            None
        );
        // A binding gh reads from a file or that is not an owner-shaped value is no signal.
        assert_eq!(
            api_owner(&parsed(&args(&[
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
            api_owner(&parsed(&args(&[
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
        // not run inside its checkout; -R beats it, as in gh. Both are read
        // in canonical form only.
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let host = DEFAULT_HOST;
        let scratch = tempfile::tempdir().expect("tempdir");
        with_env(&[("GH_REPO", "acme/work")], || {
            assert_eq!(
                target(&parsed(&args(&["pr", "list"])), scratch.path()),
                Target::Repo(format!("https://{host}/acme/work.git"))
            );
            assert_eq!(
                target(
                    &parsed(&args(&["api", "repos/{owner}/{repo}/pulls"])),
                    scratch.path()
                ),
                Target::Repo(format!("https://{host}/acme/work.git"))
            );
            assert_eq!(
                target(
                    &parsed(&args(&["pr", "list", "-R", "other/repo"])),
                    scratch.path()
                ),
                Target::Repo(format!("https://{host}/other/repo.git"))
            );
            // A path literal is the request's real target and still wins.
            assert_eq!(
                target(
                    &parsed(&args(&["api", "repos/literal/repo/pulls"])),
                    scratch.path()
                ),
                Target::Repo(format!("https://{host}/literal/gh-api.git"))
            );
        });
        with_env(&[("GH_REPO", "")], || {
            assert_eq!(
                target(&parsed(&args(&["pr", "list"])), scratch.path()),
                Target::Absent
            );
        });
        // A `GH_REPO` outside the grammar is unreadable, not guessed at.
        with_env(&[("GH_REPO", &format!("https://{host}/acme/work"))], || {
            assert_eq!(
                target(&parsed(&args(&["pr", "list"])), scratch.path()),
                Target::Unreadable(gh_canon::repo_refusal(
                    "GH_REPO",
                    &format!("https://{host}/acme/work")
                ))
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
                stated: None,
                resolved_remote: None,
                registered_entry: Some(&entry),
                remotes: &remotes,
            }),
            Target::Repo(format!("https://{host}/registered/upstream.git"))
        );
    }

    #[test]
    fn a_dangling_base_marker_does_not_fall_through_to_other_targets() {
        // Shim lines 151-164: a gh-resolved marker ends resolution even if its
        // named base remote can no longer produce a URL — unreadable, not the
        // next candidate.
        let host = DEFAULT_HOST;
        let entry = crate::config::RepoEntry::new(
            format!("git@{host}:registered/upstream"),
            format!("git@{host}:registered/origin"),
        );
        let remotes = BTreeMap::from([(
            "origin".to_owned(),
            format!("https://{host}/fallback/repository"),
        )]);

        let Target::Unreadable(why) = resolve_from_inputs(TargetInputs {
            api_owner: None,
            stated: None,
            resolved_remote: Some(ResolvedRemote {
                name: "missing",
                value: "base",
            }),
            registered_entry: Some(&entry),
            remotes: &remotes,
        }) else {
            panic!("a dangling base marker must be unreadable");
        };
        assert!(
            why.contains("remote.missing.gh-resolved = base names a remote with no URL"),
            "{why}"
        );
    }

    #[test]
    fn a_stated_repository_is_read_canonically_once_or_refused() {
        // `-R` is read as `OWNER/REPO` or `HOST/OWNER/REPO` and nothing else:
        // an empty value (which gh would read as "no -R"), a URL of any
        // scheme, a second `-R` are refused rather than read the way gh
        // would (round-7 code F2, F3 / deep H1).
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let host = DEFAULT_HOST;
        let read = |argv: &[&str]| stated_repository(&parsed(&args(argv)));
        assert_eq!(
            read(&["pr", "create", "-R", "acme/work"]),
            Some(Ok(Repo {
                host: host.to_owned(),
                owner: "acme".to_owned(),
                name: "work".to_owned(),
            }))
        );
        assert_eq!(
            read(&["pr", "create", "--repo=forge.example/acme/work"]),
            Some(Ok(Repo {
                host: "forge.example".to_owned(),
                owner: "acme".to_owned(),
                name: "work".to_owned(),
            }))
        );
        for (argv, text) in [
            (vec!["pr", "create", "-R", ""], ""),
            (vec!["pr", "create", "--repo="], ""),
            (vec!["pr", "create", "-R"], ""),
            (
                vec!["pr", "create", "-R", "https://github.example/acme/work"],
                "https://github.example/acme/work",
            ),
            (
                vec!["pr", "create", "-R", "git@github.example:acme/work.git"],
                "git@github.example:acme/work.git",
            ),
            (vec!["pr", "create", "-R", "acme/wo%72k"], "acme/wo%72k"),
            (vec!["pr", "create", "-R", "acme/work/"], "acme/work/"),
        ] {
            assert_eq!(
                read(&argv),
                Some(Err(gh_canon::repo_refusal("-R", text))),
                "{argv:?}"
            );
        }
        let Some(Err(two)) = read(&["pr", "create", "-R", "acme/work", "-R", "acme/other"]) else {
            panic!("two -R must be refused");
        };
        assert!(
            two.contains("states 2 repositories (-R \"acme/work\", \"acme/other\")"),
            "{two}"
        );
        assert!(two.contains("state one"), "{two}");
        assert_eq!(read(&["pr", "create"]), None);
        // An empty stated repository does not fall through to the remotes.
        let remotes = BTreeMap::from([(
            "origin".to_owned(),
            format!("https://{host}/fallback/repository"),
        )]);
        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                stated: Some(Err("refused".to_owned())),
                resolved_remote: None,
                registered_entry: None,
                remotes: &remotes,
            }),
            Target::Unreadable("refused".to_owned())
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
                api_owner: Some((DEFAULT_HOST, "api-owner")),
                stated: Some(Ok(Repo {
                    host: host.to_owned(),
                    owner: "explicit".to_owned(),
                    name: "repository".to_owned(),
                })),
                resolved_remote: Some(ResolvedRemote {
                    name: "origin",
                    value: "configured/repository",
                }),
                registered_entry: None,
                remotes: &remotes,
            }),
            Target::Repo(format!("https://{host}/api-owner/gh-api.git"))
        );
    }

    #[test]
    fn a_marker_value_is_owner_and_name_on_the_marked_remotes_host_or_unreadable() {
        // gh takes a non-`base` marker's owner and name from the value and its
        // host from the marked remote (round-7 code F5): a value carrying a
        // host, or anything but `OWNER/REPO`, is refused rather than read
        // with either host; a marker on a remote without a URL has no host
        // to take.
        let host = DEFAULT_HOST;
        let remotes = BTreeMap::from([(
            "origin".to_owned(),
            format!("https://{host}/decoy/other.git"),
        )]);
        let resolve = |value: &str, name: &str| {
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                stated: None,
                resolved_remote: Some(ResolvedRemote { name, value }),
                registered_entry: None,
                remotes: &remotes,
            })
        };
        assert_eq!(
            resolve("acme/work", "origin"),
            Target::Repo(format!("https://{host}/acme/work.git"))
        );
        for value in [
            "other.example/acme/work",
            &format!("https://{host}/acme/work"),
            "acme",
            "",
            "acme/wo%72k",
        ] {
            let Target::Unreadable(why) = resolve(value, "origin") else {
                panic!("{value:?} must be unreadable");
            };
            assert!(
                why.contains("remote.origin.gh-resolved is read only as `base` or `OWNER/REPO`"),
                "{value:?}: {why}"
            );
            assert!(why.contains("gh repo set-default"), "{value:?}: {why}");
        }
        let Target::Unreadable(why) = resolve("acme/work", "missing") else {
            panic!("a marker on a remote without a URL must be unreadable");
        };
        assert!(
            why.contains("names a remote with no URL to take the host from"),
            "{why}"
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
                stated: None,
                resolved_remote: Some(ResolvedRemote {
                    name: "zebra",
                    value: "base",
                }),
                registered_entry: None,
                remotes: &remotes,
            }),
            Target::Repo(format!("https://{host}/zebra/repository.git"))
        );
        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                stated: None,
                resolved_remote: None,
                registered_entry: None,
                remotes: &remotes,
            }),
            Target::Repo(format!("https://{host}/github/repository.git"))
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
                stated: None,
                resolved_remote: None,
                registered_entry: None,
                remotes: &remotes,
            }),
            Target::Repo(format!("https://{host}/alpha/repository.git"))
        );
        assert_eq!(
            resolve_from_inputs(TargetInputs {
                api_owner: None,
                stated: None,
                resolved_remote: None,
                registered_entry: None,
                remotes: &BTreeMap::new(),
            }),
            Target::Absent
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
            repo: Repo {
                host: DEFAULT_HOST.to_owned(),
                owner: "o".to_owned(),
                name: "r".to_owned(),
            },
            heads: vec!["feat/x".to_owned()],
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
        ] {
            assert_eq!(pull_opening(&parsed(&args(&argv))), rest, "{argv:?}");
        }
        // gh sends an absolute URL verbatim; the path is the same endpoint.
        let absolute = format!("https://api.{DEFAULT_HOST}/repos/o/r/pulls");
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
        let queried = pull_opening(&parsed(&args(&[
            "api",
            "-X",
            "POST",
            &format!("{absolute}?x=1"),
            "-f",
            "head=feat/x",
        ])));
        assert!(
            matches!(&queried, Some(PullOpening::Unreadable(why)) if why.contains("carries a query string")),
            "{queried:?}"
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
    fn a_body_file_makes_a_rest_creation_unreadable_whatever_its_fields() {
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        // `--input` moves every field to the query string and takes the body
        // from a file knives does not read: unreadable, whatever `-f head=`
        // sits beside it (round-9 H1).
        for argv in [
            vec![
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls",
                "--input",
                "pr.json",
                "-f",
                "head=o:feat/x",
            ],
            vec!["api", "repos/o/r/pulls", "--input=-"],
            vec![
                "api",
                "-X",
                "POST",
                "repos/{owner}/{repo}/pulls",
                "--input",
                "pr.json",
            ],
        ] {
            let opening = pull_opening(&parsed(&args(&argv)));
            assert!(
                matches!(&opening, Some(PullOpening::Unreadable(why)) if why.contains("takes its body from --input")),
                "{argv:?}: {opening:?}"
            );
        }
    }

    #[test]
    fn the_hostname_flag_is_the_host_a_rest_creation_addresses() {
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        // `--hostname` is the host the creation addresses (a GitHub
        // Enterprise upstream is registered by that host).
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "--hostname",
                "example.test",
                "/repos/o/r/pulls",
                "-f",
                "head=feat/x",
            ]))),
            Some(PullOpening::Rest {
                repo: Repo {
                    host: "example.test".to_owned(),
                    owner: "o".to_owned(),
                    name: "r".to_owned(),
                },
                heads: vec!["feat/x".to_owned()],
            })
        );
        let two_hosts = pull_opening(&parsed(&args(&[
            "api",
            "--hostname",
            "a.test",
            "--hostname",
            "b.test",
            "-X",
            "POST",
            "repos/o/r/pulls",
        ])));
        assert!(
            matches!(&two_hosts, Some(PullOpening::Unreadable(why)) if why.contains("--hostname a.test, --hostname b.test")),
            "{two_hosts:?}"
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
            repo: Repo {
                host: DEFAULT_HOST.to_owned(),
                owner: "o".to_owned(),
                name: "r".to_owned(),
            },
            heads: vec!["feat/x".to_owned()],
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
        // A `?query` on the creation endpoint is refused: GitHub may read it
        // over the body knives certifies the head from (round-10 deep L2).
        let queried = pull_opening(&parsed(&args(&[
            "api",
            "-X",
            "POST",
            "repos/o/r/pulls?a=1#x",
            "-f",
            "head=feat/x",
        ])));
        assert!(
            matches!(&queried, Some(PullOpening::Unreadable(why)) if why.contains("carries a query string")),
            "query and fragment: {queried:?}"
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
        let by_id = Some(PullOpening::Unreadable(
            "a pull request creation by numeric repository id (1318902388) cannot be checked \
             against the registry: state the repository as repos/<owner>/<repo>"
                .to_owned(),
        ));
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
            assert_eq!(api_owner(&parsed(&args(&argv))), None, "{argv:?}");
        }
    }

    #[test]
    fn a_help_flag_anywhere_opens_nothing() {
        // gh prints help and never reaches the API when `--help`/`-h` is
        // anywhere in the line, `pr create` and `gh api` alike; nothing past
        // it can open a pull request.
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        for argv in [
            vec!["pr", "create", "-R", "o/r", "-H", "feat/x", "--help"],
            vec!["pr", "create", "-R", "o/r", "-H", "feat/x", "-h"],
            vec!["pr", "create", "--help", "-R", "o/r"],
            vec![
                "api",
                "--help",
                "-X",
                "POST",
                "repos/o/r/pulls",
                "-f",
                "head=feat/x",
            ],
            vec![
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls",
                "-f",
                "head=feat/x",
                "-h",
            ],
            vec!["pr", "create", "-R", "o/r", "-H", "feat/x", "-h=false"],
            vec!["pr", "create", "--help=false", "--help", "-R", "o/r"],
        ] {
            assert_eq!(pull_opening(&parsed(&args(&argv))), None, "{argv:?}");
        }
        // `--help=false` is a switch given false: gh runs the command, so
        // the creation is read (round-7 code F1).
        for argv in [
            vec!["pr", "create", "-R", "o/r", "-H", "feat/x", "--help=false"],
            vec![
                "pr", "create", "--help", "--help=0", "-R", "o/r", "-H", "feat/x",
            ],
            vec!["--help=false", "pr", "create", "-H", "feat/x"],
        ] {
            assert_eq!(
                pull_opening(&parsed(&args(&argv))),
                Some(PullOpening::Create {
                    heads: vec!["feat/x".to_owned()]
                }),
                "{argv:?}"
            );
        }
        assert!(matches!(
            pull_opening(&parsed(&args(&[
                "api",
                "--help=false",
                "-X",
                "POST",
                "repos/o/r/pulls",
                "-f",
                "head=feat/x"
            ]))),
            Some(PullOpening::Rest { .. })
        ));
    }

    #[test]
    fn one_path_reader_serves_the_gate_and_token_routing() {
        // gh's placeholders, `{owner}` and `:owner` alike, name the current
        // directory's repository for the gate to resolve, and route no token;
        // an absolute `https://api.<host>/` URL is the same endpoint to both,
        // and any other absolute URL is a host knives does not compare.
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
                Some(PullOpening::RestAtBase { heads: Vec::new() }),
                "{placeholder}"
            );
            assert_eq!(api_owner(&parsed(&args(&["api", &placeholder]))), None);
            assert!(is_placeholder(&owner, "owner"), "{owner}");
        }
        assert!(!is_placeholder("routed-a", "owner"));
        assert!(!is_placeholder("{Owner}", "owner"));
        assert!(!is_placeholder(":ownerx", "owner"));
        let absolute = format!("https://api.{DEFAULT_HOST}/repos/o/r/pulls");
        assert_eq!(
            api_owner(&parsed(&args(&["api", &absolute]))).as_deref(),
            Some("o")
        );
        // A percent-escaped or empty owner segment is a signal knives cannot
        // route on: unreadable, not a fall-through to the checkout's remotes.
        for endpoint in ["repos/%72outed-a/upstream/pulls", "repos//upstream/pulls"] {
            let read = owner_from_api_args(&parsed(&args(&["api", endpoint])));
            assert!(
                matches!(&read, Err(why) if why.contains("outside the grammar knives routes by")),
                "{endpoint}: {read:?}"
            );
        }
        // A mixed placeholder-and-literal, a placeholder outside gh's own
        // spelling, or a foreign absolute URL is unreadable on a POST.
        let (brace_owner, brace_owner_cased, brace_repo) = (
            format!("{{{}}}", "owner"),
            format!("{{{}}}", "Owner"),
            format!("{{{}}}", "repo"),
        );
        for endpoint in [
            &format!("repos/{brace_owner}/literal/pulls"),
            &format!("repos/{brace_owner_cased}/{brace_repo}/pulls"),
            "repos/o/r%65po/pulls",
            "repos//r/pulls",
            "repos/o/r/pulls/",
            &format!("https://API.{DEFAULT_HOST}/repos/o/r/pulls"),
            &format!("http://api.{DEFAULT_HOST}/repos/o/r/pulls"),
            "https://other.example/repos/o/r/pulls",
        ] {
            let opening = pull_opening(&parsed(&args(&["api", "-X", "POST", endpoint])));
            assert!(
                matches!(opening, Some(PullOpening::Unreadable(_))),
                "{endpoint}: {opening:?}"
            );
        }
        // The same foreign URL on a GET opens nothing and is not refused.
        assert_eq!(
            pull_opening(&parsed(&args(&[
                "api",
                "https://other.example/repos/o/r/pulls"
            ]))),
            None
        );
    }

    #[test]
    fn two_methods_are_refused_and_one_is_read_case_insensitively() {
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        let opening = pull_opening(&parsed(&args(&[
            "api",
            "-X",
            "GET",
            "-X",
            "POST",
            "repos/o/r/pulls",
            "-f",
            "head=feat/x",
        ])));
        let Some(PullOpening::Unreadable(why)) = opening else {
            panic!("two methods must be unreadable: {opening:?}");
        };
        assert!(why.contains("states 2 methods (-X GET, -X POST)"), "{why}");
        assert!(why.contains("state one"), "{why}");
        for method in ["post", "Post", "POST"] {
            assert!(
                matches!(
                    pull_opening(&parsed(&args(&["api", "-X", method, "repos/o/r/pulls"]))),
                    Some(PullOpening::Rest { .. })
                ),
                "{method}"
            );
        }
        assert_eq!(
            pull_opening(&parsed(&args(&["api", "-X", "PUT", "repos/o/r/pulls"]))),
            None
        );
    }

    #[test]
    fn graphql_is_read_case_insensitively_and_a_percent_escaped_post_is_refused() {
        // GitHub's `graphql` handler answers to a case variant; a
        // percent-escape anywhere in a POST's path is refused rather than
        // decoded (GitHub decodes it, knives does not).
        let args = |arguments: &[&str]| {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect::<Vec<_>>()
        };
        let mutation = "mutation { createPullRequest(input:{repositoryId:\"x\",headRefName:$h}) { pullRequest { id } } }";
        for endpoint in ["GRAPHQL", "Graphql", "graphql"] {
            assert_eq!(
                pull_opening(&parsed(&args(&[
                    "api",
                    endpoint,
                    "-f",
                    &format!("query={mutation}"),
                    "-f",
                    "headRefName=feat/x",
                ]))),
                Some(PullOpening::Graphql {
                    head: Some("feat/x".to_owned())
                }),
                "{endpoint}"
            );
        }
        for endpoint in ["gra%70hql", "repos/o/r/pull%73", "repos/o/r/pulls%2F"] {
            let opening = pull_opening(&parsed(&args(&["api", endpoint, "-f", "head=feat/x"])));
            let Some(PullOpening::Unreadable(why)) = opening else {
                panic!("{endpoint}: {opening:?}");
            };
            assert!(why.contains("is percent-encoded"), "{endpoint}: {why}");
            assert!(
                why.contains("state the endpoint unencoded"),
                "{endpoint}: {why}"
            );
        }
        // The same escaped path on a GET is nothing to the gate.
        assert_eq!(
            pull_opening(&parsed(&args(&["api", "-X", "GET", "repos/o/r/pull%73"]))),
            None
        );
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
        // `OWNER:BRANCH` is kept as written: the gate refuses it as a branch
        // of another repository rather than reading the branch alone.
        assert_eq!(
            pull_opening(&parsed(&args(&["pr", "create", "--head", "o:feat/x"]))),
            Some(PullOpening::Create {
                heads: vec!["o:feat/x".to_owned()],
            })
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
