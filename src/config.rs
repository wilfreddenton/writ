use std::path::PathBuf;

use clap::Parser;

/// Environment variable name for GitHub token (shared between clap and tests)
pub const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Config {
    #[arg(short, long, required_unless_present = "demo")]
    pub file: Option<PathBuf>,

    #[arg(long)]
    pub demo: bool,

    /// Save file automatically on every edit (useful for GhostText integration)
    #[arg(long)]
    pub autosave: bool,

    /// GitHub personal access token for API access (issue/PR references)
    #[arg(long, env = GITHUB_TOKEN_ENV)]
    pub github_token: Option<String>,

    /// GitHub repository (owner/repo) for autolink detection.
    /// If not specified, will try to detect from .git/config.
    #[arg(long, env = "WRIT_GITHUB_REPO")]
    pub github_repo: Option<String>,
}
