//! GNOME backend via gsettings. One short-lived process per key; nothing resident.
use anyhow::{Context, Result, bail};
use std::path::Path;
use tokio::process::Command;

const BG: &str = "org.gnome.desktop.background";
const SS: &str = "org.gnome.desktop.screensaver";

/// GVariant string literal.
fn gstr(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

async fn gset(schema: &str, key: &str, value: &str) -> Result<()> {
    let out = Command::new("gsettings").args(["set", schema, key, value]).output().await.context("running gsettings")?;
    if !out.status.success() {
        bail!("gsettings set {schema} {key}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

async fn gget(schema: &str, key: &str) -> Result<String> {
    let out = Command::new("gsettings").args(["get", schema, key]).output().await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn file_uri(p: &Path) -> String {
    url::Url::from_file_path(p).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", p.display()))
}

/// Desktop background (GNOME 42+ also uses this, blurred, for the lock screen).
pub async fn set_desktop(path: &Path, mode: &str) -> Result<()> {
    let uri = gstr(&file_uri(path));
    gset(BG, "picture-options", &gstr(mode)).await?;
    gset(BG, "picture-uri", &uri).await?;
    gset(BG, "picture-uri-dark", &uri).await?;
    Ok(())
}

/// Legacy screensaver key: ignored by GNOME Shell ≥ 3.38 but honored by
/// Cinnamon/Budgie/older shells. Cheap to keep in sync.
pub async fn set_screensaver(path: &Path, mode: &str) -> Result<()> {
    gset(SS, "picture-options", &gstr(mode)).await?;
    gset(SS, "picture-uri", &gstr(&file_uri(path))).await
}

// ---------- keyboard shortcuts (the no-extension control surface) ----------

const MK: &str = "org.gnome.settings-daemon.plugins.media-keys";
const KB_BASE: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings";

pub const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("next", "Wallpaper: next", "<Super><Alt>Right"),
    ("prev", "Wallpaper: previous", "<Super><Alt>Left"),
    ("fav", "Wallpaper: favorite", "<Super><Alt>Up"),
    ("trash", "Wallpaper: trash", "<Super><Alt>Down"),
    ("ban", "Wallpaper: ban", "<Super><Alt>Delete"),
];

fn parse_strv(s: &str) -> Vec<String> {
    // '@as []' or "['/a/', '/b/']"
    s.trim_start_matches("@as")
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|x| x.trim().trim_matches('\'').to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

pub async fn install_shortcuts(exe: &Path, remove: bool) -> Result<()> {
    let mut list = parse_strv(&gget(MK, "custom-keybindings").await?);
    // Only ever touch our own bindings.
    for old in list.iter().filter(|p| p.contains("/wallspin-")) {
        let _ = Command::new("dconf").args(["reset", "-f", old]).status().await;
    }
    list.retain(|p| !p.contains("/wallspin-"));
    if !remove {
        for (cmd, name, binding) in SHORTCUTS {
            let path = format!("{KB_BASE}/wallspin-{cmd}/");
            let schema = format!("{MK}.custom-keybinding:{path}");
            gset(&schema, "name", &gstr(name)).await?;
            gset(&schema, "command", &gstr(&format!("'{}' {cmd}", exe.display()))).await?;
            gset(&schema, "binding", &gstr(binding)).await?;
            list.push(path);
        }
    }
    let v = format!("[{}]", list.iter().map(|p| gstr(p)).collect::<Vec<_>>().join(", "));
    gset(MK, "custom-keybindings", if list.is_empty() { "@as []" } else { &v }).await
}
