use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const APP: &str = "wallspin";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Seconds between automatic changes; 0 = manual only.
    pub change_interval: u64,
    pub change_on_start: bool,
    /// Seconds between download rounds; 0 = never download automatically.
    pub download_interval: u64,
    /// Images fetched per download round (rotating across enabled sources).
    pub download_batch: usize,
    pub download_folder: PathBuf,
    pub favorites_folder: PathBuf,
    /// Download folder size cap in MB; oldest files are pruned beyond it.
    pub quota_mb: u64,
    /// Copy the active wallpaper here and point GNOME at the copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_to: Option<PathBuf>,
    pub min_width: u32,
    pub min_height: u32,
    pub landscape_only: bool,
    /// Multi-monitor behaviour: each (different image per monitor), same, span.
    pub monitors: MonitorMode,
    /// GNOME picture-options used in `same` mode without the clock (zoom, scaled, centered, ...).
    pub display_mode: String,
    /// Show an actionable notification on every change (works without any extension).
    pub notify: bool,
    /// Show a tray icon when a StatusNotifier host exists (AppIndicator ext, KDE, waybar).
    pub tray: bool,
    /// Do not repeat any of the last N wallpapers.
    pub avoid_recent: usize,
    pub keys: Keys,
    pub clock: ClockConfig,
    pub lockscreen: LockConfig,
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    pub enabled: bool,
    /// fontconfig pattern, e.g. "Roboto Condensed:style=Light"
    pub font: String,
    /// points (Variety-compatible: rendered at 100 dpi, scaled to the primary monitor height)
    pub size: f32,
    pub date_font: String,
    pub date_size: f32,
    /// chrono/strftime formats
    pub time_format: String,
    pub date_format: String,
    /// bottom-right, bottom-left, top-right, top-left (on the primary monitor)
    pub position: String,
    pub shadow: bool,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            font: "Sans:style=Light".into(),
            size: 48.0,
            date_font: "Sans:style=Light".into(),
            date_size: 30.0,
            time_format: "%H:%M".into(),
            date_format: "%A, %B %d".into(),
            position: "bottom-right".into(),
            shadow: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub wallhaven: String,
    pub unsplash: String,
    /// NASA API key; empty uses DEMO_KEY (rate-limited).
    pub nasa: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MonitorMode {
    /// A different image on every monitor, each cropped to that monitor's shape.
    #[default]
    Each,
    /// The same image on every monitor.
    Same,
    /// One image stretched across all monitors.
    Span,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockMode {
    /// Lock screen shows the (blurred) desktop wallpaper — GNOME default.
    #[default]
    Same,
    /// Swap in a different image while locked, restore on unlock.
    Separate,
    /// Don't touch lock screen settings.
    Off,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LockConfig {
    pub mode: LockMode,
    /// Folder to pick lock images from; default: favorites, then the normal pool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Subfolder of download_folder; derived from the provider settings if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(flatten)]
    pub provider: Provider,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum Provider {
    Favorites,
    Folder {
        path: PathBuf,
    },
    Wallhaven(Wallhaven),
    Artstation {
        user: String,
    },
    Bing {
        #[serde(default = "default_market")]
        market: String,
    },
    Apod,
    Unsplash {
        query: String,
    },
}

fn default_market() -> String {
    "en-US".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wallhaven {
    pub query: String,
    /// any of: general, anime, people
    pub categories: Vec<String>,
    /// any of: sfw, sketchy, nsfw (nsfw needs an API key)
    pub purity: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_resolution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolutions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratios: Option<String>,
    /// relevance, random, toplist, views, favorites, date_added
    pub sorting: String,
    /// toplist range: 1d 3d 1w 1M 3M 6M 1y
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_range: Option<String>,
}

impl Default for Wallhaven {
    fn default() -> Self {
        Self {
            query: String::new(),
            categories: vec!["general".into(), "people".into()],
            purity: vec!["sfw".into()],
            min_resolution: None,
            resolutions: None,
            ratios: Some("landscape".into()),
            sorting: "relevance".into(),
            top_range: None,
        }
    }
}

impl Provider {
    pub fn is_remote(&self) -> bool {
        !matches!(self, Provider::Favorites | Provider::Folder { .. })
    }

    pub fn label(&self) -> String {
        match self {
            Provider::Favorites => "Favorites".into(),
            Provider::Folder { path } => format!("Folder: {}", path.display()),
            Provider::Wallhaven(w) => {
                let mut s = format!("Wallhaven: {}", if w.query.is_empty() { "(any)" } else { &w.query });
                if let Some(r) = w.min_resolution.as_ref().or(w.resolutions.as_ref()) {
                    s.push_str(&format!(" ≥{r}"));
                }
                if w.sorting != "relevance" {
                    s.push_str(&format!(" [{}]", w.sorting));
                }
                s
            }
            Provider::Artstation { user } => format!("ArtStation: {user}"),
            Provider::Bing { market } => format!("Bing daily ({market})"),
            Provider::Apod => "NASA Astronomy Picture of the Day".into(),
            Provider::Unsplash { query } => format!("Unsplash: {query}"),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let data = data_dir();
        Self {
            change_interval: 0,
            change_on_start: false,
            download_interval: 3600,
            download_batch: 3,
            download_folder: data.join("Downloaded"),
            favorites_folder: data.join("Favorites"),
            quota_mb: 2000,
            copy_to: None,
            min_width: 1920,
            min_height: 1080,
            landscape_only: true,
            monitors: MonitorMode::Each,
            display_mode: "zoom".into(),
            notify: true,
            tray: true,
            avoid_recent: 50,
            keys: Keys::default(),
            clock: ClockConfig::default(),
            lockscreen: LockConfig::default(),
            sources: vec![
                Source { enabled: true, dir: None, provider: Provider::Favorites },
                Source {
                    enabled: true,
                    dir: None,
                    provider: Provider::Wallhaven(Wallhaven { query: "landscape".into(), ..Default::default() }),
                },
            ],
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP)
}
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP)
}
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}
pub fn db_path() -> PathBuf {
    data_dir().join("state.db")
}

pub fn expand(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) if s == "~" || s.starts_with("~/") => {
            dirs::home_dir().unwrap_or_default().join(s.trim_start_matches('~').trim_start_matches('/'))
        }
        _ => p.to_path_buf(),
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let mut cfg: Config = if path.exists() {
            let txt = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&txt).with_context(|| format!("parsing {}", path.display()))?
        } else {
            let cfg = Config::default();
            cfg.save()?;
            cfg
        };
        cfg.download_folder = expand(&cfg.download_folder);
        cfg.favorites_folder = expand(&cfg.favorites_folder);
        cfg.copy_to = cfg.copy_to.as_deref().map(expand);
        cfg.lockscreen.folder = cfg.lockscreen.folder.as_deref().map(expand);
        for s in &mut cfg.sources {
            if let Provider::Folder { path } = &mut s.provider {
                *path = expand(path);
            }
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let txt = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, txt)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Local folder that holds images for a source.
    pub fn source_dir(&self, s: &Source) -> PathBuf {
        match &s.provider {
            Provider::Favorites => self.favorites_folder.clone(),
            Provider::Folder { path } => path.clone(),
            p => self.download_folder.join(s.dir.clone().unwrap_or_else(|| default_dir_name(p))),
        }
    }
}

/// Stable, readable folder name derived from provider settings.
pub fn default_dir_name(p: &Provider) -> String {
    let (name, key) = match p {
        Provider::Wallhaven(w) => ("wallhaven", w.query.clone()),
        Provider::Artstation { user } => ("artstation", user.clone()),
        Provider::Bing { market } => ("bing", market.clone()),
        Provider::Apod => ("apod", String::new()),
        Provider::Unsplash { query } => ("unsplash", query.clone()),
        _ => ("local", String::new()),
    };
    let slug: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    // Hash the full settings so different filter combos don't share a folder.
    let h = fnv(&serde_json::to_string(p).unwrap_or_default());
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() { format!("{name}-{h:08x}") } else { format!("{name}-{slug}-{h:08x}") }
}

pub fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}
