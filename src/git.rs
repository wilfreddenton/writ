use std::path::Path;

use crate::inline::GitHubContext;

/// Detect GitHub repository context by discovering the git repo
/// and reading the origin remote URL.
pub fn detect_github_context(start_path: &Path) -> Option<GitHubContext> {
    // `gix::discover` walks up from a directory; when handed a file (the opened doc),
    // start from its parent. A relative filename's parent is "" — fall back to "." so
    // discovery still starts in the current directory.
    let parent = start_path.parent().filter(|p| !p.as_os_str().is_empty());
    let start_dir = if start_path.is_dir() {
        start_path
    } else {
        parent.unwrap_or_else(|| Path::new("."))
    };
    let repo = gix::discover(start_dir).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url(gix::remote::Direction::Fetch)?;
    parse_github_url(url.to_bstring().to_string().as_str())
}

/// Parse a GitHub URL (SSH `git@github.com:owner/repo.git` or HTTPS
/// `https://github.com/owner/repo(.git)`) into a `GitHubContext`.
fn parse_github_url(url: &str) -> Option<GitHubContext> {
    let repo_path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if url.contains("github.com") {
        url.strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?
            .strip_prefix("github.com/")?
    } else {
        return None;
    };
    let repo_path = repo_path.strip_suffix(".git").unwrap_or(repo_path);
    let (owner, repo) = repo_path.split_once('/')?;
    // Trim trailing path components (e.g. `.../owner/repo/pulls`) — applies to SSH too.
    let repo = repo.split('/').next()?;
    Some(GitHubContext {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Read the git HEAD version of the file at `path` as a UTF-8 string.
///
/// Used as the base for the inline diff view. Returns `None` when there's no
/// repository, no HEAD commit, the file isn't tracked in HEAD (new file), or
/// the blob isn't valid UTF-8 — in all of which cases there's no diff base.
pub fn head_blob_text(path: &Path) -> Option<String> {
    let abs = path.canonicalize().ok()?;
    let repo = gix::discover(abs.parent()?).ok()?;
    let workdir = repo.workdir()?.canonicalize().ok()?;
    let rel = abs.strip_prefix(&workdir).ok()?;

    let tree = repo.head_commit().ok()?.tree().ok()?;
    let entry = tree.lookup_entry_by_path(rel).ok()??;
    // `gix::Object` is `Drop`, so `data` can't be moved out; take it (leaving an empty
    // Vec behind) to avoid deep-copying the whole file.
    let mut blob = entry.object().ok()?;
    String::from_utf8(std::mem::take(&mut blob.data)).ok()
}

/// Parse a "owner/repo" string into GitHubContext.
pub fn parse_github_repo_string(s: &str) -> Option<GitHubContext> {
    let (owner, repo) = s.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GitHubContext {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_ssh_url() {
        let ctx = parse_github_url("git@github.com:wilfred/writ.git").unwrap();
        assert_eq!(ctx.owner, "wilfred");
        assert_eq!(ctx.repo, "writ");

        // Without .git suffix
        let ctx = parse_github_url("git@github.com:wilfred/writ").unwrap();
        assert_eq!(ctx.owner, "wilfred");
        assert_eq!(ctx.repo, "writ");

        // Trailing path components are trimmed on the SSH branch too (regression).
        let ctx = parse_github_url("git@github.com:wilfred/writ/extra").unwrap();
        assert_eq!(ctx.owner, "wilfred");
        assert_eq!(ctx.repo, "writ");
    }

    /// Detecting from a FILE path must match detecting from its parent directory —
    /// `gix::discover` walks up from a dir, so handing it the file used to return None
    /// (no context → no ref detection/coloring in the app). Uses this repo's checkout.
    #[test]
    fn detect_context_from_file_matches_dir() {
        let from_file = detect_github_context(Path::new("src/git.rs"));
        let from_dir = detect_github_context(Path::new("src"));
        assert_eq!(from_file, from_dir);
    }

    #[test]
    fn test_parse_github_https_url() {
        let ctx = parse_github_url("https://github.com/wilfred/writ.git").unwrap();
        assert_eq!(ctx.owner, "wilfred");
        assert_eq!(ctx.repo, "writ");

        // Without .git suffix
        let ctx = parse_github_url("https://github.com/wilfred/writ").unwrap();
        assert_eq!(ctx.owner, "wilfred");
        assert_eq!(ctx.repo, "writ");
    }

    #[test]
    fn test_parse_non_github_url() {
        assert!(parse_github_url("git@gitlab.com:owner/repo.git").is_none());
        assert!(parse_github_url("https://gitlab.com/owner/repo").is_none());
    }

    #[test]
    fn test_parse_github_repo_string() {
        let ctx = parse_github_repo_string("wilfred/writ").unwrap();
        assert_eq!(ctx.owner, "wilfred");
        assert_eq!(ctx.repo, "writ");

        assert!(parse_github_repo_string("invalid").is_none());
        assert!(parse_github_repo_string("/repo").is_none());
        assert!(parse_github_repo_string("owner/").is_none());
    }

    #[test]
    fn test_detect_github_context_in_repo() {
        // This test runs from within the writ repo itself
        let ctx = detect_github_context(std::path::Path::new(".")).unwrap();
        assert_eq!(ctx.owner, "wilfreddenton");
        assert_eq!(ctx.repo, "writ");
    }
}

#[cfg(test)]
mod head_blob_tests {
    use super::*;

    #[test]
    fn head_blob_reads_tracked_file() {
        // Cargo.toml is always committed and always contains the [package] table,
        // regardless of uncommitted working-tree edits.
        let text = head_blob_text(std::path::Path::new("Cargo.toml")).unwrap();
        assert!(text.contains("[package]"));
    }

    #[test]
    fn head_blob_none_for_missing() {
        assert!(head_blob_text(std::path::Path::new("does/not/exist.md")).is_none());
    }
}
