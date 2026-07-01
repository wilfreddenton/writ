use anyhow::Result;
use std::path::PathBuf;

use clap::Parser;
use gpui::Global;

/// Environment variable name for GitHub token (shared between clap and tests)
pub const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

#[cfg(target_os = "windows")]
const DEFAULT_TEXT_FONT: &str = "Segoe UI";
#[cfg(target_os = "windows")]
const DEFAULT_CODE_FONT: &str = "Consolas";

#[cfg(target_os = "macos")]
const DEFAULT_TEXT_FONT: &str = ".AppleSystemUIFont";
#[cfg(target_os = "macos")]
const DEFAULT_CODE_FONT: &str = "Menlo";

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const DEFAULT_TEXT_FONT: &str = "Liberation Sans";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const DEFAULT_CODE_FONT: &str = "Liberation Mono";

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Config {
    #[arg(short, long, required_unless_present = "demo")]
    pub file: Option<PathBuf>,

    #[arg(long)]
    pub demo: bool,

    #[arg(long, env = "WRIT_TEXT_FONT", default_value = DEFAULT_TEXT_FONT)]
    pub text_font: String,

    #[arg(long, env = "WRIT_CODE_FONT", default_value = DEFAULT_CODE_FONT)]
    pub code_font: String,

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

impl Global for Config {}

impl Config {
    pub fn validate(self) -> Result<Self> {
        // File doesn't need to exist - we'll create it on save
        Ok(self)
    }
}
