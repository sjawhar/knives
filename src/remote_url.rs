//! What a remote URL names, whatever its spelling.
//!
//! Identity and trust both compare remote URLs as their `(host, path)`: `https`
//! and `ssh` forms, a user before the host, a port after it, a trailing `/` or
//! `.git`, and letter case are all spellings of one repository. A value that is
//! not a URL — a filesystem path, or a `file://` URL, whose authority is empty
//! — is only ever equal to itself.

/// Whether the remote spelling `stated` names the repository `registered`
/// names.
///
/// A value that parses as a remote URL with a host compares as its
/// [`host_and_path`]: the paths case-insensitively, the hosts by
/// [`same_host`] — `stated`'s host may be a subdomain spelling of
/// `registered`'s. A value that does not (a filesystem path, or a `file://`
/// URL, whose authority is empty) compares as its trimmed text, so two
/// directories that differ by `.git` stay two directories.
pub fn same_remote(registered: &str, stated: &str) -> bool {
    match (host_and_path(registered), host_and_path(stated)) {
        (Some((host_r, path_r)), Some((host_s, path_s))) => {
            same_host(host_s, host_r) && path_s.eq_ignore_ascii_case(path_r)
        }
        (None, None) => {
            registered.trim().trim_end_matches('/') == stated.trim().trim_end_matches('/')
        }
        _ => false,
    }
}

/// Whether the host spelling `stated` names the forge `registered` names.
///
/// Equal, or a subdomain of it — `www.github.com`, `api.github.com`,
/// `foo.github.com` are all `github.com` — case-insensitively, a trailing
/// `.` ignored, and a leading `www.` folded off both sides first: the
/// registered side is trusted configuration, but a registry that spells its
/// upstream on `www.github.com` names the same repository gh's own
/// `normalizeHostname` reads, and must match the canonical `o/r`.
///
/// gh folds every `*.github.com` to `github.com` (and `*.<tenant>.ghe.com`,
/// `*.localhost` likewise) when choosing the token and the endpoint; this is
/// the superset of that fold — any subdomain of a registered host is that
/// host — so a spelling gh would send to a registered upstream is never
/// compared as some other host. The cost is over-matching a distinct host
/// that happens to be a subdomain of a registered one, which gates and
/// routes more, never less.
pub fn same_host(stated: &str, registered: &str) -> bool {
    let stated = without_www(stated.trim_end_matches('.'));
    let registered = without_www(registered.trim_end_matches('.'));
    stated.eq_ignore_ascii_case(registered)
        || (stated.len() > registered.len() + 1
            && stated.as_bytes().get(stated.len() - registered.len() - 1) == Some(&b'.')
            && stated
                .get(stated.len() - registered.len()..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(registered)))
}

/// `host` less one leading `www.` in any case.
fn without_www(host: &str) -> &str {
    match host.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("www.") => &host[4..],
        _ => host,
    }
}

/// `(authority, path)` of a remote in any URL spelling — `scheme://…`,
/// `user@host:path`, or the user-less scp form `host:path` when the part
/// before the colon holds no `/` (a filesystem path with a colon in a later
/// component stays a path) — the authority possibly empty. The one reader
/// [`host_and_path`] and [`url_owner`] share, so identity and the owner a
/// head is qualified with never disagree about a spelling.
fn authority_and_path(remote: &str) -> Option<(&str, &str)> {
    let trimmed = remote.trim().trim_end_matches('/');
    remote_authority_and_path(trimmed).or_else(|| {
        let (host, path) = trimmed.split_once(':')?;
        (!host.is_empty() && !host.contains('/')).then_some((host, path))
    })
}

/// `(host, path)` of a remote URL as spelled: the authority without its user
/// or port, and the path without its query or fragment, surrounding `/`, or
/// a `.git` suffix. `None` for a non-URL: a filesystem path, or a `file://`
/// URL, whose authority is empty.
fn host_and_path(remote: &str) -> Option<(&str, &str)> {
    let (authority, path) = authority_and_path(remote)?;
    if authority.is_empty() {
        return None;
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // `host:2222` is `host`: the port is how to reach it, not what it is.
    let host = match host.rsplit_once(':') {
        Some((name, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    };
    // A query string or `#fragment` is never part of a repository's
    // identity; cut before trimming slashes or the `.git` suffix so
    // `owner/repo?x=1` and `owner/repo#frag` compare as `owner/repo`.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    Some((host, without_git_suffix(path.trim_matches('/'))))
}

/// `path` without a trailing `.git` in any case.
const fn without_git_suffix(path: &str) -> &str {
    match path.split_at_checked(path.len().saturating_sub(4)) {
        Some((stem, suffix)) if suffix.eq_ignore_ascii_case(".git") => stem,
        _ => path,
    }
}

/// `(authority, path)` of `scheme://authority/path` or `user@authority:path`;
/// `None` otherwise.
pub fn remote_authority_and_path(url: &str) -> Option<(&str, &str)> {
    let url = url.trim_end_matches('/');
    if let Some((_, authority_and_path)) = url.split_once("://") {
        return authority_and_path.split_once('/');
    }
    let (authority, path) = url.split_once(':')?;
    authority.contains('@').then_some((authority, path))
}

/// `url` with its ssh host alias resolved the way go-gh's ssh translator
/// resolves it.
///
/// For an ssh remote (`ssh://…`, `git+ssh://…`, or the scp form
/// `[user@]host:path`) the host is what `ssh -G <host>` answers as
/// `hostname` — an ssh-config `Host` alias names the host it stands for;
/// `ssh.github.com` folds to `github.com` as gh folds it. Any other URL, or
/// any failure — no `ssh` on PATH, a non-zero exit, no `hostname` line —
/// leaves the URL as written (gh's own fallback). `cache` remembers each
/// host's answer for the caller's run.
pub fn with_ssh_alias_resolved(
    url: &str,
    cache: &mut std::collections::BTreeMap<String, String>,
) -> String {
    let Some((authority, _)) = authority_and_path(url) else {
        return url.to_owned();
    };
    let is_ssh = url.starts_with("ssh://") || url.starts_with("git+ssh://") || !url.contains("://");
    if !is_ssh || authority.is_empty() {
        return url.to_owned();
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host.rsplit_once(':') {
        Some((name, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    };
    if host.is_empty() {
        return url.to_owned();
    }
    let resolved = cache
        .entry(host.to_ascii_lowercase())
        .or_insert_with(|| ssh_hostname(host).unwrap_or_else(|| host.to_owned()))
        .clone();
    if resolved.eq_ignore_ascii_case(host) {
        return url.to_owned();
    }
    // The host is a subslice of `url`; splice the answer in its place.
    let offset = host.as_ptr() as usize - url.as_ptr() as usize;
    let mut rewritten = String::with_capacity(url.len() + resolved.len());
    rewritten.push_str(&url[..offset]);
    rewritten.push_str(&resolved);
    rewritten.push_str(&url[offset + host.len()..]);
    rewritten
}

/// What `ssh -G host` answers as `hostname` (the last such line), with
/// `ssh.github.com` folded to `github.com`; `None` on any failure.
fn ssh_hostname(host: &str) -> Option<String> {
    let output = std::process::Command::new("ssh")
        .args(["-G", host])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let answer = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("hostname "))?
        .trim()
        .to_owned();
    if answer.is_empty() {
        return None;
    }
    Some(if answer.eq_ignore_ascii_case("ssh.github.com") {
        "github.com".to_owned()
    } else {
        answer
    })
}

/// The host of a remote URL, without its user or port; `None` for a non-URL.
pub fn remote_host(url: &str) -> Option<&str> {
    host_and_path(url).map(|(host, _)| host)
}

/// The owner segment of a remote's `<owner>/<repository>` path, in every
/// URL spelling [`same_remote`] reads (the user-less scp form included).
///
/// Unlike [`remote_slug`], an empty authority (`https:///owner/repo`) still
/// yields its owner: this feeds heuristics that should stay conservative when a
/// URL is odd, not identity, which needs a host.
pub fn url_owner(url: &str) -> Option<&str> {
    let (_, path) = authority_and_path(url)?;
    let (owner, repository) = path.trim_start_matches('/').split_once('/')?;
    (!owner.is_empty() && !repository.is_empty()).then_some(owner)
}

/// The `owner/repo` path of a forge remote with trailing `/` and `.git` removed;
/// `None` for a non-URL.
pub fn remote_slug(url: &str) -> Option<&str> {
    let (_, path) = host_and_path(url)?;
    let (owner, repository) = path.split_once('/')?;
    (!owner.is_empty() && !repository.is_empty() && !repository.contains('/')).then_some(path)
}

/// The last path segment of a remote URL without `.git`: the repository's own
/// name, whichever owner or forge holds it.
pub fn repository_name(url: &str) -> Option<&str> {
    let (_, repository) = url.trim_end_matches('/').rsplit_once('/')?;
    let name = without_git_suffix(repository);
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::{remote_host, remote_slug, repository_name, same_host, same_remote, url_owner};

    #[test]
    fn https_and_ssh_spellings_of_one_repository_are_the_same_remote() {
        assert!(same_remote(
            "https://forge.example/org/tool",
            "git@forge.example:org/tool.git"
        ));
        assert!(same_remote(
            "https://forge.example/org/tool.git/",
            "HTTPS://Forge.Example/Org/Tool"
        ));
        assert!(same_remote(
            "ssh://git@forge.example/org/tool",
            "https://forge.example/org/tool"
        ));
    }

    #[test]
    fn an_uppercase_git_suffix_is_stripped_like_a_lowercase_one() {
        assert!(same_remote(
            "https://forge.example/Org/Tool.GIT",
            "https://forge.example/org/tool"
        ));
    }

    #[test]
    fn a_port_on_the_host_does_not_make_another_repository() {
        assert!(same_remote(
            "ssh://git@forge.example:2222/org/tool",
            "https://forge.example/org/tool"
        ));
        assert!(same_remote(
            "https://forge.example:443/org/tool.git",
            "git@forge.example:org/tool"
        ));
    }

    #[test]
    fn scp_form_without_a_user_is_a_url_when_the_host_holds_no_slash() {
        assert!(same_remote(
            "forge.example:org/tool",
            "git@forge.example:org/tool.git"
        ));
        assert!(same_remote(
            "forge.example:org/tool",
            "https://forge.example/org/tool"
        ));
        // A colon in a later path component does not turn a directory into a host.
        assert!(!same_remote(
            "/tmp/lab/a:b/tool",
            "https://tmp/lab/a:b/tool"
        ));
        assert!(same_remote("/tmp/lab/a:b/tool", " /tmp/lab/a:b/tool/ "));
    }

    #[test]
    fn different_repositories_are_not_the_same_remote() {
        assert!(!same_remote(
            "https://forge.example/org/tool",
            "https://forge.example/org/tool-2"
        ));
        assert!(!same_remote(
            "https://forge.example/org/tool",
            "https://forge.example/other/tool"
        ));
        assert!(!same_remote(
            "https://forge.example/org/tool",
            "https://elsewhere.example/org/tool"
        ));
    }

    #[test]
    fn a_filesystem_path_compares_as_its_trimmed_text() {
        assert!(same_remote("/tmp/lab/upstream", " /tmp/lab/upstream/ "));
        assert!(!same_remote("/tmp/lab/upstream", "/tmp/lab/other"));
        // Two directories that differ by `.git` are two directories, spelled
        // as paths or as `file://` URLs.
        assert!(!same_remote("/tmp/lab/origin.git", "/tmp/lab/origin"));
        assert!(!same_remote("file:///tmp/x.git", "file:///tmp/x"));
        assert!(same_remote("file:///tmp/x.git", "file:///tmp/x.git/"));
    }

    #[test]
    fn a_remote_slug_is_the_owner_and_repository_of_a_forge_url() {
        assert_eq!(
            remote_slug("https://forge.example/Org/Tool.git/"),
            Some("Org/Tool")
        );
        assert_eq!(remote_slug("git@forge.example:org/tool"), Some("org/tool"));
        assert_eq!(remote_slug("/tmp/lab/upstream"), None);
        assert_eq!(url_owner("git@forge.example:org/tool.git"), Some("org"));
        // The owner is read from every spelling `same_remote` reads, the
        // user-less scp form included, and from a userinfo/port URL.
        assert_eq!(url_owner("forge.example:org/tool.git"), Some("org"));
        assert_eq!(
            url_owner("ssh://git@forge.example:22/org/tool"),
            Some("org")
        );
        assert_eq!(
            url_owner("https://u:p@forge.example:443/org/tool/"),
            Some("org")
        );
        assert_eq!(url_owner("https://forge.example//org/tool"), Some("org"));
        assert_eq!(url_owner("/tmp/lab/upstream"), None);
        assert_eq!(url_owner("forge.example:tool"), None);
        assert_eq!(
            repository_name("https://forge.invalid/someone/Tool.GIT/"),
            Some("Tool")
        );
        assert_eq!(repository_name("https://forge.invalid/someone/.git"), None);
    }

    #[test]
    fn a_subdomain_of_a_host_is_that_host() {
        // gh folds every `*.github.com` to `github.com`; the one comparison
        // rule folds any subdomain of a registered host, a superset.
        for host in [
            "www.forge.example",
            "WWW.Forge.Example",
            "api.forge.example",
            "foo.forge.example",
            "www.www.forge.example",
            "forge.example.",
        ] {
            assert!(
                same_remote(
                    "https://forge.example/org/tool",
                    &format!("https://{host}/org/tool")
                ),
                "{host}"
            );
            assert!(same_host(host, "forge.example"), "{host}");
        }
        // The suffix fold is directional: the stated host may be a subdomain
        // of the registered one, not the other way around…
        assert!(!same_host("forge.example", "api.forge.example"));
        assert!(!same_remote(
            "https://api.forge.example/org/tool",
            "https://forge.example/org/tool"
        ));
        // …but a leading `www.` is folded off both sides first: a registry
        // spelled on `www.` names the repository the canonical spelling does.
        assert!(same_host("forge.example", "www.forge.example"));
        assert!(same_host("forge.example", "WWW.Forge.Example"));
        assert!(same_remote(
            "https://www.forge.example/org/tool",
            "https://forge.example/org/tool"
        ));
        assert!(same_remote(
            "git@www.forge.example:org/tool.git",
            "forge.example:org/tool"
        ));
        assert!(!same_host("forge.example", "www.api.forge.example"));
        assert!(same_remote(
            "https://forge.example/org/tool",
            "www.forge.example:org/tool"
        ));
        // A host that merely ends in the other's text, or is another host
        // altogether, is not folded.
        for host in ["www", "notforge.example", "forge.example.evil", "example"] {
            assert!(
                !same_remote(
                    "https://forge.example/org/tool",
                    &format!("https://{host}/org/tool")
                ),
                "{host}"
            );
            assert!(!same_host(host, "forge.example"), "{host}");
        }
    }

    #[test]
    fn a_query_string_or_fragment_is_not_part_of_the_path() {
        assert!(same_remote(
            "https://forge.example/org/tool?tab=readme",
            "https://forge.example/org/tool"
        ));
        assert!(same_remote(
            "https://forge.example/org/tool#readme",
            "https://forge.example/org/tool"
        ));
        assert!(same_remote(
            "https://forge.example/org/tool.git?x=1",
            "https://forge.example/org/tool"
        ));
        assert!(!same_remote(
            "https://forge.example/org/tool?x=1",
            "https://forge.example/org/tool-2"
        ));
    }

    #[test]
    fn the_host_of_a_remote_drops_its_user_and_port() {
        assert_eq!(
            remote_host("git@forge.example:org/tool.git"),
            Some("forge.example")
        );
        assert_eq!(
            remote_host("ssh://git@forge.example:2222/org/tool"),
            Some("forge.example")
        );
        assert_eq!(remote_host("forge.example:org/tool"), Some("forge.example"));
        assert_eq!(remote_host("https:///ours/work.git"), None);
        assert_eq!(remote_host("/tmp/lab/upstream"), None);
    }
}
