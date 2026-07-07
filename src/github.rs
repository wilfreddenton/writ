//! GitHub API client using GraphQL.
//!
//! Uses GitHub's GraphQL API for search/autocomplete and validation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use crate::inline::GitHubRef;
use crate::validation::{IssueOrPr, MentionableUser, ValidatedRefData};

const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// GraphQL field selection shared by every issue/PR query. The first line has no
/// leading indentation; callers supply it via a 24-space-indented placeholder.
const ISSUE_PR_FIELDS: &str = "__typename
                        ... on Issue { number title state stateReason }
                        ... on PullRequest { number title state merged isDraft }";

// ============================================================================
// GraphQL request/response types
// ============================================================================

#[derive(Serialize)]
struct GraphQLRequest<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQLError>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

// Issue/PR search response types
#[derive(Debug, Deserialize)]
struct IssueSearchData {
    search: SearchNodes,
    #[serde(default)]
    repository: Option<RepoIssueData>,
}

#[derive(Debug, Deserialize)]
struct SearchNodes {
    nodes: Vec<IssueOrPr>,
}

#[derive(Debug, Deserialize)]
struct RepoIssueData {
    #[serde(rename = "issueOrPullRequest")]
    issue_or_pull_request: Option<IssueOrPr>,
}

// Mentionable users response types
#[derive(Debug, Deserialize)]
struct MentionableData {
    repository: Option<RepoMentionableUsers>,
}

#[derive(Debug, Deserialize)]
struct RepoMentionableUsers {
    #[serde(rename = "mentionableUsers")]
    mentionable_users: UserNodes,
}

#[derive(Debug, Deserialize)]
struct UserNodes {
    nodes: Vec<MentionableUser>,
}

// Validation lookup response types
/// Response type for issue validation that returns full issue data.
#[derive(Debug, Deserialize)]
struct IssueValidationData {
    repository: Option<IssueValidationRepoData>,
}

#[derive(Debug, Deserialize)]
struct IssueValidationRepoData {
    #[serde(rename = "issueOrPullRequest")]
    issue_or_pull_request: Option<IssueOrPr>,
}

/// Response type for user validation that returns full user data.
#[derive(Debug, Deserialize)]
struct UserValidationData {
    user: Option<MentionableUser>,
}

/// Response type for commit validation.
#[derive(Debug, Deserialize)]
struct CommitValidationData {
    repository: Option<CommitValidationRepoData>,
}

#[derive(Debug, Deserialize)]
struct CommitValidationRepoData {
    object: Option<CommitValidationObject>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CommitValidationObject {
    oid: String,
}

// ============================================================================
// Validation result
// ============================================================================

/// Result of validating a GitHub reference.
pub enum ValidationResult {
    /// Reference exists and has detailed data for hover.
    ValidWithData(ValidatedRefData),
    /// Reference exists but has no detailed hover data (commits, etc.).
    ValidNoData,
    /// Reference does not exist.
    Invalid,
}

// ============================================================================
// Autocomplete caches
// ============================================================================

/// Thread-safe key/value cache shared across clones.
#[derive(Clone)]
pub struct Cache<K, V> {
    cache: Arc<Mutex<HashMap<K, V>>>,
}

impl<K: Eq + Hash, V: Clone> Default for Cache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash, V: Clone> Cache<K, V> {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.cache.lock().unwrap().get(key).cloned()
    }

    pub fn set(&self, key: K, value: V) {
        self.cache.lock().unwrap().insert(key, value);
    }

    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }
}

/// Cache for issue/PR autocomplete results.
pub type IssueCache = Cache<String, Vec<IssueOrPr>>;

/// Cache for user autocomplete results.
pub type UserCache = Cache<String, Vec<MentionableUser>>;

// ============================================================================
// GitHub client
// ============================================================================

/// GitHub API client using GraphQL.
#[derive(Clone)]
pub struct GitHubClient {
    token: String,
    client: reqwest::Client,
    issue_cache: IssueCache,
    user_cache: UserCache,
}

impl GitHubClient {
    pub fn new(token: String) -> Self {
        let client = reqwest::Client::new();
        Self {
            token,
            client,
            issue_cache: IssueCache::new(),
            user_cache: UserCache::new(),
        }
    }

    pub fn clear_autocomplete_cache(&self) {
        self.issue_cache.clear();
    }

    pub fn clear_user_cache(&self) {
        self.user_cache.clear();
    }

    /// Execute a GraphQL query.
    async fn graphql<T: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: Option<serde_json::Value>,
    ) -> Option<T> {
        let request = GraphQLRequest { query, variables };

        let response = self
            .client
            .post(GITHUB_GRAPHQL_URL)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("User-Agent", "writ")
            .json(&request)
            .send()
            .await
            .ok()?;

        let result: GraphQLResponse<T> = response.json().await.ok()?;

        if !result.errors.is_empty() {
            eprintln!(
                "[graphql] errors: {:?}",
                result.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
            );
            // Treat a partially-failed query as a failure rather than acting on
            // incomplete data (e.g. validating a ref against a truncated result).
            return None;
        }

        result.data
    }

    // ========================================================================
    // Issue/PR autocomplete
    // ========================================================================

    /// Fetch issues/PRs for autocomplete.
    ///
    /// - Empty prefix: returns most recently updated issues/PRs
    /// - Numeric prefix: returns exact match (if exists) + text search
    /// - Text prefix: returns text search results
    pub async fn issues_matching_prefix(
        &self,
        owner: &str,
        repo: &str,
        prefix: &str,
        limit: usize,
    ) -> Vec<IssueOrPr> {
        let cache_key = format!("{}/{}:{}", owner, repo, prefix);

        if let Some(cached) = self.issue_cache.get(&cache_key) {
            return cached.into_iter().take(limit).collect();
        }

        let results = if prefix.is_empty() {
            self.search_issues(owner, repo, None, limit).await
        } else if let Ok(number) = prefix.parse::<u64>() {
            // Numeric prefix: get exact match + search
            self.search_issues_with_exact(owner, repo, number, prefix, limit)
                .await
        } else {
            // Text prefix: just search
            self.search_issues(owner, repo, Some(prefix), limit).await
        };

        self.issue_cache.set(cache_key, results.clone());
        results
    }

    /// Search issues/PRs, optionally with a text query.
    /// The GitHub search qualifier string for issues+PRs in one repo, sorted by recency,
    /// optionally constrained to `text`.
    fn issue_search_query(owner: &str, repo: &str, text: Option<&str>) -> String {
        match text {
            Some(t) => format!("repo:{owner}/{repo} type:issue type:pr {t} sort:updated"),
            None => format!("repo:{owner}/{repo} type:issue type:pr sort:updated"),
        }
    }

    async fn search_issues(
        &self,
        owner: &str,
        repo: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Vec<IssueOrPr> {
        let search_query = Self::issue_search_query(owner, repo, query);

        let graphql_query = format!(
            r#"
            query($query: String!, $limit: Int!) {{
                search(query: $query, type: ISSUE, first: $limit) {{
                    nodes {{
                        {ISSUE_PR_FIELDS}
                    }}
                }}
            }}
        "#
        );

        let variables = serde_json::json!({
            "query": search_query,
            "limit": limit
        });

        let data: Option<IssueSearchData> = self.graphql(&graphql_query, Some(variables)).await;

        data.map(|d| d.search.nodes).unwrap_or_default()
    }

    /// Search issues/PRs with an exact number lookup in one query.
    async fn search_issues_with_exact(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        search_text: &str,
        limit: usize,
    ) -> Vec<IssueOrPr> {
        let search_query = Self::issue_search_query(owner, repo, Some(search_text));

        let graphql_query = format!(
            r#"
            query($owner: String!, $repo: String!, $number: Int!, $query: String!, $limit: Int!) {{
                repository(owner: $owner, name: $repo) {{
                    issueOrPullRequest(number: $number) {{
                        {ISSUE_PR_FIELDS}
                    }}
                }}
                search(query: $query, type: ISSUE, first: $limit) {{
                    nodes {{
                        {ISSUE_PR_FIELDS}
                    }}
                }}
            }}
        "#
        );

        let variables = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "number": number,
            "query": search_query,
            "limit": limit
        });

        let data: Option<IssueSearchData> = self.graphql(&graphql_query, Some(variables)).await;

        let Some(data) = data else {
            return vec![];
        };

        let mut results = Vec::new();

        // Add exact match first if it exists
        if let Some(repo_data) = data.repository
            && let Some(issue) = repo_data.issue_or_pull_request
        {
            results.push(issue);
        }

        // Add search results, deduplicating
        for issue in data.search.nodes {
            if !results.iter().any(|i| i.number == issue.number) {
                results.push(issue);
            }
            if results.len() >= limit {
                break;
            }
        }

        results.truncate(limit);
        results
    }

    // ========================================================================
    // User autocomplete (mentionableUsers)
    // ========================================================================

    /// Fetch mentionable users for autocomplete.
    /// Uses server-side search against both login and name.
    pub async fn users_matching_prefix(
        &self,
        owner: &str,
        repo: &str,
        prefix: &str,
        limit: usize,
    ) -> Vec<MentionableUser> {
        let cache_key = format!("{}/{}:{}", owner, repo, prefix);

        if let Some(cached) = self.user_cache.get(&cache_key) {
            return cached.into_iter().take(limit).collect();
        }

        let graphql_query = r#"
            query($owner: String!, $repo: String!, $query: String!, $limit: Int!) {
                repository(owner: $owner, name: $repo) {
                    mentionableUsers(query: $query, first: $limit) {
                        nodes {
                            login
                            name
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "query": prefix,
            "limit": limit
        });

        let data: Option<MentionableData> = self.graphql(graphql_query, Some(variables)).await;

        let users = data
            .and_then(|d| d.repository)
            .map(|r| r.mentionable_users.nodes)
            .unwrap_or_default();

        self.user_cache.set(cache_key, users.clone());
        users
    }

    // ========================================================================
    // Validation (for GitHubRef validation)
    // ========================================================================

    /// Validate a GitHub reference and return detailed data if available.
    pub async fn validate_ref(&self, ref_: &GitHubRef) -> ValidationResult {
        match ref_ {
            GitHubRef::Issue {
                owner,
                repo,
                number,
            } => match self.validate_issue(owner, repo, *number).await {
                Some(issue) => ValidationResult::ValidWithData(ValidatedRefData::Issue(issue)),
                None => ValidationResult::Invalid,
            },
            GitHubRef::User { username } => match self.validate_user(username).await {
                Some(user) => ValidationResult::ValidWithData(ValidatedRefData::User(user)),
                None => ValidationResult::Invalid,
            },
            GitHubRef::Commit { owner, repo, sha } => {
                if self.validate_commit(owner, repo, sha).await {
                    ValidationResult::ValidNoData
                } else {
                    ValidationResult::Invalid
                }
            }
            // Compare and File refs come from pasted URLs - assume valid, no hover data
            GitHubRef::Compare { .. } | GitHubRef::File { .. } => ValidationResult::ValidNoData,
        }
    }

    async fn validate_issue(&self, owner: &str, repo: &str, number: u64) -> Option<IssueOrPr> {
        let query = format!(
            r#"
            query($owner: String!, $repo: String!, $number: Int!) {{
                repository(owner: $owner, name: $repo) {{
                    issueOrPullRequest(number: $number) {{
                        {ISSUE_PR_FIELDS}
                    }}
                }}
            }}
        "#
        );

        let variables = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "number": number
        });

        let data: Option<IssueValidationData> = self.graphql(&query, Some(variables)).await;

        data.and_then(|d| d.repository)
            .and_then(|r| r.issue_or_pull_request)
    }

    async fn validate_user(&self, username: &str) -> Option<MentionableUser> {
        let query = r#"
            query($login: String!) {
                user(login: $login) {
                    login
                    name
                }
            }
        "#;

        let variables = serde_json::json!({
            "login": username
        });

        let data: Option<UserValidationData> = self.graphql(query, Some(variables)).await;

        data.and_then(|d| d.user)
    }

    async fn validate_commit(&self, owner: &str, repo: &str, sha: &str) -> bool {
        let query = r#"
            query($owner: String!, $repo: String!, $oid: GitObjectID!) {
                repository(owner: $owner, name: $repo) {
                    object(oid: $oid) {
                        oid
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "oid": sha
        });

        let data: Option<CommitValidationData> = self.graphql(query, Some(variables)).await;

        data.and_then(|d| d.repository)
            .and_then(|r| r.object)
            .is_some()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GITHUB_TOKEN_ENV;

    fn token_from_env() -> String {
        std::env::var(GITHUB_TOKEN_ENV).expect("GITHUB_TOKEN env var required for tests")
    }

    fn setup_crypto() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    #[tokio::test]
    async fn test_issues_matching_prefix_empty() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let recent = client
            .issues_matching_prefix("rust-lang", "rust", "", 5)
            .await;
        assert!(
            !recent.is_empty(),
            "Should return recent issues for empty prefix"
        );
        assert!(recent.len() <= 5, "Should respect limit");
    }

    #[tokio::test]
    async fn test_issues_matching_prefix_numeric() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let results = client
            .issues_matching_prefix("rust-lang", "rust", "1", 5)
            .await;
        assert!(
            !results.is_empty(),
            "Should return results for numeric prefix"
        );
        // First result should be issue #1 (exact match)
        assert_eq!(results[0].number, 1, "First result should be exact match");
    }

    #[tokio::test]
    async fn test_issues_matching_prefix_text() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let results = client
            .issues_matching_prefix("rust-lang", "rust", "ICE", 5)
            .await;
        assert!(!results.is_empty(), "Should return results for text prefix");
    }

    #[tokio::test]
    async fn test_users_matching_prefix_empty() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let users = client
            .users_matching_prefix("rust-lang", "rust", "", 5)
            .await;
        assert!(!users.is_empty(), "Should return mentionable users");
    }

    #[tokio::test]
    async fn test_users_matching_prefix_with_query() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let users = client
            .users_matching_prefix("rust-lang", "rust", "mat", 10)
            .await;
        assert!(!users.is_empty(), "Should return matching users");
        // Should match users with 'mat' in login or name
    }

    #[tokio::test]
    async fn test_users_have_names() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let users = client
            .users_matching_prefix("rust-lang", "rust", "", 20)
            .await;

        // At least some users should have display names
        let with_names = users.iter().filter(|u| u.name.is_some()).count();
        assert!(with_names > 0, "Some users should have display names");
    }

    #[tokio::test]
    async fn test_validate_issue_exists() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let issue = client.validate_issue("rust-lang", "rust", 1).await;
        assert!(issue.is_some(), "Issue #1 should exist in rust-lang/rust");
        assert_eq!(issue.unwrap().number, 1);
    }

    #[tokio::test]
    async fn test_validate_issue_not_found() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let issue = client.validate_issue("rust-lang", "rust", 999999999).await;
        assert!(issue.is_none(), "Non-existent issue should not be found");
    }

    #[tokio::test]
    async fn test_validate_user_exists() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let user = client.validate_user("torvalds").await;
        assert!(user.is_some(), "torvalds should exist");
        assert_eq!(user.unwrap().login, "torvalds");
    }

    #[tokio::test]
    async fn test_validate_user_not_found() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let user = client
            .validate_user("this-user-definitely-does-not-exist-12345")
            .await;
        assert!(user.is_none(), "Non-existent user should not be found");
    }

    #[tokio::test]
    async fn test_validate_commit_exists() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        // First commit in rust-lang/rust (full SHA required for GraphQL)
        let exists = client
            .validate_commit(
                "rust-lang",
                "rust",
                "c01efc669f09508b55eced32d3c88702578a7c3e",
            )
            .await;
        assert!(exists, "First commit should exist in rust-lang/rust");
    }

    #[tokio::test]
    async fn test_validate_commit_not_found() {
        setup_crypto();
        let client = GitHubClient::new(token_from_env());

        let exists = client
            .validate_commit(
                "rust-lang",
                "rust",
                "0000000000000000000000000000000000000000",
            )
            .await;
        assert!(!exists, "Invalid commit should not be found");
    }
}
