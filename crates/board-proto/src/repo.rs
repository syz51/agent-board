//! Repo keys: the normalized form of a git origin URL that boards, agents, leases and
//! touches are matched by.

/// The longest repo key accepted.
pub const REPO_KEY_MAX: usize = 200;

/// Normalizes a git origin URL, or an already normalized key, to a repo key such as
/// `github.com/owner/repo`.
///
/// Accepts `scheme://[user[:password]@]host[:port]/path` (`https`, `http`, `ssh`, `git`,
/// `git+ssh`, …), scp-like `[user@]host:path` and the bare key `host/path`. Only a scheme
/// URL has a port: in scp form everything after the colon is the path, so
/// `git@host:12345/repo` keeps `12345` as the owner. The user, port, a `.git` suffix and
/// leading and trailing slashes are dropped, and the whole key is lowercased (ASCII), so
/// every checkout of a repo yields the same key whatever case its URL was written in.
///
/// A key is its own normalization. Returns `None` for local paths (`/srv/repo.git`,
/// `./repo`, `../repo`, `file://…`) and anything else
/// that is not a remote: an empty host or path, `.` or `..` segments, whitespace, control
/// characters or backslashes, or a key longer than [`REPO_KEY_MAX`].
pub fn normalize_repo(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (host, path) = if let Some((scheme, rest)) = raw.split_once("://") {
        let scheme_ok = !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !scheme_ok || scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        match host.split_once(':') {
            Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => (h, path.to_owned()),
            // `ssh://host:owner/repo`: scp-style path inside a scheme URL.
            Some((h, first)) => (h, format!("{first}/{path}")),
            None => (host, path.to_owned()),
        }
    } else if let Some((authority, path)) = raw
        .split_once(':')
        .filter(|(authority, _)| !authority.contains('/'))
    {
        // scp-like `[user@]host:path`; a colon after a slash belongs to a local path.
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host, path.to_owned())
    } else {
        // A bare key `host/path`, as this function returns it.
        let (host, path) = raw.split_once('/')?;
        (host, path.to_owned())
    };
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    let host_ok = !host.is_empty()
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    let path_ok = !path.is_empty()
        && path.split('/').all(|seg| !matches!(seg, "" | "." | ".."))
        && !path
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\');
    if !host_ok || !path_ok {
        return None;
    }
    let key = format!("{host}/{path}").to_ascii_lowercase();
    (key.len() <= REPO_KEY_MAX).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_normalize_to_host_owner_repo() {
        for url in [
            "git@github.com:syz51/agent-board.git",
            "git@github.com:syz51/agent-board",
            "ssh://git@github.com/syz51/agent-board.git",
            "ssh://git@github.com:22/syz51/agent-board.git",
            "ssh://git@GitHub.com:22/syz51/agent-board/",
            "ssh://git@github.com:syz51/agent-board.git",
            "git+ssh://git@github.com/syz51/agent-board.git",
            "https://github.com/syz51/agent-board.git",
            "https://github.com/syz51/agent-board/",
            "https://user:pw@github.com/Syz51/Agent-Board",
            "http://github.com:443/syz51/agent-board.git",
            "git://github.com/syz51/agent-board.git",
            "github.com/syz51/agent-board",
            "  git@github.com:syz51/agent-board.git\n",
        ] {
            assert_eq!(
                normalize_repo(url).as_deref(),
                Some("github.com/syz51/agent-board"),
                "{url}"
            );
        }
        assert_eq!(
            normalize_repo("http://user@example.com/a/b").as_deref(),
            Some("example.com/a/b")
        );
        assert_eq!(
            normalize_repo("git@gitlab.com:group/sub/repo.git").as_deref(),
            Some("gitlab.com/group/sub/repo")
        );
        assert_eq!(
            normalize_repo("git@gitea:o/r.git").as_deref(),
            Some("gitea/o/r")
        );
    }

    #[test]
    fn keys_are_lowercase_whatever_the_url_case() {
        let want = Some("github.com/myorg/repo");
        for url in [
            "github.com/MyOrg/Repo",
            "https://GitHub.com/MyOrg/Repo.git",
            "git@github.com:MyOrg/Repo.git",
            "github.com/myorg/repo",
        ] {
            assert_eq!(normalize_repo(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn scp_paths_never_hold_a_port() {
        assert_eq!(
            normalize_repo("git@github.com:12345/repo.git").as_deref(),
            Some("github.com/12345/repo")
        );
        assert_eq!(
            normalize_repo("github.com:12345/repo").as_deref(),
            Some("github.com/12345/repo")
        );
        // Only a scheme URL has a port.
        assert_eq!(
            normalize_repo("ssh://git@github.com:12345/repo.git").as_deref(),
            Some("github.com/repo")
        );
    }

    #[test]
    fn local_paths_and_garbage_have_no_key() {
        for bad in [
            "",
            "   ",
            "https://",
            "https://github.com",
            "https://github.com/",
            "a b/c",
            "github.com/a b",
            "/abs/path",
            "/srv/git/repo.git",
            "./repo",
            "../repo",
            "repo",
            "file:///srv/repo.git",
            "FILE:///srv/repo.git",
            "git@github.com:",
            ":owner/repo",
            "git@github.com:a/../b",
            "github.com/a//b",
            "C:\\src\\repo",
            "ht tp://github.com/a/b",
        ] {
            assert_eq!(normalize_repo(bad), None, "{bad:?}");
        }
        let long = format!("github.com/{}", "a".repeat(REPO_KEY_MAX));
        assert_eq!(normalize_repo(&long), None);
    }

    #[test]
    fn normalized_keys_are_fixed_points() {
        for url in [
            "git@github.com:12345/Repo.git",
            "https://GitHub.com/MyOrg/Repo",
            "git@gitea:o/r",
        ] {
            let key = normalize_repo(url).unwrap();
            assert_eq!(normalize_repo(&key).as_deref(), Some(key.as_str()), "{url}");
        }
    }
}
