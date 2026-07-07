//! GitHub reference validation model and cache.
//!
//! Pure data types (no network / async) shared between the render path and the
//! feature-gated GitHub client: validated issue/PR/user data plus the
//! thread-safe validation cache keyed by [`GitHubRef`](crate::inline::GitHubRef).

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::inline::GitHubRef;

/// Issue/PR data from GraphQL.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct IssueOrPr {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub number: u64,
    pub title: String,
    pub state: String,
    /// For issues: reason for closure (COMPLETED, NOT_PLANNED, REOPENED, or null)
    #[serde(rename = "stateReason")]
    pub state_reason: Option<String>,
    /// For PRs: whether it's a draft
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
}

/// Display status for an issue or PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStatus {
    /// Open issue or PR
    Open,
    /// Draft PR
    Draft,
    /// Merged PR
    Merged,
    /// Closed issue (completed) or closed PR (merged handled separately)
    Closed,
    /// Closed issue (not planned) or closed PR (not merged)
    ClosedNotPlanned,
}

impl IssueOrPr {
    /// Returns true if this is a pull request (vs an issue).
    pub fn is_pr(&self) -> bool {
        self.typename == "PullRequest"
    }

    /// Get the display status for coloring.
    pub fn status(&self) -> IssueStatus {
        if self.is_pr() {
            match self.state.as_str() {
                "OPEN" if self.is_draft => IssueStatus::Draft,
                "OPEN" => IssueStatus::Open,
                "MERGED" => IssueStatus::Merged,
                _ => IssueStatus::ClosedNotPlanned, // CLOSED PR (not merged)
            }
        } else {
            match self.state.as_str() {
                "OPEN" => IssueStatus::Open,
                "CLOSED" => {
                    match self.state_reason.as_deref() {
                        Some("NOT_PLANNED") => IssueStatus::ClosedNotPlanned,
                        _ => IssueStatus::Closed, // COMPLETED or other
                    }
                }
                _ => IssueStatus::Open,
            }
        }
    }

    /// Get the unicode symbol for this issue/PR type.
    pub fn symbol(&self) -> &'static str {
        if self.is_pr() {
            "⎇" // merge/branch symbol
        } else {
            "●" // filled circle
        }
    }
}

/// User data from GraphQL mentionableUsers.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MentionableUser {
    pub login: String,
    pub name: Option<String>,
}

/// Validation state for a GitHub reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationState {
    /// Fetch has been spawned but not yet completed.
    Pending,
    /// Reference exists on GitHub, optionally with detailed data for hover popup.
    Valid(Option<ValidatedRefData>),
    /// Reference does not exist on GitHub.
    Invalid,
}

/// Detailed data from a validated reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedRefData {
    /// Issue or PR with full details.
    Issue(IssueOrPr),
    /// User with full details.
    User(MentionableUser),
}

/// Cache for GitHub reference validation results.
#[derive(Debug, Clone)]
pub struct GitHubValidationCache {
    cache: Arc<Mutex<HashMap<GitHubRef, ValidationState>>>,
}

impl Default for GitHubValidationCache {
    fn default() -> Self {
        Self::new()
    }
}

impl GitHubValidationCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get(&self, ref_: &GitHubRef) -> Option<ValidationState> {
        self.cache.lock().unwrap().get(ref_).cloned()
    }

    /// Presence check that doesn't clone the value — for the "already pending/validated?"
    /// guard that spawns validation only for new refs.
    pub fn contains(&self, ref_: &GitHubRef) -> bool {
        self.cache.lock().unwrap().contains_key(ref_)
    }

    pub fn mark_pending(&self, ref_: GitHubRef) {
        self.cache
            .lock()
            .unwrap()
            .insert(ref_, ValidationState::Pending);
    }

    /// Set validation result as valid with optional detailed data.
    pub fn set_valid(&self, ref_: GitHubRef, data: Option<ValidatedRefData>) {
        self.cache
            .lock()
            .unwrap()
            .insert(ref_, ValidationState::Valid(data));
    }

    /// Set validation result as invalid.
    pub fn set_invalid(&self, ref_: GitHubRef) {
        self.cache
            .lock()
            .unwrap()
            .insert(ref_, ValidationState::Invalid);
    }

    pub fn is_valid(&self, ref_: &GitHubRef) -> bool {
        matches!(
            self.cache.lock().unwrap().get(ref_),
            Some(ValidationState::Valid(_))
        )
    }

    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_new_is_empty() {
        let cache = GitHubValidationCache::new();
        let ref_ = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };
        assert!(cache.get(&ref_).is_none());
        assert!(!cache.is_valid(&ref_));
    }

    #[test]
    fn test_cache_mark_pending() {
        let cache = GitHubValidationCache::new();
        let ref_ = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };

        cache.mark_pending(ref_.clone());
        assert_eq!(cache.get(&ref_), Some(ValidationState::Pending));
        assert!(!cache.is_valid(&ref_));
    }

    #[test]
    fn test_cache_set_result_valid() {
        let cache = GitHubValidationCache::new();
        let ref_ = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };

        let issue_data = ValidatedRefData::Issue(IssueOrPr {
            typename: "Issue".to_string(),
            number: 123,
            title: "Test issue".to_string(),
            state: "OPEN".to_string(),
            state_reason: None,
            is_draft: false,
        });
        cache.set_valid(ref_.clone(), Some(issue_data.clone()));
        assert_eq!(
            cache.get(&ref_),
            Some(ValidationState::Valid(Some(issue_data)))
        );
        assert!(cache.is_valid(&ref_));
    }

    #[test]
    fn test_cache_set_valid_no_data() {
        let cache = GitHubValidationCache::new();
        let ref_ = GitHubRef::Commit {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            sha: "abc1234".to_string(),
        };

        cache.set_valid(ref_.clone(), None);
        assert_eq!(cache.get(&ref_), Some(ValidationState::Valid(None)));
        assert!(cache.is_valid(&ref_));
    }

    #[test]
    fn test_cache_set_invalid() {
        let cache = GitHubValidationCache::new();
        let ref_ = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };

        cache.set_invalid(ref_.clone());
        assert_eq!(cache.get(&ref_), Some(ValidationState::Invalid));
        assert!(!cache.is_valid(&ref_));
    }

    #[test]
    fn test_cache_clear() {
        let cache = GitHubValidationCache::new();
        let ref_ = GitHubRef::Issue {
            owner: "rust-lang".to_string(),
            repo: "rust".to_string(),
            number: 123,
        };

        let issue_data = ValidatedRefData::Issue(IssueOrPr {
            typename: "Issue".to_string(),
            number: 123,
            title: "Test issue".to_string(),
            state: "OPEN".to_string(),
            state_reason: None,
            is_draft: false,
        });
        cache.set_valid(ref_.clone(), Some(issue_data));
        assert!(cache.is_valid(&ref_));

        cache.clear();
        assert!(cache.get(&ref_).is_none());
        assert!(!cache.is_valid(&ref_));
    }

    #[test]
    fn test_cache_clone_shares_state() {
        let cache1 = GitHubValidationCache::new();
        let cache2 = cache1.clone();

        let ref_ = GitHubRef::User {
            username: "torvalds".to_string(),
        };

        let user_data = ValidatedRefData::User(MentionableUser {
            login: "torvalds".to_string(),
            name: Some("Linus Torvalds".to_string()),
        });
        cache1.set_valid(ref_.clone(), Some(user_data));

        // Both should see the same state
        assert!(cache1.is_valid(&ref_));
        assert!(cache2.is_valid(&ref_));
    }
}
