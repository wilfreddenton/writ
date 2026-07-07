#[cfg(feature = "app")]
use std::collections::HashMap;
#[cfg(feature = "app")]
use std::path::{Path, PathBuf};

#[cfg(feature = "app")]
use clap::Parser;
#[cfg(feature = "app")]
use serde::Deserialize;
#[cfg(feature = "app")]
use vello::peniko::Color;

#[cfg(feature = "app")]
use crate::consts::FONT_SIZE;
#[cfg(feature = "app")]
use crate::editor::EditorTheme;

/// Environment variable name for GitHub token (shared between clap and tests)
pub const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

#[cfg(feature = "app")]
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

    /// Path to a TOML config file (theme + font). Defaults to the platform config dir,
    /// e.g. `~/.config/writ/config.toml`.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

/// The runtime config applied at app startup: the editor theme and the document font.
/// Built by [`load`] from `config.toml`, falling back to the built-in defaults.
#[cfg(feature = "app")]
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub theme: EditorTheme,
    /// The document font family; `None` = system monospace.
    pub font_family: Option<String>,
    pub font_size: f32,
}

#[cfg(feature = "app")]
impl Default for ResolvedConfig {
    fn default() -> Self {
        Self {
            theme: EditorTheme::dracula(),
            font_family: None,
            font_size: FONT_SIZE,
        }
    }
}

/// Top-level `config.toml` shape. Unknown keys are rejected so typos surface as a clear
/// parse error (which downgrades to defaults) instead of being silently ignored.
#[cfg(feature = "app")]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    font: Option<FontSpec>,
    theme: Option<ThemeSpec>,
    /// User-defined named palettes, selectable via `theme.name`.
    #[serde(default)]
    themes: HashMap<String, Palette>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FontSpec {
    family: Option<String>,
    size: Option<f32>,
}

#[cfg(feature = "app")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeSpec {
    name: Option<String>,
    overrides: Option<Palette>,
}

/// A partial color palette: any omitted field inherits from the theme it overlays.
/// Colors are `#RRGGBB` or `#RRGGBBAA` hex strings.
#[cfg(feature = "app")]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Palette {
    background: Option<String>,
    surface: Option<String>,
    foreground: Option<String>,
    selection: Option<String>,
    comment: Option<String>,
    red: Option<String>,
    orange: Option<String>,
    yellow: Option<String>,
    green: Option<String>,
    cyan: Option<String>,
    purple: Option<String>,
    pink: Option<String>,
}

#[cfg(feature = "app")]
impl Palette {
    /// Overlay this partial palette onto `theme`, warning about (and skipping) bad hex.
    fn apply(&self, theme: &mut EditorTheme) {
        let set = |slot: &mut Color, field: &Option<String>, name: &str| {
            if let Some(hex) = field {
                match parse_hex(hex) {
                    Ok(c) => *slot = c,
                    Err(e) => eprintln!("[writ] config: {name}: {e}"),
                }
            }
        };
        set(&mut theme.background, &self.background, "background");
        set(&mut theme.surface, &self.surface, "surface");
        set(&mut theme.foreground, &self.foreground, "foreground");
        set(&mut theme.selection, &self.selection, "selection");
        set(&mut theme.comment, &self.comment, "comment");
        set(&mut theme.red, &self.red, "red");
        set(&mut theme.orange, &self.orange, "orange");
        set(&mut theme.yellow, &self.yellow, "yellow");
        set(&mut theme.green, &self.green, "green");
        set(&mut theme.cyan, &self.cyan, "cyan");
        set(&mut theme.purple, &self.purple, "purple");
        set(&mut theme.pink, &self.pink, "pink");
    }
}

/// Parse a `#RRGGBB` / `#RRGGBBAA` hex color (leading `#` optional).
#[cfg(feature = "app")]
fn parse_hex(s: &str) -> Result<Color, String> {
    let t = s.trim();
    let h = t.strip_prefix('#').unwrap_or(t);
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16);
    match h.len() {
        6 => match (byte(0), byte(2), byte(4)) {
            (Ok(r), Ok(g), Ok(b)) => Ok(Color::from_rgba8(r, g, b, 0xFF)),
            _ => Err(format!("invalid hex color '{s}'")),
        },
        8 => match (byte(0), byte(2), byte(4), byte(6)) {
            (Ok(r), Ok(g), Ok(b), Ok(a)) => Ok(Color::from_rgba8(r, g, b, a)),
            _ => Err(format!("invalid hex color '{s}'")),
        },
        _ => Err(format!("expected #RRGGBB or #RRGGBBAA, got '{s}'")),
    }
}

/// The default config path: `<platform config dir>/writ/config.toml`.
#[cfg(feature = "app")]
fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("writ").join("config.toml"))
}

/// Load and resolve the config. `explicit` is the `--config` path (else the default
/// location is used). A missing file → defaults silently (unless named explicitly); a
/// read or parse error → warn to stderr + defaults, so a bad config never blocks startup.
#[cfg(feature = "app")]
pub fn load(explicit: Option<&Path>) -> ResolvedConfig {
    let Some(path) = explicit.map(Path::to_path_buf).or_else(default_config_path) else {
        return ResolvedConfig::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if explicit.is_some() {
                eprintln!("[writ] config: {} not found", path.display());
            }
            return ResolvedConfig::default();
        }
        Err(e) => {
            eprintln!("[writ] config: cannot read {}: {e}", path.display());
            return ResolvedConfig::default();
        }
    };
    match toml::from_str::<FileConfig>(&text) {
        Ok(file) => resolve(file),
        Err(e) => {
            eprintln!("[writ] config: {} parse error: {e}", path.display());
            ResolvedConfig::default()
        }
    }
}

/// Resolve a parsed file into runtime settings: pick the base theme (a user-defined
/// `[themes.<name>]` shadows a built-in of the same name and inherits dracula for omitted
/// colors), apply `[theme.overrides]`, then the font.
#[cfg(feature = "app")]
fn resolve(file: FileConfig) -> ResolvedConfig {
    let mut cfg = ResolvedConfig::default();
    if let Some(theme) = &file.theme {
        if let Some(name) = &theme.name {
            if let Some(custom) = file.themes.get(name) {
                let mut base = EditorTheme::dracula();
                custom.apply(&mut base);
                cfg.theme = base;
            } else if let Some(builtin) = EditorTheme::by_name(name) {
                cfg.theme = builtin;
            } else {
                eprintln!("[writ] config: unknown theme '{name}', using dracula");
            }
        }
        if let Some(overrides) = &theme.overrides {
            overrides.apply(&mut cfg.theme);
        }
    }
    if let Some(font) = &file.font {
        if let Some(family) = &font.family {
            let f = family.trim();
            if !f.is_empty() {
                cfg.font_family = Some(f.to_string());
            }
        }
        if let Some(size) = font.size {
            if size.is_finite() && (6.0..=96.0).contains(&size) {
                cfg.font_size = size;
            } else {
                eprintln!(
                    "[writ] config: font.size {size} out of range (6–96), using {}",
                    cfg.font_size
                );
            }
        }
    }
    cfg
}

#[cfg(all(test, feature = "app"))]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(
            parse_hex("#FF0000").unwrap(),
            Color::from_rgba8(255, 0, 0, 255)
        );
        assert_eq!(
            parse_hex("00FF00").unwrap(),
            Color::from_rgba8(0, 255, 0, 255)
        );
        assert_eq!(
            parse_hex("#0000FF80").unwrap(),
            Color::from_rgba8(0, 0, 255, 0x80)
        );
        assert!(parse_hex("#ZZ0000").is_err());
        assert!(parse_hex("#FFF").is_err());
    }

    #[test]
    fn empty_config_is_default() {
        let cfg = resolve(FileConfig::default());
        assert_eq!(cfg.theme.background, EditorTheme::dracula().background);
        assert!(cfg.font_family.is_none());
        assert_eq!(cfg.font_size, FONT_SIZE);
    }

    #[test]
    fn builtin_theme_by_name() {
        let f: FileConfig = toml::from_str("[theme]\nname = \"solarized-light\"").unwrap();
        let cfg = resolve(f);
        assert_eq!(
            cfg.theme.background,
            EditorTheme::solarized_light().background
        );
    }

    #[test]
    fn unknown_theme_falls_back_to_dracula() {
        let f: FileConfig = toml::from_str("[theme]\nname = \"nope\"").unwrap();
        let cfg = resolve(f);
        assert_eq!(cfg.theme.background, EditorTheme::dracula().background);
    }

    #[test]
    fn overrides_apply_on_top_of_preset() {
        let f: FileConfig =
            toml::from_str("[theme]\nname = \"nord\"\n[theme.overrides]\nbackground = \"#010203\"")
                .unwrap();
        let cfg = resolve(f);
        assert_eq!(cfg.theme.background, Color::from_rgba8(1, 2, 3, 255));
        assert_eq!(cfg.theme.pink, EditorTheme::nord().pink);
    }

    #[test]
    fn custom_theme_inherits_dracula_and_shadows_builtin() {
        let f: FileConfig = toml::from_str(
            "[theme]\nname = \"dracula\"\n[themes.dracula]\nbackground = \"#101010\"",
        )
        .unwrap();
        let cfg = resolve(f);
        assert_eq!(
            cfg.theme.background,
            Color::from_rgba8(0x10, 0x10, 0x10, 255)
        );
        assert_eq!(cfg.theme.foreground, EditorTheme::dracula().foreground);
    }

    #[test]
    fn font_family_and_size() {
        let f: FileConfig =
            toml::from_str("[font]\nfamily = \"JetBrains Mono\"\nsize = 20.0").unwrap();
        let cfg = resolve(f);
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, 20.0);
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<FileConfig>("[font]\nfamly = \"x\"").is_err());
    }
}
