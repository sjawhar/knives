//! A fake `gh` speaking the snapshot protocol: identity, cold list, warm sweep, by-number facts — plus an always-failing variant. Shared by every integration test that drives the real binary against a scripted forge.

#![allow(
    clippy::expect_used,
    reason = "a fixture that cannot proceed IS the test failure"
)]
#![allow(
    unreachable_pub,
    dead_code,
    reason = "a test fixture included by path; not every test target uses every helper"
)]

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Install a fake `gh` that answers the full snapshot protocol: repository
/// identity, cold list, warm sweep, and by-number facts.
pub fn install_snapshot_gh(shim: &Path, pulls: &str, withheld_facts: &[u64]) {
    install_snapshot_gh_configured(
        shim,
        pulls,
        SnapshotGhOptions {
            withheld_facts,
            timeline_nodes: None,
            refused_head_refs: &[],
        },
    );
}

/// Install the snapshot shim with an optional canned pull-request timeline.
pub fn install_snapshot_gh_with_timeline(shim: &Path, pulls: &str, timeline_nodes: Option<&str>) {
    install_snapshot_gh_configured(
        shim,
        pulls,
        SnapshotGhOptions {
            withheld_facts: &[],
            timeline_nodes,
            refused_head_refs: &[],
        },
    );
}

/// What the forge says, in `errors` and on gh's stderr, when the head
/// repository's organization enforces SAML against the token.
const SAML_REFUSAL: &str = "Resource protected by organization SAML enforcement. \
     You must grant your Personal Access token access to this organization.";

/// Install the snapshot shim with the head ref of each number in
/// `refused_head_refs` refused the way a fork whose organization enforces SAML
/// refuses the token: the facts reply nulls that `headRef`, names it in
/// `errors`, and gh exits 1 after printing the reply.
pub fn install_snapshot_gh_refusing_head_refs(shim: &Path, pulls: &str, refused_head_refs: &[u64]) {
    install_snapshot_gh_configured(
        shim,
        pulls,
        SnapshotGhOptions {
            withheld_facts: &[],
            timeline_nodes: None,
            refused_head_refs,
        },
    );
}

#[derive(Clone, Copy)]
struct SnapshotGhOptions<'a> {
    withheld_facts: &'a [u64],
    timeline_nodes: Option<&'a str>,
    refused_head_refs: &'a [u64],
}

fn install_snapshot_gh_configured(shim: &Path, pulls: &str, options: SnapshotGhOptions<'_>) {
    let SnapshotGhOptions {
        withheld_facts,
        timeline_nodes,
        refused_head_refs,
    } = options;
    let pulls: Vec<serde_json::Value> =
        serde_json::from_str(pulls).expect("parse fake pull request payload");
    let sweep_entries = pulls
        .iter()
        .map(|pull| {
            serde_json::json!({
                "number": pull.get("number"),
                "updatedAt": pull.get("updatedAt"),
                "state": pull.get("state"),
            })
        })
        .collect::<Vec<_>>();
    let facts_reply = facts_reply(&pulls, withheld_facts, refused_head_refs);
    let identity = shim.join("identity.json");
    let list = shim.join("pulls.json");
    let sweep = shim.join("sweep.json");
    let facts_payload = shim.join("facts.json");
    let timeline_payload = shim.join("timeline.json");
    // The head owner is read over REST, one page of the list and one call per
    // number the page missed (src/forge/github.rs).
    let (rest_page, rest_pulls) = write_rest_payloads(shim, &pulls);
    // gh exits 1 whenever the reply names errors, after printing it.
    let facts_answer = if facts_reply.get("errors").is_none() {
        format!("cat \"{}\"", facts_payload.display())
    } else {
        format!(
            "cat \"{}\"; echo \"gh: {SAML_REFUSAL}\" >&2; exit 1",
            facts_payload.display()
        )
    };
    std::fs::write(
        &identity,
        serde_json::to_vec(&serde_json::json!({
            "nameWithOwner": "fake-owner/fake-repo",
            "id": "FAKEID",
        }))
        .expect("serialize identity payload"),
    )
    .expect("write identity payload");
    std::fs::write(
        &list,
        serde_json::to_vec(&pulls).expect("serialize list payload"),
    )
    .expect("write pull request payload");
    std::fs::write(
        &sweep,
        serde_json::to_vec(&serde_json::json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "pageInfo": {"hasNextPage": false},
                        "nodes": sweep_entries,
                    }
                }
            }
        }))
        .expect("serialize sweep payload"),
    )
    .expect("write sweep payload");
    std::fs::write(
        &facts_payload,
        serde_json::to_vec(&facts_reply).expect("serialize facts payload"),
    )
    .expect("write facts payload");
    write_timeline_payload(&timeline_payload, timeline_nodes);
    let gh = shim.join("gh");
    install_executable(
        &gh,
        &snapshot_gh_script(&ShimAnswers {
            identity: &identity,
            list: &list,
            timeline: &timeline_payload,
            facts: &facts_answer,
            sweep: &sweep,
            rest_page: &rest_page,
            rest_pulls: &rest_pulls,
        }),
    );
}

/// Where the fake gh reads each answer from; `facts` is a shell fragment,
/// since a facts reply naming errors also writes stderr and exits 1.
struct ShimAnswers<'a> {
    identity: &'a Path,
    list: &'a Path,
    timeline: &'a Path,
    facts: &'a str,
    sweep: &'a Path,
    rest_page: &'a Path,
    rest_pulls: &'a Path,
}

/// The fake gh itself: the snapshot protocol over GraphQL, and the head-owner
/// read over REST — one page of the list, one call per number, 404 for a
/// number this forge does not have.
fn snapshot_gh_script(answers: &ShimAnswers<'_>) -> String {
    format!(
        "#!/bin/sh\ncase \" $* \" in\n\
         *\" repo view \"*) cat \"{}\" ;;\n\
         *\" pr list \"*) cat \"{}\" ;;\n\
         *\" api graphql \"*)\n\
           case \"$*\" in\n\
             *\"timelineItems\"*) cat \"{}\" ;;\n\
             *\"pullRequest(number:\"*) {} ;;\n\
             *\"orderBy: {{field: UPDATED_AT\"*) cat \"{}\" ;;\n\
             *) echo \"unexpected GraphQL invocation: $*\" >&2; exit 1 ;;\n\
           esac ;;\n\
         *\" api \"*)\n\
           case \"$2\" in\n\
             *\"/pulls?\"*)\n\
               case \"$2\" in\n\
                 *\"&page=1\"*) cat \"{}\" ;;\n\
                 *) echo '[]' ;;\n\
               esac ;;\n\
             *\"/pulls/\"*)\n\
               number=\"${{2##*/}}\"\n\
               if [ -f \"{}/$number.json\" ]; then\n\
                 cat \"{}/$number.json\"\n\
               else\n\
                 printf '%s' '{{\"message\":\"Not Found\",\"status\":\"404\"}}'\n\
                 echo 'gh: Not Found (HTTP 404)' >&2\n\
                 exit 1\n\
               fi ;;\n\
             *) echo \"unexpected gh api invocation: $*\" >&2; exit 1 ;;\n\
           esac ;;\n\
         *) echo \"unexpected gh invocation: $*\" >&2; exit 1 ;;\n\
         esac\n",
        answers.identity.display(),
        answers.list.display(),
        answers.timeline.display(),
        answers.facts,
        answers.sweep.display(),
        answers.rest_page.display(),
        answers.rest_pulls.display(),
        answers.rest_pulls.display(),
    )
}

/// Write what the REST head-owner read asks for: one page of the list, and
/// one file per pull request. Returns the page and the directory.
fn write_rest_payloads(shim: &Path, pulls: &[serde_json::Value]) -> (PathBuf, PathBuf) {
    let page = shim.join("rest-page.json");
    let directory = shim.join("rest");
    std::fs::create_dir_all(&directory).expect("create REST pull directory");
    let rows = pulls.iter().map(rest_row).collect::<Vec<_>>();
    for row in &rows {
        let number = row["number"].as_u64().expect("pull request number");
        std::fs::write(
            directory.join(format!("{number}.json")),
            serde_json::to_vec(row).expect("serialize REST pull request"),
        )
        .expect("write REST pull request");
    }
    std::fs::write(
        &page,
        serde_json::to_vec(&rows).expect("serialize REST page"),
    )
    .expect("write REST page");
    (page, directory)
}

/// The REST shape of one pull request, read for its head owner alone: a row
/// stating no owner has no head repository, as a deleted one does.
fn rest_row(pull: &serde_json::Value) -> serde_json::Value {
    let owner = pull
        .get("headRepositoryOwner")
        .and_then(|owner| owner.get("login"))
        .cloned();
    serde_json::json!({
        "number": pull.get("number"),
        "head": {
            "ref": pull.get("headRefName"),
            "repo": owner.map(|login| serde_json::json!({"owner": {"login": login}})),
        },
    })
}

/// The by-number facts reply: one row per pull not withheld, each refused head
/// ref nulled and named in `errors`.
fn facts_reply(
    pulls: &[serde_json::Value],
    withheld_facts: &[u64],
    refused_head_refs: &[u64],
) -> serde_json::Value {
    let mut fact_rows = serde_json::Map::new();
    let mut fact_errors = Vec::new();
    for pull in pulls {
        let number = pull["number"].as_u64().expect("pull request number");
        if !withheld_facts.contains(&number) {
            let alias = format!("p{number}");
            let mut row = pull.clone();
            if refused_head_refs.contains(&number) {
                row["headRef"] = serde_json::Value::Null;
                fact_errors.push(serde_json::json!({
                    "type": "FORBIDDEN",
                    "path": ["repository", &alias, "headRef"],
                    "extensions": {"saml_failure": true, "ip_allow_list_failure": false},
                    "message": SAML_REFUSAL,
                }));
            }
            let _ = fact_rows.insert(alias, row);
        }
    }
    let mut reply = serde_json::json!({
        "data": {"repository": fact_rows},
    });
    if !fact_errors.is_empty() {
        reply["errors"] = serde_json::Value::Array(fact_errors);
    }
    reply
}

fn write_timeline_payload(path: &Path, timeline_nodes: Option<&str>) {
    let timeline_nodes: Vec<serde_json::Value> = timeline_nodes.map_or_else(Vec::new, |nodes| {
        serde_json::from_str(nodes).expect("parse fake timeline payload")
    });
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {
                            "pageInfo": {"hasPreviousPage": false},
                            "nodes": timeline_nodes,
                        }
                    }
                }
            }
        }))
        .expect("serialize timeline payload"),
    )
    .expect("write timeline payload");
}

pub fn install_failing_gh(shim: &Path, log: &Path) {
    let gh = shim.join("gh");
    install_executable(
        &gh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 1\n",
            log.display()
        ),
    );
}

/// Install the snapshot shim behind a gate standing in for a gh wrapper that holds
/// one owner's token, in a checkout whose remotes name more than one owner: the gate
/// appends each call's arguments to `log`, and refuses (exit 2, and a `REFUSED` line
/// in `log`) every call that names no repository, since gh would take the
/// repository from those remotes and the wrapper cannot tell whose token to send. It
/// reads the argument shapes knives sends, not every gh syntax: a call names a
/// repository by a positional argument to `repo view`, by `-R` or `--repo` on any
/// other command, by a GraphQL call's `owner` field, or by an `api repos/...`
/// endpoint; a call that names one reaches the snapshot shim. A test that uses it
/// also checks the exact repository each call names.
pub fn install_repository_requiring_gh(shim: &Path, pulls: &str, log: &Path) {
    let answers = shim.join("answers");
    std::fs::create_dir_all(&answers).expect("create the snapshot shim directory");
    install_snapshot_gh(&answers, pulls, &[]);
    install_executable(
        &shim.join("gh"),
        &format!(
            "#!/bin/sh\n\
             log=\"{log}\"\n\
             printf '%s\\n' \"$*\" >> \"$log\"\n\
             named=\n\
             previous=\n\
             case \"$1 $2\" in\n\
             \"repo view\") case \"$3\" in ''|-*) ;; *) named=yes ;; esac ;;\n\
             \"api graphql\")\n\
               for argument in \"$@\"; do\n\
                 case \"$previous $argument\" in \"-f owner=\"?*|\"-F owner=\"?*) named=yes ;; esac\n\
                 previous=$argument\n\
               done ;;\n\
             \"api repos/\"*) named=yes ;;\n\
             *)\n\
               for argument in \"$@\"; do\n\
                 case \"$previous\" in -R|--repo) [ -n \"$argument\" ] && named=yes ;; esac\n\
                 case \"$argument\" in --repo=?*) named=yes ;; esac\n\
                 previous=$argument\n\
               done ;;\n\
             esac\n\
             if [ -z \"$named\" ]; then\n\
               printf 'REFUSED %s\\n' \"$*\" >> \"$log\"\n\
               echo 'gh wrapper: the command names no repository and the remotes of the working \
             directory name more than one owner, so it sends no token; name one repository' >&2\n\
               exit 2\n\
             fi\n\
             exec \"{answer}\" \"$@\"\n",
            log = log.display(),
            answer = answers.join("gh").display(),
        ),
    );
}

pub fn path_with_gh_shim(shim: &Path) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(shim.to_owned()).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH is set"),
        )),
    )
    .expect("construct shim PATH")
}

/// One forge repository a [`install_repository_gh`] shim answers for: the
/// `<owner>/<name>` a call names it by, the `nameWithOwner` the forge gives
/// it, and its pull requests ([`pull_record`] rows, as a JSON array).
pub struct Repository<'a> {
    pub named: &'a str,
    pub canonical: &'a str,
    pub pulls: &'a str,
}

/// Install a fake `gh` that answers each of `repositories` apart, as a forge
/// does, matching a repository's name whatever its letter case, as GitHub
/// does: `repo view <owner>/<name>` gives that repository's identity, `pr
/// list -R <owner>/<name>` its pull requests, and a by-number facts query
/// (`api graphql -f owner=… -f name=…`) the pull requests it asks about of
/// the repository its `owner` and `name` fields name, a number that
/// repository does not have, or the query does not ask about, being absent
/// from the reply. A status snapshot's warm sweep finds nothing. A call
/// naming any other repository fails as gh does for one that does not
/// exist, or that the token cannot see, but a list of one is empty. A pull
/// request row may repeat a field to replace [`pull_record_with_fields`]'s
/// default: the last one is kept. Each call's arguments are appended to
/// `log`.
pub fn install_repository_gh(shim: &Path, repositories: &[Repository<'_>], log: &Path) {
    install_repository_gh_failing(shim, repositories, &[], log);
}

/// [`install_repository_gh`], but every call naming one of `failing` (an
/// `<owner>/<name>`) fails as an outage does, `HTTP 502`, not as a name the
/// forge cannot resolve.
pub fn install_repository_gh_failing(
    shim: &Path,
    repositories: &[Repository<'_>],
    failing: &[&str],
    log: &Path,
) {
    let sweep = json_file(
        shim,
        "sweep.json",
        &serde_json::json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "pageInfo": {"hasNextPage": false},
                        "nodes": [],
                    }
                }
            }
        }),
    );
    let mut views = String::new();
    let mut lists = String::new();
    let mut facts = String::new();
    let outage = "echo \"HTTP 502: Bad Gateway (https://api.github.com/graphql)\" >&2; exit 1 ;;";
    for repository in failing {
        let repository = repository.to_ascii_lowercase();
        let (owner, name) = repository
            .split_once('/')
            .expect("a repository is named <owner>/<name>");
        writeln!(views, "  {repository}) {outage}").expect("write to a string");
        writeln!(lists, "  {repository}) {outage}").expect("write to a string");
        writeln!(facts, "  \"{owner} {name}\") {outage}").expect("write to a string");
    }
    for (index, repository) in repositories.iter().enumerate() {
        let named = repository.named.to_ascii_lowercase();
        let (owner, name) = named
            .split_once('/')
            .expect("a repository is named <owner>/<name>");
        let identity = json_file(
            shim,
            &format!("identity-{index}.json"),
            &serde_json::json!({
                "nameWithOwner": repository.canonical,
                "id": format!("ID{index}"),
            }),
        );
        let pulls: Vec<serde_json::Value> =
            serde_json::from_str(repository.pulls).expect("parse fake pull request payload");
        let list = json_file(shim, &format!("list-{index}.json"), &pulls);
        cat_arm(&mut lists, &named, &list);
        cat_arm(&mut views, &named, &identity);
        facts.push_str(&facts_arm(shim, (owner, name), &pulls, &sweep));
    }
    install_executable(
        &shim.join("gh"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             owner=\n\
             name=\n\
             previous=\n\
             for argument in \"$@\"; do\n\
               case \"$previous $argument\" in\n\
               \"-f owner=\"*) owner=${{argument#owner=}} ;;\n\
               \"-f name=\"*) name=${{argument#name=}} ;;\n\
               esac\n\
               previous=$argument\n\
             done\n\
             case \"$1 $2\" in\n\
             \"repo view\")\n\
               case \"$(printf '%s' \"$3\" | tr 'A-Z' 'a-z')\" in\n\
             {views}\
               esac\n\
               echo \"GraphQL: Could not resolve to a Repository with the name '$3'.\" >&2\n\
               exit 1 ;;\n\
             \"pr list\")\n\
               case \"$(printf '%s' \"$4\" | tr 'A-Z' 'a-z')\" in\n\
             {lists}\
               esac\n\
               echo '[]'; exit 0 ;;\n\
             \"api graphql\")\n\
               case \"$(printf '%s %s' \"$owner\" \"$name\" | tr 'A-Z' 'a-z')\" in\n\
             {facts}\
               esac\n\
               echo \"GraphQL: Could not resolve to a Repository with the name '$owner/$name'.\" >&2\n\
               exit 1 ;;\n\
             esac\n\
             echo \"unexpected gh invocation: $*\" >&2\n\
             exit 1\n",
            log = log.display(),
        ),
    );
}

/// The `case` arm that answers a by-number facts query on `owner`/`name`,
/// whose pull requests are `pulls`, for the numbers the query asks about,
/// as GitHub answers it: one fragment per pull request, put together for
/// the `pullRequest(number: N)` fields the query holds. A reinstall can hide
/// a repository, so the fragments are kept by the repository's name, and
/// cleared first.
fn facts_arm(
    shim: &Path,
    (owner, name): (&str, &str),
    pulls: &[serde_json::Value],
    sweep: &Path,
) -> String {
    let dir = shim.join(format!("facts-{owner}-{name}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear the facts directory");
    }
    std::fs::create_dir_all(&dir).expect("create the facts directory");
    for pull in pulls {
        let number = pull["number"].as_u64().expect("pull request number");
        std::fs::write(
            dir.join(format!("{number}.json")),
            format!(
                "\"p{number}\":{}",
                serde_json::to_string(pull).expect("serialize a fake pull request")
            ),
        )
        .expect("write a fake pull request's facts");
    }
    format!(
        "  \"{owner} {name}\")\n\
         \x20   case \"$*\" in\n\
         \x20   *\"pullRequest(number:\"*)\n\
         \x20     printf '{{\"data\":{{\"repository\":{{'\n\
         \x20     separator=\n\
         \x20     for number in $(printf '%s\\n' \"$*\" | grep -o 'pullRequest(number: [0-9]*' | grep -o '[0-9]*$'); do\n\
         \x20       if [ -f \"{dir}/$number.json\" ]; then\n\
         \x20         printf '%s' \"$separator\"; cat \"{dir}/$number.json\"; separator=,\n\
         \x20       fi\n\
         \x20     done\n\
         \x20     printf '}}}}}}\\n'; exit 0 ;;\n\
         \x20   *\"orderBy: {{field: UPDATED_AT\"*) cat \"{sweep}\"; exit 0 ;;\n\
         \x20   esac ;;\n",
        dir = dir.display(),
        sweep = sweep.display(),
    )
}

/// Write `value` as JSON to `name` in `shim`; its path.
fn json_file(shim: &Path, name: &str, value: &impl serde::Serialize) -> std::path::PathBuf {
    let path = shim.join(name);
    std::fs::write(
        &path,
        serde_json::to_vec(value).expect("serialize a fake forge payload"),
    )
    .expect("write a fake forge payload");
    path
}

/// Append to `arms` a shell `case` arm that prints `path` for `pattern`.
fn cat_arm(arms: &mut String, pattern: &str, path: &Path) {
    writeln!(arms, "  {pattern}) cat \"{}\"; exit 0 ;;", path.display())
        .expect("write to a string");
}

/// One forge pull request record with the fields the binary requires.
pub fn pull_record(number: u64, state: &str, branch: &str, merge_oid: Option<&str>) -> String {
    let merge = merge_oid.map_or_else(String::new, |oid| {
        format!(",\"mergeCommit\":{{\"oid\":\"{oid}\"}}")
    });
    pull_record_with_fields(number, state, branch, &merge)
}

/// One forge pull request record with additional raw JSON fields.
///
/// `extra_fields` begins with a comma when non-empty, so test scenarios can
/// add facts to a single row without changing existing callers.
pub fn pull_record_with_fields(
    number: u64,
    state: &str,
    branch: &str,
    extra_fields: &str,
) -> String {
    format!(
        "{{\"number\":{number},\"state\":\"{state}\",\"headRefName\":\"{branch}\",\
         \"headRefOid\":\"0123456789abcdef0123456789abcdef01234567\",\
         \"updatedAt\":\"2026-08-07T00:00:00Z\",\"baseRefName\":\"main\"{extra_fields}}}"
    )
}

fn install_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write gh shim");
    let mut permissions = std::fs::metadata(path)
        .expect("read gh shim permissions")
        .permissions();
    PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(path, permissions).expect("make gh shim executable");
}
