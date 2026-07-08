//! Inline style extraction for markdown text.
//!
//! This module extracts styled regions (bold, italic, code, links, etc.)
//! from the inline parse trees, plus GitHub autolink references.

use regex::Regex;
use ropey::Rope;
use std::ops::Range;
use std::sync::LazyLock;
use tree_sitter::Node;

use crate::parser::MarkdownTree;
use crate::validation::GitHubValidationCache;

/// GitHub repository context for resolving relative references like #123.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubContext {
    pub owner: String,
    pub repo: String,
}

/// A detected GitHub reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitHubRef {
    /// Issue or PR: #123 or GH-123
    Issue {
        owner: String,
        repo: String,
        number: u64,
    },
    /// User mention: @username
    User { username: String },
    /// Commit SHA (7-40 hex chars)
    Commit {
        owner: String,
        repo: String,
        sha: String,
    },
    /// Compare URL: owner/repo/compare/base...head
    Compare {
        owner: String,
        repo: String,
        base: String,
        head: String,
    },
    /// File permalink: owner/repo/blob/sha/path#lines
    File {
        owner: String,
        repo: String,
        sha: String,
        path: String,
        lines: Option<String>,
    },
}

impl GitHubRef {
    /// Generate the GitHub URL for this reference.
    pub fn url(&self) -> String {
        match self {
            GitHubRef::Issue {
                owner,
                repo,
                number,
            } => {
                format!("https://github.com/{owner}/{repo}/issues/{number}")
            }
            GitHubRef::User { username } => {
                format!("https://github.com/{username}")
            }
            GitHubRef::Commit { owner, repo, sha } => {
                format!("https://github.com/{owner}/{repo}/commit/{sha}")
            }
            GitHubRef::Compare {
                owner,
                repo,
                base,
                head,
            } => {
                format!("https://github.com/{owner}/{repo}/compare/{base}...{head}")
            }
            GitHubRef::File {
                owner,
                repo,
                sha,
                path,
                lines,
            } => {
                let base = format!("https://github.com/{owner}/{repo}/blob/{sha}/{path}");
                match lines {
                    Some(l) => format!("{base}#{l}"),
                    None => base,
                }
            }
        }
    }

    /// Generate the short display text for this reference (used for URL shortening).
    /// If `context` is provided and the ref is from the same repo, omits the `owner/repo` prefix.
    pub fn short_display(&self, context: Option<&GitHubContext>) -> String {
        // Check if this ref is from the same repo as the context
        let is_same_repo = |owner: &str, repo: &str| {
            context.is_some_and(|ctx| ctx.owner == owner && ctx.repo == repo)
        };

        match self {
            GitHubRef::Issue {
                owner,
                repo,
                number,
            } => {
                if is_same_repo(owner, repo) {
                    format!("#{number}")
                } else {
                    format!("{owner}/{repo}#{number}")
                }
            }
            GitHubRef::User { username } => format!("@{username}"),
            GitHubRef::Commit { owner, repo, sha } => {
                let short_sha = &sha[..sha.len().min(7)];
                if is_same_repo(owner, repo) {
                    format!("@{short_sha}")
                } else {
                    format!("{owner}/{repo}@{short_sha}")
                }
            }
            GitHubRef::Compare {
                owner,
                repo,
                base,
                head,
            } => {
                if is_same_repo(owner, repo) {
                    format!("@{base}...{head}")
                } else {
                    format!("{owner}/{repo}@{base}...{head}")
                }
            }
            GitHubRef::File {
                owner,
                repo,
                sha,
                path,
                lines,
            } => {
                let short_sha = &sha[..sha.len().min(7)];
                let display = if is_same_repo(owner, repo) {
                    format!("@{short_sha}:{path}")
                } else {
                    format!("{owner}/{repo}@{short_sha}:{path}")
                };
                match lines {
                    Some(l) => format!("{display}#{l}"),
                    None => display,
                }
            }
        }
    }

    /// Create an Issue ref from a cross-repo capture (owner/repo#number).
    /// Capture groups: 1=full, 2=owner, 3=repo, 4=number
    fn from_cross_repo_issue_capture(cap: &regex::Captures) -> Self {
        GitHubRef::Issue {
            owner: cap[2].to_string(),
            repo: cap[3].to_string(),
            number: cap[4].parse().expect("regex guarantees valid number"),
        }
    }

    /// Create an Issue ref from a simple #number capture with context.
    /// Capture groups: 1=number
    fn from_issue_capture(cap: &regex::Captures, ctx: &GitHubContext) -> Self {
        GitHubRef::Issue {
            owner: ctx.owner.clone(),
            repo: ctx.repo.clone(),
            number: cap[1].parse().expect("regex guarantees valid number"),
        }
    }

    /// Create a Commit ref from a cross-repo capture (owner/repo@sha).
    /// Capture groups: 1=full, 2=owner, 3=repo, 4=sha
    fn from_cross_repo_commit_capture(cap: &regex::Captures) -> Self {
        GitHubRef::Commit {
            owner: cap[2].to_string(),
            repo: cap[3].to_string(),
            sha: cap[4].to_string(),
        }
    }

    /// Create a Commit ref from a simple SHA capture with context.
    /// Capture groups: 1=sha
    fn from_sha_capture(cap: &regex::Captures, ctx: &GitHubContext) -> Self {
        GitHubRef::Commit {
            owner: ctx.owner.clone(),
            repo: ctx.repo.clone(),
            sha: cap[1].to_string(),
        }
    }

    /// Create a User ref from a capture (@username).
    /// Capture groups: 1=full, 2=username
    fn from_user_capture(cap: &regex::Captures) -> Self {
        GitHubRef::User {
            username: cap[2].to_string(),
        }
    }

    /// Try to parse a GitHub URL into a GitHubRef.
    /// Returns None if the URL is not a recognized GitHub URL pattern.
    pub fn from_url(url: &str) -> Option<Self> {
        // Issue/PR URL: https://github.com/owner/repo/issues/123
        if let Some(cap) = GITHUB_ISSUE_URL_RE.captures(url) {
            return Some(GitHubRef::Issue {
                owner: cap[1].to_string(),
                repo: cap[2].to_string(),
                number: cap[3].parse().ok()?,
            });
        }

        // Compare URL: https://github.com/owner/repo/compare/base...head
        if let Some(cap) = GITHUB_COMPARE_URL_RE.captures(url) {
            return Some(GitHubRef::Compare {
                owner: cap[1].to_string(),
                repo: cap[2].to_string(),
                base: cap[3].to_string(),
                head: cap[4].to_string(),
            });
        }

        // File permalink: https://github.com/owner/repo/blob/sha/path#L10-L20
        if let Some(cap) = GITHUB_FILE_URL_RE.captures(url) {
            return Some(GitHubRef::File {
                owner: cap[1].to_string(),
                repo: cap[2].to_string(),
                sha: cap[3].to_string(),
                path: cap[4].to_string(),
                lines: cap.get(5).map(|m| m.as_str().to_string()),
            });
        }

        None
    }
}

// Regex patterns for GitHub reference detection.
// These are compiled once and reused.
// Note: Boundary checking is done manually in code since regex crate doesn't support lookbehind.
static ISSUE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#(\d{1,10})").unwrap());
static GH_ISSUE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)GH-(\d{1,10})").unwrap());
// Patterns with trailing boundary use an outer capture group for the full match without the boundary.
// E.g., (full_match)(?:boundary) so cap[1] is the text we want.
static USER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(@([a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?))(?:[^a-zA-Z0-9/]|$)").unwrap()
});
static CROSS_REPO_ISSUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(([a-zA-Z0-9-]+)/([a-zA-Z0-9._-]+)#(\d{1,10}))(?:[^a-zA-Z0-9]|$)").unwrap()
});
static SHA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([0-9a-f]{7,40})\b").unwrap());
static CROSS_REPO_COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(([a-zA-Z0-9-]+)/([a-zA-Z0-9._-]+)@([0-9a-f]{7,40}))(?:[^a-zA-Z0-9]|$)").unwrap()
});

// URL patterns for GitHub links
static GITHUB_ISSUE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://github\.com/([a-zA-Z0-9-]+)/([a-zA-Z0-9._-]+)/(?:issues|pull)/(\d+)")
        .unwrap()
});
static GITHUB_COMPARE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match base...head where base can contain dots but not the ... separator
    Regex::new(
        r"https?://github\.com/([a-zA-Z0-9-]+)/([a-zA-Z0-9._-]+)/compare/(.+?)\.\.\.([^\s]+)",
    )
    .unwrap()
});
static GITHUB_FILE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https?://github\.com/([a-zA-Z0-9-]+)/([a-zA-Z0-9._-]+)/blob/([0-9a-f]+)/([^#\s]+)(?:#(L\d+(?:-L\d+)?))?",
    )
    .unwrap()
});

// General URL pattern for naked URL detection
static NAKED_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match http:// or https:// URLs with a valid domain (must have at least one dot)
    // Domain: letters, digits, hyphens, with at least one dot for TLD
    // Path/query/fragment: any non-whitespace except certain punctuation
    Regex::new(r"https?://[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?)+(?::\d+)?(?:/[^\s<>\[\]()]*)?").unwrap()
});

/// A raw match from regex detection (before validation).
#[derive(Debug, Clone)]
pub struct RawGitHubMatch {
    /// The reference type and details.
    pub reference: GitHubRef,
    /// Byte range in the rope where this match was found.
    pub byte_range: Range<usize>,
}

/// An inline `$…$` math span detected in a line (buffer byte ranges).
#[derive(Debug, Clone)]
pub struct MathSpan {
    /// The whole `$…$` including delimiters.
    pub full_range: Range<usize>,
    /// The LaTeX between the delimiters.
    pub content_range: Range<usize>,
}

/// Detect inline `$…$` math in one line. `$` is ordinary text in CommonMark, so this is a
/// hand-rolled scan (no tree-sitter): an unescaped `$` opens (but `$$` is a display-math
/// delimiter, skipped here — those are collected once per build), the next unescaped `$`
/// on the same line closes. Following the GitHub/pandoc rule, the opener must not be
/// followed by whitespace and the closer not preceded by whitespace (so prose like
/// "$5 and $10" doesn't match). `$` inside `code_ranges` (inline code) or `block_ranges`
/// (`$$…$$` display math) is skipped. Buffer offsets via `line_start`.
#[cfg(feature = "math")]
pub fn detect_inline_math(
    line: &str,
    line_start: usize,
    code_ranges: &[Range<usize>],
    block_ranges: &[Range<usize>],
) -> Vec<MathSpan> {
    let mut spans = Vec::new();
    if !line.contains('$') {
        return spans;
    }
    let b = line.as_bytes();
    let excluded = |abs: usize| {
        code_ranges
            .iter()
            .chain(block_ranges.iter())
            .any(|r| r.contains(&abs))
    };
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' || (i > 0 && b[i - 1] == b'\\') {
            i += 1;
            continue;
        }
        // `$$` is a display-math delimiter, not an inline opener.
        if i + 1 < b.len() && b[i + 1] == b'$' {
            i += 2;
            continue;
        }
        // Opener must be followed by a non-whitespace char, and be outside code/block math.
        if i + 1 >= b.len() || b[i + 1].is_ascii_whitespace() || excluded(line_start + i) {
            i += 1;
            continue;
        }
        // Closing `$`: first unescaped `$` on the line whose preceding char isn't whitespace.
        let mut j = i + 1;
        while j < b.len() && !(b[j] == b'$' && b[j - 1] != b'\\') {
            j += 1;
        }
        if j < b.len() && !b[j - 1].is_ascii_whitespace() && !excluded(line_start + j) {
            spans.push(MathSpan {
                full_range: (line_start + i)..(line_start + j + 1),
                content_range: (line_start + i + 1)..(line_start + j),
            });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    spans
}

/// A naked URL detected in text (not inside []() markdown link syntax).
#[derive(Debug, Clone)]
pub struct NakedUrl {
    /// The full URL text.
    pub url: String,
    /// Byte range in the rope where this URL was found.
    pub byte_range: Range<usize>,
    /// If this is a GitHub URL, the parsed reference.
    pub github_ref: Option<GitHubRef>,
}

/// Whether `pos` falls inside any of `ranges`. Shared by the inline detectors so that
/// "skip matches inside code spans / markdown links" is one predicate, not re-inlined.
fn in_any(ranges: &[Range<usize>], pos: usize) -> bool {
    ranges.iter().any(|r| r.contains(&pos))
}

/// Detect naked URLs in a single line of text.
///
/// Returns URLs that are not inside markdown link syntax or code spans.
/// For GitHub URLs, also parses the reference for potential shortening.
///
/// - `line`: the line text to scan
/// - `line_byte_offset`: byte offset of this line in the buffer (for absolute ranges)
/// - `code_ranges`: absolute byte ranges of code spans to skip
/// - `link_ranges`: absolute byte ranges of markdown links to skip
pub fn detect_naked_urls(
    line: &str,
    line_byte_offset: usize,
    code_ranges: &[Range<usize>],
    link_ranges: &[Range<usize>],
) -> Vec<NakedUrl> {
    let mut urls = Vec::new();

    for m in NAKED_URL_RE.find_iter(line) {
        let abs_range = (line_byte_offset + m.start())..(line_byte_offset + m.end());

        // Skip if inside code span or markdown link
        if in_any(code_ranges, abs_range.start) || in_any(link_ranges, abs_range.start) {
            continue;
        }

        let url = m.as_str().to_string();
        let github_ref = GitHubRef::from_url(&url);

        urls.push(NakedUrl {
            url,
            byte_range: abs_range,
            github_ref,
        });
    }

    urls
}

/// Cheap byte pre-scan for whether a line could contain any GitHub reference. True when
/// it has a hash, at-sign, or slash, a case-insensitive GH-dash, or a 7+ ascii-hex run.
fn line_might_contain_ref(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut hex_run = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' || b == b'@' || b == b'/' {
            return true;
        }
        // `GH-` (case-insensitive), the GH-123 issue form.
        if b == b'-' && i >= 2 && (bytes[i - 1] | 0x20) == b'h' && (bytes[i - 2] | 0x20) == b'g' {
            return true;
        }
        if b.is_ascii_hexdigit() {
            hex_run += 1;
            if hex_run >= 7 {
                return true;
            }
        } else {
            hex_run = 0;
        }
    }
    false
}

/// Detect GitHub references in a single line of text.
///
/// Returns raw matches that should be validated against the GitHub API
/// before being styled as links.
///
/// - `line`: the line text to scan
/// - `line_byte_offset`: byte offset of this line in the buffer (for absolute ranges)
/// - `github_context`: owner/repo for resolving relative refs like #123
/// - `code_ranges`: absolute byte ranges of code spans to skip
pub fn detect_github_references_in_line(
    line: &str,
    line_byte_offset: usize,
    github_context: Option<&GitHubContext>,
    code_ranges: &[Range<usize>],
    link_ranges: &[Range<usize>],
) -> Vec<RawGitHubMatch> {
    // Cheap pre-filter: a line with no `#`/`@`/`/` and no 7+ run of ascii-hex can't
    // contain any reference, so skip all the regex passes (the common prose case).
    if !line_might_contain_ref(line) {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let mut matched_ranges: Vec<Range<usize>> = Vec::new();

    // A reference inside a code span or a markdown link (`[fixes #123](url)`) is not a
    // standalone ref — skip it so the link stays clickable and the ref isn't double-styled.
    let is_skipped =
        |abs_pos: usize| -> bool { in_any(code_ranges, abs_pos) || in_any(link_ranges, abs_pos) };

    // Helper to check if a range overlaps with already matched ranges
    let overlaps_matched = |range: &Range<usize>, matched: &[Range<usize>]| -> bool {
        matched
            .iter()
            .any(|r| range.start < r.end && range.end > r.start)
    };

    // Helper to check if char at position is a word boundary (not alphanumeric)
    let is_word_boundary = |pos: usize| -> bool {
        if pos >= line.len() {
            return true;
        }
        !line.as_bytes()[pos].is_ascii_alphanumeric()
    };

    // Cross-repo refs (owner/repo#123, owner/repo@sha): cap[1] is the full match
    // without the trailing boundary. Checked before the simple #123 / @user passes.
    let mut push_cross_repo = |re: &Regex, make: fn(&regex::Captures) -> GitHubRef| {
        for cap in re.captures_iter(line) {
            let full = cap.get(1).unwrap();
            let abs_range = (line_byte_offset + full.start())..(line_byte_offset + full.end());
            if is_skipped(abs_range.start) {
                continue;
            }
            matched_ranges.push(abs_range.clone());
            matches.push(RawGitHubMatch {
                reference: make(&cap),
                byte_range: abs_range,
            });
        }
    };
    push_cross_repo(
        &CROSS_REPO_ISSUE_RE,
        GitHubRef::from_cross_repo_issue_capture,
    );
    push_cross_repo(
        &CROSS_REPO_COMMIT_RE,
        GitHubRef::from_cross_repo_commit_capture,
    );

    // User mentions: @username
    for cap in USER_RE.captures_iter(line) {
        let full = cap.get(1).unwrap(); // `@username`; `full.start()` is the `@`
        let abs_range = (line_byte_offset + full.start())..(line_byte_offset + full.end());
        if is_skipped(abs_range.start) {
            continue;
        }
        // Left word-boundary: without this, an email like `foo@example.com` matches
        // `@example` and underlines "example" (and fires a needless validation request).
        if full.start() > 0 && !is_word_boundary(full.start() - 1) {
            continue;
        }
        if overlaps_matched(&abs_range, &matched_ranges) {
            continue;
        }
        matched_ranges.push(abs_range.clone());
        matches.push(RawGitHubMatch {
            reference: GitHubRef::from_user_capture(&cap),
            byte_range: abs_range,
        });
    }

    // Simple issues: #123 and GH-123 (only if we have GitHub context)
    if let Some(ctx) = github_context {
        let mut push_issue_matches = |re: &Regex| {
            for cap in re.captures_iter(line) {
                let full_match = cap.get(0).unwrap();
                let match_start = full_match.start();
                let match_end = full_match.end();
                let abs_start = line_byte_offset + match_start;
                if is_skipped(abs_start) {
                    continue;
                }
                // Check word boundaries
                if match_start > 0 && !is_word_boundary(match_start - 1) {
                    continue;
                }
                if match_end < line.len() && !is_word_boundary(match_end) {
                    continue;
                }
                let abs_range = abs_start..(line_byte_offset + match_end);
                if overlaps_matched(&abs_range, &matched_ranges) {
                    continue;
                }
                matched_ranges.push(abs_range.clone());
                matches.push(RawGitHubMatch {
                    reference: GitHubRef::from_issue_capture(&cap, ctx),
                    byte_range: abs_range,
                });
            }
        };

        for re in [&*ISSUE_RE, &*GH_ISSUE_RE] {
            push_issue_matches(re);
        }

        // Simple SHA
        for cap in SHA_RE.captures_iter(line) {
            let m = cap.get(1).unwrap();
            let start = m.start();
            let abs_start = line_byte_offset + start;
            if is_skipped(abs_start) {
                continue;
            }
            let abs_range = abs_start..(line_byte_offset + m.end());
            if overlaps_matched(&abs_range, &matched_ranges) {
                continue;
            }
            // Record the span (like the issue pass) so any future pass added after this one
            // won't double-match a SHA; harmless today as SHAs are the last pass.
            matched_ranges.push(abs_range.clone());
            matches.push(RawGitHubMatch {
                reference: GitHubRef::from_sha_capture(&cap, ctx),
                byte_range: abs_range,
            });
        }
    }

    matches
}

/// Convert validated GitHub references into styled regions.
///
/// Only references that exist in `validated_refs` will be styled as links.
pub fn github_refs_to_styled_regions(
    matches: &[RawGitHubMatch],
    cache: &GitHubValidationCache,
) -> Vec<StyledRegion> {
    matches
        .iter()
        .filter(|m| cache.is_valid(&m.reference))
        .map(|m| StyledRegion {
            full_range: m.byte_range.clone(),
            content_range: m.byte_range.clone(),
            link_url: Some(m.reference.url()),
            ..Default::default()
        })
        .collect()
}

/// Convert naked URLs into styled regions (clickable links).
/// For GitHub URLs with validated refs, sets display_text for shortening.
/// If `context` is provided, refs from the same repo omit the `owner/repo` prefix.
pub fn naked_urls_to_styled_regions(
    urls: &[NakedUrl],
    cache: &GitHubValidationCache,
    context: Option<&GitHubContext>,
) -> Vec<StyledRegion> {
    urls.iter()
        .map(|u| {
            // Check if this is a validated GitHub URL that should be shortened
            let display_text = u.github_ref.as_ref().and_then(|ref_| {
                if cache.is_valid(ref_) {
                    Some(ref_.short_display(context))
                } else {
                    None
                }
            });

            StyledRegion {
                full_range: u.byte_range.clone(),
                content_range: u.byte_range.clone(),
                link_url: Some(u.url.clone()),
                display_text,
                ..Default::default()
            }
        })
        .collect()
}

/// Style attributes for inline text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strikethrough: bool,
    pub heading_level: u8,
}

impl TextStyle {
    pub fn bold() -> Self {
        Self {
            bold: true,
            ..Default::default()
        }
    }

    pub fn italic() -> Self {
        Self {
            italic: true,
            ..Default::default()
        }
    }

    pub fn code() -> Self {
        Self {
            code: true,
            ..Default::default()
        }
    }

    pub fn strikethrough() -> Self {
        Self {
            strikethrough: true,
            ..Default::default()
        }
    }

    pub fn heading(level: u8) -> Self {
        Self {
            heading_level: level,
            bold: true,
            ..Default::default()
        }
    }

    pub fn merge(&self, other: &TextStyle) -> Self {
        Self {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            code: self.code || other.code,
            strikethrough: self.strikethrough || other.strikethrough,
            heading_level: self.heading_level.max(other.heading_level),
        }
    }
}

/// A styled region of inline text with its delimiters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StyledRegion {
    /// The full range including delimiters (e.g., `**bold**` → 0..8)
    pub full_range: Range<usize>,
    /// The content range excluding delimiters (e.g., `**bold**` → 2..6)
    pub content_range: Range<usize>,
    pub style: TextStyle,
    pub link_url: Option<String>,
    pub is_image: bool,
    /// If Some, this is a checkbox. The bool indicates checked state.
    pub checkbox: Option<bool>,
    /// If Some, display this text instead of the buffer content.
    /// Used for GitHub URL shortening (e.g., show "owner/repo#123" instead of full URL).
    /// When set, the region is "atomic" - cursor/selection treat it as a single unit.
    pub display_text: Option<String>,
}

/// Extract all inline styles from a markdown tree.
/// Returns a flat Vec sorted by start byte position.
pub fn extract_all_inline_styles(tree: &MarkdownTree, rope: &Rope) -> Vec<StyledRegion> {
    let mut styles = Vec::new();
    // The parser already stores every inline subtree; iterate them directly instead of
    // re-walking the whole block tree to rediscover the nodes they hang off.
    for inline_tree in tree.inline_trees() {
        collect_from_inline_tree(inline_tree.root_node(), rope, &mut styles);
    }
    styles.sort_by_key(|s| s.full_range.start);
    styles
}

/// Collect styled regions from an inline tree.
fn collect_from_inline_tree(node: Node, rope: &Rope, styles: &mut Vec<StyledRegion>) {
    collect_from_inline_tree_inner(node, rope, styles, false);
}

/// Inner function that tracks whether we're inside a strikethrough.
fn collect_from_inline_tree_inner(
    node: Node,
    rope: &Rope,
    styles: &mut Vec<StyledRegion>,
    in_strikethrough: bool,
) {
    let mut child_in_strikethrough = in_strikethrough;

    match node.kind() {
        "emphasis" => {
            if let Some(region) = extract_emphasis_region(&node, TextStyle::italic()) {
                styles.push(region);
            }
        }
        "strong_emphasis" => {
            if let Some(region) = extract_emphasis_region(&node, TextStyle::bold()) {
                styles.push(region);
            }
        }
        "code_span" => {
            if let Some(region) = extract_code_span_region(&node) {
                styles.push(region);
            }
        }
        "strikethrough" => {
            // Skip nested strikethroughs - tree-sitter parses ~~text~~ as nested ~(~text~)~
            if !in_strikethrough {
                if let Some(region) = extract_emphasis_region(&node, TextStyle::strikethrough()) {
                    styles.push(region);
                }
                child_in_strikethrough = true;
            }
        }
        "inline_link" | "full_reference_link" | "collapsed_reference_link" | "shortcut_link" => {
            if let Some(region) = extract_link_region(&node, rope) {
                styles.push(region);
            }
        }
        "image" => {
            if let Some(region) = extract_image_region(&node, rope) {
                styles.push(region);
            }
        }
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_from_inline_tree_inner(child, rope, styles, child_in_strikethrough);
    }
}

fn extract_emphasis_region(node: &Node, style: TextStyle) -> Option<StyledRegion> {
    let full_start = node.start_byte();
    let full_end = node.end_byte();

    let mut content_start = full_start;
    let mut content_end = full_end;

    // Find all delimiter boundaries recursively
    // This handles ~~text~~ which tree-sitter parses as nested ~(~text~)~
    fn collect_delimiters(node: &Node, delimiters: &mut Vec<(usize, usize)>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let kind = child.kind();
            if kind.ends_with("_delimiter") {
                delimiters.push((child.start_byte(), child.end_byte()));
            }
            // Recurse into nested emphasis/strikethrough of the same type
            if kind == node.kind() {
                collect_delimiters(&child, delimiters);
            }
        }
    }

    let mut delimiters: Vec<(usize, usize)> = Vec::new();
    collect_delimiters(node, &mut delimiters);

    // Opening delimiters from start - keep consuming adjacent delimiters
    delimiters.sort_by_key(|(start, _)| *start);
    for &(start, end) in &delimiters {
        if start == content_start {
            content_start = end;
        }
    }

    // Closing delimiters from end - keep consuming adjacent delimiters
    for &(start, end) in delimiters.iter().rev() {
        if end == content_end {
            content_end = start;
        }
    }

    Some(StyledRegion {
        full_range: full_start..full_end,
        content_range: content_start..content_end,
        style,
        ..Default::default()
    })
}

fn extract_code_span_region(node: &Node) -> Option<StyledRegion> {
    let full_start = node.start_byte();
    let full_end = node.end_byte();

    let mut content_start = full_start;
    let mut content_end = full_end;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "code_span_delimiter" {
            if child.start_byte() == full_start {
                content_start = child.end_byte();
            } else if child.end_byte() == full_end {
                content_end = child.start_byte();
            }
        }
    }

    Some(StyledRegion {
        full_range: full_start..full_end,
        content_range: content_start..content_end,
        style: TextStyle::code(),
        ..Default::default()
    })
}

fn extract_link_region(node: &Node, rope: &Rope) -> Option<StyledRegion> {
    let full_start = node.start_byte();
    let full_end = node.end_byte();

    // Skip task list checkbox patterns like [ ] or [x] or [X]
    // These get misdetected as shortcut_links when tree-sitter doesn't
    // recognize the task list (e.g., when there's no content after the checkbox)
    if node.kind() == "shortcut_link" {
        let start = rope.byte_to_char(full_start);
        let end = rope.byte_to_char(full_end);
        let text = rope.slice(start..end).to_string();
        if text == "[ ]" || text == "[x]" || text == "[X]" {
            return None;
        }
    }

    let mut content_start = full_start;
    let mut content_end = full_end;
    let mut url: Option<String> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "link_text" => {
                content_start = child.start_byte();
                content_end = child.end_byte();
            }
            "link_destination" => {
                let start = rope.byte_to_char(child.start_byte());
                let end = rope.byte_to_char(child.end_byte());
                url = Some(rope.slice(start..end).to_string());
            }
            _ => {}
        }
    }

    // Fallback for reference-style links without explicit link_text
    if url.is_none() {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "[" {
                content_start = child.end_byte();
            } else if child.kind() == "]" {
                content_end = child.start_byte();
            }
        }
    }

    Some(StyledRegion {
        full_range: full_start..full_end,
        content_range: content_start..content_end,
        link_url: url,
        ..Default::default()
    })
}

fn extract_image_region(node: &Node, rope: &Rope) -> Option<StyledRegion> {
    let full_start = node.start_byte();
    let full_end = node.end_byte();

    let mut alt_start = full_start;
    let mut alt_end = full_end;
    let mut url: Option<String> = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "image_description" => {
                alt_start = child.start_byte();
                alt_end = child.end_byte();
            }
            "link_destination" => {
                let start = rope.byte_to_char(child.start_byte());
                let end = rope.byte_to_char(child.end_byte());
                url = Some(rope.slice(start..end).to_string());
            }
            _ => {}
        }
    }

    let url = url?;

    Some(StyledRegion {
        full_range: full_start..full_end,
        content_range: alt_start..alt_end,
        link_url: Some(url),
        is_image: true,
        ..Default::default()
    })
}

/// Get inline styles that overlap with a byte range.
///
/// `styles` must be sorted by `full_range.start` (ascending).
pub fn styles_in_range<'a>(
    styles: &'a [StyledRegion],
    range: &Range<usize>,
) -> Vec<&'a StyledRegion> {
    // Regions are sorted by start, so anything starting at/after `range.end`
    // (and everything after it) cannot overlap.
    let end_idx = styles.partition_point(|s| s.full_range.start < range.end);

    // Among the remaining candidates, a region overlaps iff it ends past
    // `range.start`. We must check all of them, not just the immediate
    // predecessor: an enclosing region (e.g. bold spanning a nested link) can
    // start several indices earlier while a closer nested region ends before
    // `range.start`. Callers only query visible lines, so `end_idx` is bounded.
    styles[..end_idx]
        .iter()
        .filter(|s| s.full_range.end > range.start)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn get_styles(text: &str) -> Vec<StyledRegion> {
        let buf: Buffer = text.parse().unwrap();
        extract_all_inline_styles(buf.tree().unwrap(), buf.rope())
    }

    fn region(full: Range<usize>) -> StyledRegion {
        StyledRegion {
            content_range: full.clone(),
            full_range: full,
            ..Default::default()
        }
    }

    #[test]
    fn test_styles_in_range_includes_enclosing_region() {
        // Enclosing region A spans a nested region B; a third region C sits
        // between B and the query point. Querying inside A but after B and C
        // must still return A (the earlier "walk back one" approach dropped it).
        let styles = vec![region(0..50), region(5..10), region(12..14)];
        let hit = styles_in_range(&styles, &(15..16));
        let ranges: Vec<_> = hit.iter().map(|s| s.full_range.clone()).collect();
        assert_eq!(ranges, vec![0..50]);
    }

    #[test]
    fn test_styles_in_range_nested_pair() {
        let styles = vec![region(0..50), region(10..20)];
        let hit = styles_in_range(&styles, &(15..16));
        let ranges: Vec<_> = hit.iter().map(|s| s.full_range.clone()).collect();
        assert_eq!(ranges, vec![0..50, 10..20]);
    }

    #[test]
    fn test_styles_in_range_excludes_non_overlapping() {
        let styles = vec![region(0..5), region(10..20)];
        assert!(styles_in_range(&styles, &(6..9)).is_empty());
    }

    #[test]
    fn test_bold() {
        let styles = get_styles("**bold** text\n");
        assert_eq!(styles.len(), 1);
        assert!(styles[0].style.bold);
        assert_eq!(styles[0].full_range, 0..8);
        assert_eq!(styles[0].content_range, 2..6);
    }

    #[test]
    fn test_italic() {
        let styles = get_styles("*italic* text\n");
        assert_eq!(styles.len(), 1);
        assert!(styles[0].style.italic);
        assert_eq!(styles[0].full_range, 0..8);
        assert_eq!(styles[0].content_range, 1..7);
    }

    #[test]
    fn test_code() {
        let styles = get_styles("`code` text\n");
        assert_eq!(styles.len(), 1);
        assert!(styles[0].style.code);
        assert_eq!(styles[0].full_range, 0..6);
        assert_eq!(styles[0].content_range, 1..5);
    }

    #[test]
    fn test_link() {
        let styles = get_styles("[text](http://example.com)\n");
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].link_url, Some("http://example.com".to_string()));
        assert_eq!(styles[0].full_range, 0..26);
        // content_range should be the link text "text"
        assert_eq!(styles[0].content_range, 1..5);
    }

    #[test]
    fn test_nested_bold_italic() {
        let styles = get_styles("***bold italic***\n");
        // Should have both bold and italic regions
        assert!(!styles.is_empty());
    }

    #[test]
    fn test_multiple_lines() {
        let styles = get_styles("**bold**\n*italic*\n`code`\n");
        assert_eq!(styles.len(), 3);
        // Should be sorted by position
        assert!(styles[0].style.bold);
        assert!(styles[1].style.italic);
        assert!(styles[2].style.code);
    }

    #[test]
    fn test_styles_in_range() {
        let styles = get_styles("**bold**\n*italic*\n`code`\n");

        // Line 1: bytes 0-8
        let line1_styles = styles_in_range(&styles, &(0..8));
        assert_eq!(line1_styles.len(), 1);
        assert!(line1_styles[0].style.bold);

        // Line 2: bytes 9-17
        let line2_styles = styles_in_range(&styles, &(9..17));
        assert_eq!(line2_styles.len(), 1);
        assert!(line2_styles[0].style.italic);
    }

    #[test]
    fn test_blockquote_inline() {
        let styles = get_styles("> **bold** in quote\n");
        assert_eq!(styles.len(), 1);
        assert!(styles[0].style.bold);
    }

    #[test]
    fn test_list_inline() {
        let styles = get_styles("- **bold** in list\n- *italic* too\n");
        assert_eq!(styles.len(), 2);
        assert!(styles[0].style.bold);
        assert!(styles[1].style.italic);
    }

    #[test]
    fn test_strikethrough() {
        let styles = get_styles("~~hey~~\n");
        // Tree-sitter parses ~~hey~~ as nested strikethrough ~(~hey~)~
        // We skip the inner one and collect all delimiters recursively
        assert_eq!(styles.len(), 1);
        assert!(styles[0].style.strikethrough);
        // full_range is the entire ~~hey~~ (0..7)
        // content_range excludes all delimiters (2..5 for just "hey")
        assert_eq!(styles[0].full_range, 0..7);
        assert_eq!(styles[0].content_range, 2..5);
    }

    // GitHub reference detection tests

    fn github_ctx() -> GitHubContext {
        GitHubContext {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
        }
    }

    #[test]
    fn test_github_issue_ref() {
        let line = "See #123 for details";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::Issue { owner, repo, number }
            if owner == "rust-lang" && repo == "rust" && *number == 123
        ));
        assert_eq!(matches[0].byte_range, 4..8); // "#123"
    }

    #[test]
    fn test_github_issue_at_start() {
        let line = "#456 is fixed";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::Issue { number, .. } if *number == 456
        ));
        assert_eq!(matches[0].byte_range, 0..4);
    }

    #[test]
    fn test_github_gh_format() {
        let line = "Fixed in GH-789";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::Issue { number, .. } if *number == 789
        ));
    }

    #[test]
    fn test_github_user_mention() {
        let line = "Thanks @torvalds for the review";
        let matches = detect_github_references_in_line(line, 0, None, &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::User { username } if username == "torvalds"
        ));
        assert_eq!(matches[0].byte_range, 7..16); // "@torvalds"
    }

    #[test]
    fn test_github_cross_repo_issue() {
        let line = "See tokio-rs/tokio#1234";
        let matches = detect_github_references_in_line(line, 0, None, &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::Issue { owner, repo, number }
            if owner == "tokio-rs" && repo == "tokio" && *number == 1234
        ));
    }

    #[test]
    fn test_github_sha_ref() {
        let line = "Fixed in a1b2c3d";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::Commit { sha, .. } if sha == "a1b2c3d"
        ));
    }

    #[test]
    fn test_github_cross_repo_commit() {
        let line = "See tokio-rs/tokio@abc1234";
        let matches = detect_github_references_in_line(line, 0, None, &[], &[]);

        assert_eq!(matches.len(), 1);
        assert!(matches!(
            &matches[0].reference,
            GitHubRef::Commit { owner, repo, sha }
            if owner == "tokio-rs" && repo == "tokio" && sha == "abc1234"
        ));
    }

    #[test]
    fn test_github_skip_code_span() {
        let line = "Use `#123` in code";
        let ctx = github_ctx();
        // Simulate code span at bytes 4..10 ("`#123`")
        let code_range = 4..10;
        let matches = detect_github_references_in_line(
            line,
            0,
            Some(&ctx),
            std::slice::from_ref(&code_range),
            &[],
        );

        assert!(matches.is_empty(), "Should not match inside code span");
    }

    #[test]
    fn test_github_ref_inside_link_is_skipped() {
        let ctx = GitHubContext {
            owner: "o".into(),
            repo: "r".into(),
        };
        // `#123` and `@user` inside a markdown link belong to the link, not a ref.
        let line = "see [fixes #123 by @bob](http://x)";
        let link = 4..24; // the `[...](...)` span
        let matches =
            detect_github_references_in_line(line, 0, Some(&ctx), &[], std::slice::from_ref(&link));
        assert!(matches.is_empty(), "refs inside a link must be skipped");
    }

    #[test]
    fn test_email_domain_is_not_a_mention() {
        // `foo@example.com` must not match `@example` (no left word boundary).
        let matches = detect_github_references_in_line("mail foo@example.com", 0, None, &[], &[]);
        assert!(
            matches.is_empty(),
            "email domain should not become an @mention: {matches:?}"
        );
        // A real mention after a boundary still matches.
        let m2 = detect_github_references_in_line("hi @bob there", 0, None, &[], &[]);
        assert_eq!(m2.len(), 1);
    }

    #[test]
    fn test_github_no_context_no_simple_refs() {
        let line = "Issue #123 and commit a1b2c3d";
        // Without context, simple #123 and bare SHA should not be detected
        let matches = detect_github_references_in_line(line, 0, None, &[], &[]);

        assert!(matches.is_empty(), "Simple refs need GitHub context");
    }

    #[test]
    fn test_github_multiple_refs() {
        let line = "#1 #2 @user rust-lang/rust#3";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        // Should find: #1, #2, @user, rust-lang/rust#3
        assert_eq!(matches.len(), 4);
    }

    #[test]
    fn test_github_line_byte_offset() {
        // Simulate a line that starts at byte 100 in the buffer
        let line = "See #123";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 100, Some(&ctx), &[], &[]);

        assert_eq!(matches.len(), 1);
        // Byte range should be absolute (100 + 4 = 104, 100 + 8 = 108)
        assert_eq!(matches[0].byte_range, 104..108);
    }

    #[test]
    fn test_github_ref_url() {
        let issue = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };
        assert_eq!(issue.url(), "https://github.com/rust-lang/rust/issues/123");

        let user = GitHubRef::User {
            username: "torvalds".to_string(),
        };
        assert_eq!(user.url(), "https://github.com/torvalds");

        let commit = GitHubRef::Commit {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            sha: "abc1234".to_string(),
        };
        assert_eq!(
            commit.url(),
            "https://github.com/rust-lang/rust/commit/abc1234"
        );
    }

    #[test]
    fn test_github_refs_to_styled_regions() {
        let line = "See #123";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        // Simulate validation - mark the issue as valid (no hover data needed for this test)
        let cache = GitHubValidationCache::new();
        cache.set_valid(
            GitHubRef::Issue {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                number: 123,
            },
            None,
        );

        let regions = github_refs_to_styled_regions(&matches, &cache);
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0].link_url,
            Some("https://github.com/rust-lang/rust/issues/123".to_string())
        );
    }

    #[test]
    fn test_github_unvalidated_ref_not_styled() {
        let line = "See #999999";
        let ctx = github_ctx();
        let matches = detect_github_references_in_line(line, 0, Some(&ctx), &[], &[]);

        // Empty cache - nothing validated
        let cache = GitHubValidationCache::new();
        let regions = github_refs_to_styled_regions(&matches, &cache);

        assert!(regions.is_empty(), "Unvalidated refs should not be styled");
    }

    // Naked URL detection tests

    #[test]
    fn test_naked_url_detection() {
        let line = "See https://example.com/page for details";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com/page");
        assert_eq!(urls[0].byte_range, 4..28);
        assert!(urls[0].github_ref.is_none());
    }

    #[test]
    fn test_naked_url_skips_code_span() {
        let line = "Use `https://example.com` in code";
        // Simulate code span at bytes 4..25
        let code_range = 4..25;
        let urls = detect_naked_urls(line, 0, &[code_range], &[]);

        assert!(urls.is_empty(), "Should not match inside code span");
    }

    #[test]
    fn test_naked_url_skips_markdown_link() {
        let line = "See [link](https://example.com) here";
        // Simulate link at bytes 4..31
        let link_range = 4..31;
        let urls = detect_naked_urls(line, 0, &[], &[link_range]);

        assert!(urls.is_empty(), "Should not match inside markdown link");
    }

    #[test]
    fn test_naked_github_issue_url() {
        let line = "See https://github.com/rust-lang/rust/issues/123 for details";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert!(matches!(
            &urls[0].github_ref,
            Some(GitHubRef::Issue { owner, repo, number })
            if owner == "rust-lang" && repo == "rust" && *number == 123
        ));
        assert_eq!(urls[0].byte_range, 4..48);
    }

    #[test]
    fn test_naked_github_pr_url() {
        let line = "Fixed in https://github.com/tokio-rs/tokio/pull/456";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert!(matches!(
            &urls[0].github_ref,
            Some(GitHubRef::Issue { owner, repo, number })
            if owner == "tokio-rs" && repo == "tokio" && *number == 456
        ));
    }

    #[test]
    fn test_naked_github_compare_url() {
        let line = "Changes: https://github.com/rust-lang/rust/compare/v1.0...v2.0";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert!(matches!(
            &urls[0].github_ref,
            Some(GitHubRef::Compare { owner, repo, base, head })
            if owner == "rust-lang" && repo == "rust" && base == "v1.0" && head == "v2.0"
        ));
    }

    #[test]
    fn test_naked_github_file_url() {
        let line = "See https://github.com/rust-lang/rust/blob/abc1234def/src/main.rs#L10-L20";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert!(matches!(
            &urls[0].github_ref,
            Some(GitHubRef::File { owner, repo, sha, path, lines })
            if owner == "rust-lang" && repo == "rust" && sha == "abc1234def"
               && path == "src/main.rs" && lines.as_deref() == Some("L10-L20")
        ));
    }

    #[test]
    fn test_naked_github_file_url_no_lines() {
        let line = "File: https://github.com/owner/repo/blob/abc1234/path/to/file.rs";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert!(matches!(
            &urls[0].github_ref,
            Some(GitHubRef::File { path, lines, .. })
            if path == "path/to/file.rs" && lines.is_none()
        ));
    }

    #[test]
    fn test_non_github_url_has_no_ref() {
        let line = "See https://example.com/page";
        let urls = detect_naked_urls(line, 0, &[], &[]);

        assert_eq!(urls.len(), 1);
        assert!(urls[0].github_ref.is_none());
    }

    #[test]
    fn test_invalid_urls_not_matched() {
        // URL without proper domain (no TLD)
        let line = "http://g is not a valid URL";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert!(urls.is_empty(), "http://g should not match");

        // Just protocol with single char
        let line = "https://x should not match";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert!(urls.is_empty(), "https://x should not match");

        // Domain without TLD
        let line = "http://localhost is common but no TLD";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert!(
            urls.is_empty(),
            "http://localhost should not match (no TLD)"
        );
    }

    #[test]
    fn test_valid_urls_matched() {
        // Standard domain
        let line = "Visit https://example.com for info";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://example.com");

        // Domain with path
        let line = "See http://foo.bar/path/to/page";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "http://foo.bar/path/to/page");

        // Domain with port
        let line = "Dev server at http://example.com:8080/api";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "http://example.com:8080/api");

        // Subdomain
        let line = "Check https://sub.domain.example.org/page";
        let urls = detect_naked_urls(line, 0, &[], &[]);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "https://sub.domain.example.org/page");
    }

    #[test]
    fn test_github_ref_from_url() {
        // Issue URL
        let issue = GitHubRef::from_url("https://github.com/rust-lang/rust/issues/123");
        assert!(matches!(
            issue,
            Some(GitHubRef::Issue { owner, repo, number })
            if owner == "rust-lang" && repo == "rust" && number == 123
        ));

        // PR URL
        let pr = GitHubRef::from_url("https://github.com/tokio-rs/tokio/pull/456");
        assert!(matches!(
            pr,
            Some(GitHubRef::Issue { number, .. })
            if number == 456
        ));

        // Compare URL
        let compare = GitHubRef::from_url("https://github.com/owner/repo/compare/v1.0...v2.0");
        assert!(matches!(
            compare,
            Some(GitHubRef::Compare { base, head, .. })
            if base == "v1.0" && head == "v2.0"
        ));

        // File URL
        let file = GitHubRef::from_url("https://github.com/owner/repo/blob/abc123/src/lib.rs#L5");
        assert!(matches!(
            file,
            Some(GitHubRef::File { sha, path, lines, .. })
            if sha == "abc123" && path == "src/lib.rs" && lines.as_deref() == Some("L5")
        ));

        // Non-GitHub URL
        let other = GitHubRef::from_url("https://example.com/page");
        assert!(other.is_none());
    }

    // short_display tests

    #[test]
    fn test_short_display_issue() {
        let issue = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };
        // Without context, shows full owner/repo
        assert_eq!(issue.short_display(None), "rust-lang/rust#123");

        // With matching context, omits owner/repo
        let ctx = GitHubContext {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
        };
        assert_eq!(issue.short_display(Some(&ctx)), "#123");

        // With different context, shows full owner/repo
        let other_ctx = GitHubContext {
            owner: "other".to_string(),
            repo: "repo".to_string(),
        };
        assert_eq!(issue.short_display(Some(&other_ctx)), "rust-lang/rust#123");
    }

    #[test]
    fn test_short_display_user() {
        let user = GitHubRef::User {
            username: "torvalds".to_string(),
        };
        assert_eq!(user.short_display(None), "@torvalds");
    }

    #[test]
    fn test_short_display_commit() {
        let commit = GitHubRef::Commit {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            sha: "abc1234567890".to_string(),
        };
        // SHA should be truncated to 7 chars
        assert_eq!(commit.short_display(None), "rust-lang/rust@abc1234");

        // With matching context
        let ctx = GitHubContext {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
        };
        assert_eq!(commit.short_display(Some(&ctx)), "@abc1234");
    }

    #[test]
    fn test_short_display_compare() {
        let compare = GitHubRef::Compare {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            base: "v1.0".to_string(),
            head: "v2.0".to_string(),
        };
        assert_eq!(compare.short_display(None), "rust-lang/rust@v1.0...v2.0");

        // With matching context
        let ctx = GitHubContext {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
        };
        assert_eq!(compare.short_display(Some(&ctx)), "@v1.0...v2.0");
    }

    #[test]
    fn test_short_display_file() {
        let file = GitHubRef::File {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            sha: "abc1234567890".to_string(),
            path: "src/main.rs".to_string(),
            lines: Some("L10-L20".to_string()),
        };
        assert_eq!(
            file.short_display(None),
            "rust-lang/rust@abc1234:src/main.rs#L10-L20"
        );

        // With matching context
        let ctx = GitHubContext {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
        };
        assert_eq!(
            file.short_display(Some(&ctx)),
            "@abc1234:src/main.rs#L10-L20"
        );
    }

    #[test]
    fn test_short_display_file_no_lines() {
        let file = GitHubRef::File {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            sha: "abc1234".to_string(),
            path: "README.md".to_string(),
            lines: None,
        };
        assert_eq!(file.short_display(None), "rust-lang/rust@abc1234:README.md");
    }

    #[test]
    fn test_url_and_short_display_for_new_variants() {
        let compare = GitHubRef::Compare {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            base: "main".to_string(),
            head: "feature".to_string(),
        };
        assert_eq!(
            compare.url(),
            "https://github.com/owner/repo/compare/main...feature"
        );

        let file = GitHubRef::File {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            sha: "abc1234".to_string(),
            path: "src/lib.rs".to_string(),
            lines: Some("L5".to_string()),
        };
        assert_eq!(
            file.url(),
            "https://github.com/owner/repo/blob/abc1234/src/lib.rs#L5"
        );
    }

    #[test]
    fn test_naked_urls_to_styled_regions() {
        let urls = vec![
            NakedUrl {
                url: "https://example.com/page".to_string(),
                byte_range: 4..27,
                github_ref: None,
            },
            NakedUrl {
                url: "https://github.com/rust-lang/rust/issues/123".to_string(),
                byte_range: 30..74,
                github_ref: Some(GitHubRef::Issue {
                    owner: "rust-lang".to_string(),
                    repo: "rust".to_string(),
                    number: 123,
                }),
            },
        ];

        // Empty cache - no validation yet
        let cache = GitHubValidationCache::new();
        let regions = naked_urls_to_styled_regions(&urls, &cache, None);

        assert_eq!(regions.len(), 2);

        // First URL - plain link, no display_text
        assert_eq!(regions[0].full_range, 4..27);
        assert_eq!(
            regions[0].link_url,
            Some("https://example.com/page".to_string())
        );
        assert!(regions[0].display_text.is_none());

        // Second URL - GitHub URL, not yet validated so no display_text
        assert_eq!(regions[1].full_range, 30..74);
        assert_eq!(
            regions[1].link_url,
            Some("https://github.com/rust-lang/rust/issues/123".to_string())
        );
        assert!(regions[1].display_text.is_none());
    }

    #[test]
    fn test_naked_urls_with_validated_github_ref() {
        let github_ref = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };

        let urls = vec![NakedUrl {
            url: "https://github.com/rust-lang/rust/issues/123".to_string(),
            byte_range: 0..44,
            github_ref: Some(github_ref.clone()),
        }];

        // Mark the ref as validated (no hover data needed for this test)
        let cache = GitHubValidationCache::new();
        cache.set_valid(github_ref, None);

        // Without context - shows full owner/repo
        let regions = naked_urls_to_styled_regions(&urls, &cache, None);
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0].display_text,
            Some("rust-lang/rust#123".to_string())
        );

        // With matching context - omits owner/repo
        let ctx = GitHubContext {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
        };
        let regions = naked_urls_to_styled_regions(&urls, &cache, Some(&ctx));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].display_text, Some("#123".to_string()));
    }

    #[cfg(feature = "math")]
    #[test]
    fn inline_math_basic_and_content() {
        let line = "before $x^2 + 1$ after";
        let spans = detect_inline_math(line, 0, &[], &[]);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].full_range, 7..16);
        assert_eq!(&line[spans[0].content_range.clone()], "x^2 + 1");
    }

    #[cfg(feature = "math")]
    #[test]
    fn inline_math_rejects_currency() {
        // "$5 ... $10": opener followed by a digit is fine, but the closer is preceded by
        // whitespace ("$10" — the space before the second `$`), so no span matches.
        assert!(detect_inline_math("this $5 and $10 more", 0, &[], &[]).is_empty());
    }

    #[cfg(feature = "math")]
    #[test]
    fn inline_math_two_spans_and_offsets() {
        let line = "$a$ and $b+c$";
        let spans = detect_inline_math(line, 100, &[], &[]);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].full_range, 100..103);
        assert_eq!(spans[1].full_range, 108..113);
    }

    #[cfg(feature = "math")]
    #[test]
    fn inline_math_skips_display_and_escaped_and_code() {
        // `$$` display delimiter is not an inline opener.
        assert!(detect_inline_math("$$x$$", 0, &[], &[]).is_empty());
        // Escaped `\$` is literal.
        assert!(detect_inline_math("cost is \\$5 today", 0, &[], &[]).is_empty());
        // A `$` inside an inline-code range is skipped.
        let line = "`$x$` and $y$";
        let spans = detect_inline_math(line, 0, std::slice::from_ref(&(0..5)), &[]);
        assert_eq!(spans.len(), 1);
        assert_eq!(&line[spans[0].content_range.clone()], "y");
    }
}
