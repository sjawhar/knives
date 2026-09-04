//! What a remote URL names, whatever its spelling.
//!
//! Identity and trust both compare remote URLs as their `(host, path)`: `https`
//! and `ssh` forms, a user before the host, a port after it, a trailing `/` or
//! `.git`, and letter case are all spellings of one repository. A value that is
//! not a URL — a filesystem path, or a `file://` URL, whose authority is empty
//! — is only ever equal to itself.

/// Whether two remote spellings name one repository.
///
/// A value that parses as a remote URL with a host compares as its
/// [`host_and_path`], case-insensitively. A value that does not (a filesystem
/// path, or a `file://` URL, whose authority is empty) compares as its trimmed
/// text, so two directories that differ by `.git` stay two directories.
pub fn same_remote(a: &str, b: &str) -> bool {
    match (host_and_path(a), host_and_path(b)) {
        (Some((host_a, path_a)), Some((host_b, path_b))) => {
            host_a.eq_ignore_ascii_case(host_b) && path_a.eq_ignore_ascii_case(path_b)
        }
        (None, None) => a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/'),
        _ => false,
    }
}

/// `(host, path)` of a remote URL as spelled: the authority without its user
/// or port, and the path without surrounding `/` or a `.git` suffix. `None`
/// for a non-URL: a filesystem path, or a `file://` URL, whose authority is
/// empty. scp form without a user, `host:path`, is a URL too when the part
/// before the colon holds no `/`; a filesystem path with a colon in a later
/// component stays a path.
fn host_and_path(remote: &str) -> Option<(&str, &str)> {
    let trimmed = remote.trim().trim_end_matches('/');
    let (authority, path) = remote_authority_and_path(trimmed).or_else(|| {
        let (host, path) = trimmed.split_once(':')?;
        (!host.is_empty() && !host.contains('/')).then_some((host, path))
    })?;
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

/// The host of a remote URL, without its user or port; `None` for a non-URL.
pub fn remote_host(url: &str) -> Option<&str> {
    host_and_path(url).map(|(host, _)| host)
}

/// The owner segment of an authority-delimited `<owner>/<repository>` remote path.
///
/// Unlike [`remote_slug`], an empty authority (`https:///owner/repo`) still
/// yields its owner: this feeds heuristics that should stay conservative when a
/// URL is odd, not identity, which needs a host.
pub fn url_owner(url: &str) -> Option<&str> {
    let (_, path) = remote_authority_and_path(url)?;
    let (owner, repository) = path.split_once('/')?;
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
    use super::{remote_host, remote_slug, repository_name, same_remote, url_owner};

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
        assert_eq!(
            repository_name("https://forge.invalid/someone/Tool.GIT/"),
            Some("Tool")
        );
        assert_eq!(repository_name("https://forge.invalid/someone/.git"), None);
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
