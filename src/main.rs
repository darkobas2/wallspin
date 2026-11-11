mod assets;
mod clock;
mod config;
mod daemon;
mod db;
mod gnome;
mod import;
mod prefs;
mod providers;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Provider, Source, Wallhaven};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "wallspin", version, about = "Wallpaper changer for GNOME. Run without arguments to open Preferences.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the preferences window (default)
    Prefs {
        /// Start on a page: wallpaper, sources, downloads, controls
        #[arg(long)]
        page: Option<String>,
    },
    /// Run the background daemon
    Daemon,
    /// Next wallpaper
    Next,
    /// Previous wallpaper
    Prev,
    /// Copy current wallpaper to favorites
    Fav,
    /// Never show current wallpaper again
    Ban,
    /// Move current wallpaper to trash
    Trash,
    /// Show daemon status
    Status,
    /// Download images now
    Download { n: Option<u32> },
    /// Re-read config.toml
    Reload,
    /// Stop the daemon
    Quit,
    /// Manage wallpaper sources (topics, artists, daily photos, folders)
    #[command(subcommand)]
    Source(SourceCmd),
    /// Import settings, bans and history from ~/.config/variety
    ImportVariety {
        #[arg(long)]
        force: bool,
    },
    /// Install GNOME keyboard shortcuts (Super+Alt+←/→/↑/↓/Del)
    Shortcuts {
        #[arg(long)]
        remove: bool,
    },
    /// Multi-monitor mode: each (different per monitor), same, span
    Monitors { mode: config::MonitorMode },
    /// Turn the clock overlay on/off
    Clock {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Print config file path
    ConfigPath,
}

#[derive(Subcommand)]
enum SourceCmd {
    /// List sources
    #[command(alias = "ls")]
    List,
    /// Add a source
    #[command(subcommand)]
    Add(AddCmd),
    /// Enable source N (from `source list`)
    Enable { n: usize },
    /// Disable source N
    Disable { n: usize },
    /// Remove source N
    #[command(alias = "remove")]
    Rm { n: usize },
    /// Show suggested Wallhaven topics
    Topics,
}

#[derive(Subcommand)]
enum AddCmd {
    /// Wallhaven search topic, e.g. `wallhaven space` or `wallhaven "#landscape" --sort toplist`
    Wallhaven {
        /// Search terms / #tags
        query: Vec<String>,
        /// general,anime,people
        #[arg(long, value_delimiter = ',', default_values = ["general", "people"])]
        category: Vec<String>,
        /// sfw,sketchy,nsfw
        #[arg(long, value_delimiter = ',', default_values = ["sfw"])]
        purity: Vec<String>,
        /// Minimum resolution, e.g. 3440x1440
        #[arg(long)]
        min_res: Option<String>,
        /// Exact resolution(s), e.g. 3440x1440
        #[arg(long)]
        res: Option<String>,
        /// Aspect ratio filter (landscape, portrait, 21x9, ...)
        #[arg(long, default_value = "landscape")]
        ratio: String,
        /// relevance, random, toplist, views, favorites, date_added
        #[arg(long, default_value = "relevance")]
        sort: String,
        /// toplist range: 1d 3d 1w 1M 3M 6M 1y
        #[arg(long)]
        top: Option<String>,
    },
    /// ArtStation artist (username or profile URL)
    Artstation { user: String },
    /// Bing photo of the day
    Bing {
        #[arg(long, default_value = "en-US")]
        market: String,
    },
    /// NASA Astronomy Picture of the Day
    Apod,
    /// Unsplash topic (needs keys.unsplash)
    Unsplash { query: Vec<String> },
    /// Local folder
    Folder { path: PathBuf },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wallspin=info,warn".into()),
        )
        .without_time() // journald adds timestamps
        .init();

    tune_malloc();
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        // GTK runs its own main loop; no tokio runtime for the preferences window.
        None => std::process::exit(prefs::run(None)),
        Some(Cmd::Prefs { page }) => std::process::exit(prefs::run(page)),
        Some(c) => c,
    };
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?.block_on(run(cmd))
}

async fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Prefs { .. } => unreachable!(),
        Cmd::Daemon => {
            assets::install();
            daemon::run().await
        }
        Cmd::Next => remote("Next").await,
        Cmd::Prev => remote("Prev").await,
        Cmd::Fav => remote("Favorite").await,
        Cmd::Ban => remote("Ban").await,
        Cmd::Trash => remote("Trash").await,
        Cmd::Status => remote("Status").await,
        Cmd::Reload => remote("Reload").await,
        Cmd::Quit => remote("Quit").await,
        Cmd::Download { n } => {
            let s = call("Download", &(n.unwrap_or(0),)).await?;
            println!("{s}");
            Ok(())
        }
        Cmd::Source(sc) => source_cmd(sc).await,
        Cmd::ImportVariety { force } => {
            print!("{}", import::run(force)?);
            let _ = call("Reload", &()).await;
            Ok(())
        }
        Cmd::Shortcuts { remove } => {
            let exe = std::env::current_exe()?;
            gnome::install_shortcuts(&exe, remove).await?;
            if remove {
                println!("removed wallspin shortcuts");
            } else {
                for (_, name, key) in gnome::SHORTCUTS {
                    println!("  {key:<22} {name}");
                }
            }
            Ok(())
        }
        Cmd::Monitors { mode } => {
            let mut cfg = Config::load()?;
            cfg.monitors = mode;
            cfg.save()?;
            println!("monitors: {mode:?}");
            let _ = call("Reload", &()).await;
            Ok(())
        }
        Cmd::Clock { state } => {
            let mut cfg = Config::load()?;
            cfg.clock.enabled = state == "on";
            cfg.save()?;
            println!("clock {state}");
            let _ = call("Reload", &()).await;
            Ok(())
        }
        Cmd::ConfigPath => {
            println!("{}", config::config_path().display());
            Ok(())
        }
    }
}

async fn call<B>(method: &str, body: &B) -> Result<String>
where
    B: serde::Serialize + zbus::DynamicType,
{
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&conn, daemon::BUS_NAME, daemon::OBJ_PATH, daemon::IFACE).await?;
    proxy.call(method, body).await.map_err(|e| match e {
        zbus::Error::MethodError(name, msg, _) if name.as_str().ends_with("ServiceUnknown") => {
            anyhow::anyhow!("daemon not running (start it with `wallspin daemon` or the systemd unit){}", msg.map(|_| "").unwrap_or(""))
        }
        e => anyhow::anyhow!("{e}"),
    })
}

async fn remote(method: &str) -> Result<()> {
    let s = call(method, &()).await?;
    if !s.is_empty() {
        println!("{s}");
    }
    Ok(())
}

async fn source_cmd(sc: SourceCmd) -> Result<()> {
    let mut cfg = Config::load()?;
    let idx = |n: usize, cfg: &Config| -> Result<usize> {
        if n == 0 || n > cfg.sources.len() {
            bail!("no source #{n}; see `wallspin source list`");
        }
        Ok(n - 1)
    };
    let msg = match sc {
        SourceCmd::List => {
            for (i, s) in cfg.sources.iter().enumerate() {
                let n = providers::scan_dir(&cfg.source_dir(s)).len();
                println!("{:>3}  [{}] {}  ({n} local)", i + 1, if s.enabled { "x" } else { " " }, s.provider.label());
            }
            return Ok(());
        }
        SourceCmd::Topics => {
            println!("Suggested topics (add with `wallspin source add wallhaven <topic>`):");
            for t in daemon::SUGGESTED_TOPICS {
                println!("  {t}");
            }
            println!("Tips: #tag for exact tags, id:<n> for a tag id, combine words freely.");
            return Ok(());
        }
        SourceCmd::Add(a) => {
            let p = match a {
                AddCmd::Wallhaven { query, category, purity, min_res, res, ratio, sort, top } => {
                    Provider::Wallhaven(Wallhaven {
                        query: query.join(" "),
                        categories: category,
                        purity,
                        min_resolution: min_res,
                        resolutions: res,
                        ratios: Some(ratio).filter(|r| !r.is_empty() && r != "any"),
                        sorting: sort,
                        top_range: top,
                    })
                }
                AddCmd::Artstation { user } => Provider::Artstation {
                    user: import::artstation_user(&user)
                        .unwrap_or(user)
                        .trim_start_matches('@')
                        .to_string(),
                },
                AddCmd::Bing { market } => Provider::Bing { market },
                AddCmd::Apod => Provider::Apod,
                AddCmd::Unsplash { query } => Provider::Unsplash { query: query.join(" ") },
                AddCmd::Folder { path } => Provider::Folder { path: std::fs::canonicalize(&path).context("folder")? },
            };
            let label = p.label();
            match cfg.sources.iter_mut().find(|s| s.provider == p) {
                Some(s) => s.enabled = true,
                None => cfg.sources.push(Source { enabled: true, dir: None, provider: p }),
            }
            format!("added {label}")
        }
        SourceCmd::Enable { n } => {
            let i = idx(n, &cfg)?;
            cfg.sources[i].enabled = true;
            format!("enabled {}", cfg.sources[i].provider.label())
        }
        SourceCmd::Disable { n } => {
            let i = idx(n, &cfg)?;
            cfg.sources[i].enabled = false;
            format!("disabled {}", cfg.sources[i].provider.label())
        }
        SourceCmd::Rm { n } => {
            let i = idx(n, &cfg)?;
            let s = cfg.sources.remove(i);
            format!("removed {} (downloaded files kept in {})", s.provider.label(), cfg.source_dir(&s).display())
        }
    };
    cfg.save()?;
    println!("{msg}");
    if call("Reload", &()).await.is_ok() {
        let _ = call("Download", &(0u32,)).await;
    }
    Ok(())
}

/// Make glibc hand image-sized buffers straight back to the OS. By default its mmap threshold
/// grows after big frees, so later decodes land on the heap and RSS ratchets up; pinning it
/// keeps every multi-MB buffer mmapped (unmapped on free). Fewer arenas = less fragmentation
/// across tokio's blocking threads.
fn tune_malloc() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

/// Return freed heap pages to the OS (cheap; called after each render).
pub fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}
