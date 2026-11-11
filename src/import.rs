//! One-shot migration from ~/.config/variety: sources, folders, filters, bans, history.
use crate::config::{self, Config, LockMode, Provider, Source, Wallhaven};
use crate::db::Db;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;

pub fn run(force: bool) -> Result<String> {
    let vdir = dirs::config_dir().context("no config dir")?.join("variety");
    let conf = std::fs::read_to_string(vdir.join("variety.conf")).context("reading variety.conf")?;
    if config::config_path().exists() && !force {
        bail!("{} exists; pass --force to overwrite", config::config_path().display());
    }
    let (kv, srcs) = parse(&conf);
    let get = |k: &str| kv.get(k).map(|s| s.as_str()).unwrap_or("");
    let on = |k: &str| get(k).eq_ignore_ascii_case("true");
    let num = |k: &str, d: u64| get(k).parse::<u64>().unwrap_or(d);

    let mut cfg = Config::default();
    cfg.download_folder = config::expand(Path::new(get("download_folder")));
    cfg.favorites_folder = config::expand(Path::new(get("favorites_folder")));
    cfg.change_interval = if on("change_enabled") { num("change_interval", 900) } else { 0 };
    cfg.change_on_start = on("change_on_start");
    cfg.download_interval = if on("download_enabled") { num("download_interval", 3600) } else { 0 };
    cfg.quota_mb = if on("quota_enabled") { num("quota_size", 2000) } else { 0 };
    if on("copyto_enabled") && !get("copyto_folder").is_empty() && get("copyto_folder") != "Default" {
        cfg.copy_to = Some(config::expand(Path::new(get("copyto_folder"))));
    }
    cfg.landscape_only = on("use_landscape_enabled");
    if on("min_size_enabled") {
        let pct = num("min_size", 80) as u32;
        let (w, h) = largest_monitor().unwrap_or((1920, 1080));
        cfg.min_width = w * pct / 100;
        cfg.min_height = h * pct / 100;
    } else {
        cfg.min_width = 0;
        cfg.min_height = 0;
    }
    // Variety could only span or repeat; default to a separate image per monitor.
    cfg.display_mode = match current_picture_options().as_deref() {
        Some("spanned") | None => "zoom".into(),
        Some(m) => m.to_string(),
    };
    cfg.keys.wallhaven = get("wallhaven_api_key").trim_matches('"').to_string();
    // GNOME already shows the (blurred) desktop on the lock screen; Variety's option maps to "same".
    cfg.lockscreen.mode = LockMode::Same;

    // Clock: "Roboto Condensed, Light 48" (Pango) -> fontconfig pattern + size.
    cfg.clock.enabled = on("clock_enabled");
    if let Some((pat, size)) = pango_font(get("clock_font")) {
        cfg.clock.font = pat;
        cfg.clock.size = size;
    }
    if let Some((pat, size)) = pango_font(get("clock_date_font")) {
        cfg.clock.date_font = pat;
        cfg.clock.date_size = size;
    }

    let mut notes = Vec::new();
    cfg.sources.clear();
    let existing_dirs: Vec<String> = std::fs::read_dir(&cfg.download_folder)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    for (enabled, kind, arg) in srcs {
        let provider = match kind.as_str() {
            "favorites" => Provider::Favorites,
            "folder" => Provider::Folder { path: config::expand(Path::new(&arg)) },
            "fetched" => Provider::Folder { path: config::expand(Path::new(get("fetched_folder"))) },
            "wallhaven" => match wallhaven_from_url(&arg) {
                Some(w) => Provider::Wallhaven(w),
                None => {
                    notes.push(format!("skipped unparseable wallhaven source {arg}"));
                    continue;
                }
            },
            "artstation" => match artstation_user(&arg) {
                Some(user) => Provider::Artstation { user },
                None => continue,
            },
            "bing" => Provider::Bing { market: "en-US".into() },
            "apod" => Provider::Apod,
            "unsplash" => Provider::Unsplash { query: "wallpaper".into() },
            other => {
                if enabled {
                    notes.push(format!("skipped unsupported provider {other}"));
                }
                continue;
            }
        };
        // Reuse Variety's existing download folder for this source if there is one.
        let dir = if provider.is_remote() { variety_dir(&kind, &arg, &existing_dirs) } else { None };
        if cfg.sources.iter().any(|s| s.provider == provider) {
            continue;
        }
        cfg.sources.push(Source { enabled, dir, provider });
    }
    cfg.save()?;

    // Bans and history.
    let mut db = Db::open(&config::db_path())?;
    let banned = std::fs::read_to_string(vdir.join("banned.txt")).unwrap_or_default();
    let history = std::fs::read_to_string(vdir.join("history.txt")).unwrap_or_default();
    let (nb, nh) = db.transaction(|tx| {
        let mut nb = 0;
        for l in banned.lines().map(str::trim).filter(|l| !l.is_empty()) {
            nb += tx.execute("INSERT OR IGNORE INTO banned (key, ts) VALUES (?1, ?2)", (l, crate::db::now()))?;
        }
        // history.txt: first line is a position, then paths newest-first.
        let paths: Vec<&str> =
            history.lines().skip(1).map(str::trim).filter(|l| l.starts_with('/') && Path::new(l).exists()).collect();
        let mut nh = 0;
        for p in paths.iter().rev() {
            nh += tx.execute("INSERT INTO history (path, ts) VALUES (?1, ?2)", (p, crate::db::now()))?;
        }
        Ok((nb, nh))
    })?;

    let mut out = format!(
        "wrote {}\n  {} sources ({} enabled), {} bans, {} history entries\n  min size {}x{}, monitors {:?}, downloads in {}\n",
        config::config_path().display(),
        cfg.sources.len(),
        cfg.sources.iter().filter(|s| s.enabled).count(),
        nb,
        nh,
        cfg.min_width,
        cfg.min_height,
        cfg.monitors,
        cfg.download_folder.display()
    );
    for n in notes {
        out.push_str(&format!("  note: {n}\n"));
    }
    Ok(out)
}

/// Returns top-level key/values and [sources] entries as (enabled, kind, arg).
fn parse(conf: &str) -> (HashMap<String, String>, Vec<(bool, String, String)>) {
    let mut kv = HashMap::new();
    let mut srcs = Vec::new();
    let mut section = String::new();
    for line in conf.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            section = l.trim_matches(|c| c == '[' || c == ']').to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match section.as_str() {
            "" => {
                kv.insert(k.to_string(), v.trim_matches('"').to_string());
            }
            "sources" => {
                let mut parts = v.splitn(3, '|');
                let (Some(en), Some(kind), arg) = (parts.next(), parts.next(), parts.next().unwrap_or("")) else {
                    continue;
                };
                srcs.push((en.eq_ignore_ascii_case("true"), kind.to_string(), arg.to_string()));
            }
            _ => {}
        }
    }
    (kv, srcs)
}

/// https://wallhaven.cc/search?q=...&categories=110&purity=100&atleast=... -> structured topic
pub fn wallhaven_from_url(s: &str) -> Option<Wallhaven> {
    let u = url::Url::parse(s).ok()?;
    if !u.host_str()?.ends_with("wallhaven.cc") {
        return None;
    }
    let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
    let bits = |v: Option<&String>, names: [&str; 3], d: &[&str]| -> Vec<String> {
        match v {
            Some(b) if b.len() == 3 => {
                names.iter().zip(b.chars()).filter(|(_, c)| *c == '1').map(|(n, _)| n.to_string()).collect()
            }
            _ => d.iter().map(|s| s.to_string()).collect(),
        }
    };
    Some(Wallhaven {
        query: q.get("q").cloned().unwrap_or_default(),
        categories: bits(q.get("categories"), ["general", "anime", "people"], &["general", "people"]),
        purity: bits(q.get("purity"), ["sfw", "sketchy", "nsfw"], &["sfw"]),
        min_resolution: q.get("atleast").cloned(),
        resolutions: q.get("resolutions").cloned(),
        ratios: q.get("ratios").cloned(),
        sorting: q.get("sorting").cloned().unwrap_or_else(|| "relevance".into()),
        top_range: q.get("topRange").cloned(),
    })
}

pub fn artstation_user(s: &str) -> Option<String> {
    let u = url::Url::parse(s).ok()?;
    let seg = u.path_segments()?.next()?.trim_end_matches(".rss").to_string();
    (!seg.is_empty()).then_some(seg)
}

/// Variety names folders `<kind>_<url with non-alnum -> _>`, truncated + hashed when long.
fn variety_dir(kind: &str, arg: &str, existing: &[String]) -> Option<String> {
    let bare = arg.split_once("://").map(|x| x.1).unwrap_or(arg);
    let mut name = format!("{kind}_");
    name.extend(bare.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }));
    let prefix: String = name.chars().take(80).collect();
    existing.iter().find(|d| d.starts_with(&prefix) || name.starts_with(d.as_str())).cloned()
}

/// Largest *currently active* monitor mode, from Mutter (works on Wayland).
fn largest_monitor() -> Option<(u32, u32)> {
    let out = std::process::Command::new("gdbus")
        .args([
            "call", "--session", "--dest", "org.gnome.Mutter.DisplayConfig",
            "--object-path", "/org/gnome/Mutter/DisplayConfig",
            "--method", "org.gnome.Mutter.DisplayConfig.GetCurrentState",
        ])
        .output()
        .ok()?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let re = regex::Regex::new(r"\('[^']+', (\d+), (\d+), [0-9.]+, [0-9.]+, \[[^\]]*\], \{[^}]*'is-current': <true>").ok()?;
    re.captures_iter(&txt)
        .filter_map(|c| Some((c[1].parse::<u32>().ok()?, c[2].parse::<u32>().ok()?)))
        .max_by_key(|(w, h)| w * h)
}

fn current_picture_options() -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.background", "picture-options"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
    (!s.is_empty()).then_some(s)
}

/// "Roboto Condensed, Light 48" -> ("Roboto Condensed:style=Light", 48.0)
fn pango_font(s: &str) -> Option<(String, f32)> {
    let s = s.trim().trim_matches('"');
    let (rest, size) = s.rsplit_once(' ')?;
    let size: f32 = size.parse().ok()?;
    let (family, style) = match rest.split_once(',') {
        Some((f, st)) => (f.trim(), st.trim()),
        None => (rest.trim(), ""),
    };
    Some((if style.is_empty() { family.to_string() } else { format!("{family}:style={style}") }, size))
}
