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

/// GNOME uses Super+Alt+arrows for workspaces/overview. Super+Alt+Page Up/Down mirrors
/// Super+Page Up/Down (workspaces) without colliding; the rest are Super+Alt+letter.
pub const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("next", "Wallpaper: next", "<Super><Alt>Page_Down"),
    ("prev", "Wallpaper: back", "<Super><Alt>Page_Up"),
    ("fav", "Wallpaper: favorite", "<Super><Alt>f"),
    ("trash", "Wallpaper: trash", "<Super><Alt>t"),
    ("ban", "Wallpaper: ban", "<Super><Alt>x"),
];

/// Schemas whose keys hold accelerators we must not collide with.
const BINDING_SCHEMAS: &[&str] = &[
    "org.gnome.desktop.wm.keybindings",
    "org.gnome.mutter.keybindings",
    "org.gnome.mutter.wayland.keybindings",
    "org.gnome.shell.keybindings",
    "org.gnome.settings-daemon.plugins.media-keys",
];

/// "<Primary><alt>N" -> ({"alt","control"}, "n"), so equivalent spellings compare equal.
fn norm_accel(a: &str) -> Option<(std::collections::BTreeSet<String>, String)> {
    let mut mods = std::collections::BTreeSet::new();
    let mut rest = a.trim();
    while let Some(r) = rest.strip_prefix('<') {
        let (m, after) = r.split_once('>')?;
        let m = match m.to_lowercase().as_str() {
            "primary" | "ctrl" | "control" => "control".to_string(),
            "mod4" | "super" => "super".to_string(),
            "mod1" | "alt" => "alt".to_string(),
            other => other.to_string(),
        };
        mods.insert(m);
        rest = after;
    }
    (!rest.is_empty()).then(|| (mods, rest.to_lowercase()))
}

/// Every accelerator currently bound in GNOME, except our own custom shortcuts.
async fn taken_accels(custom_paths: &[String]) -> Vec<(std::collections::BTreeSet<String>, String, String)> {
    let mut out = Vec::new();
    let re = regex::Regex::new(r"'([^']+)'").unwrap();
    for schema in BINDING_SCHEMAS {
        let Ok(o) = Command::new("gsettings").args(["list-recursively", schema]).output().await else { continue };
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut it = line.splitn(3, ' ');
            let (_, key, val) = (it.next(), it.next().unwrap_or(""), it.next().unwrap_or(""));
            for c in re.captures_iter(val) {
                if let Some((m, k)) = norm_accel(&c[1]) {
                    out.push((m, k, key.to_string()));
                }
            }
        }
    }
    for p in custom_paths {
        let schema = format!("{MK}.custom-keybinding:{p}");
        let b = gget(&schema, "binding").await.unwrap_or_default();
        let name = gget(&schema, "name").await.unwrap_or_default();
        if let Some((m, k)) = norm_accel(b.trim_matches('\'')) {
            out.push((m, k, format!("custom shortcut {name}")));
        }
    }
    out
}

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

/// Installs (or removes) our shortcuts. Returns warnings for any that were skipped because
/// the key combination is already used elsewhere — we never steal a binding.
pub async fn install_shortcuts(exe: &Path, remove: bool) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let mut list = parse_strv(&gget(MK, "custom-keybindings").await?);
    // Only ever touch our own bindings.
    for old in list.iter().filter(|p| p.contains("/wallspin-")) {
        let _ = Command::new("dconf").args(["reset", "-f", old]).status().await;
    }
    list.retain(|p| !p.contains("/wallspin-"));
    if !remove {
        let taken = taken_accels(&list).await;
        for (cmd, name, binding) in SHORTCUTS {
            let want = norm_accel(binding);
            if let Some((_, _, by)) = taken.iter().find(|(m, k, _)| want.as_ref() == Some(&(m.clone(), k.clone()))) {
                warnings.push(format!("skipped {binding} for {name}: already used by {by}"));
                continue;
            }
            let path = format!("{KB_BASE}/wallspin-{cmd}/");
            let schema = format!("{MK}.custom-keybinding:{path}");
            gset(&schema, "name", &gstr(name)).await?;
            gset(&schema, "command", &gstr(&format!("'{}' {cmd}", exe.display()))).await?;
            gset(&schema, "binding", &gstr(binding)).await?;
            list.push(path);
        }
    }
    let v = format!("[{}]", list.iter().map(|p| gstr(p)).collect::<Vec<_>>().join(", "));
    gset(MK, "custom-keybindings", if list.is_empty() { "@as []" } else { &v }).await?;
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accel_spellings_compare_equal() {
        assert_eq!(norm_accel("<Primary><alt>N"), norm_accel("<Alt><Control>n"));
        assert_ne!(norm_accel("<Super><Alt>Left"), norm_accel("<Super>Left"));
        assert_eq!(norm_accel("bare"), Some((Default::default(), "bare".into())));
    }

    /// Needs a GNOME session: GNOME binds Super+Alt+Left to switch-to-workspace-left by default.
    #[tokio::test]
    #[ignore]
    async fn detects_gnome_default_binding() {
        let taken = taken_accels(&[]).await;
        let want = norm_accel("<Alt><Super>Left").unwrap();
        assert!(taken.iter().any(|(m, k, _)| (m.clone(), k.clone()) == want));
        for (_, _, b) in SHORTCUTS {
            let w = norm_accel(b).unwrap();
            assert!(!taken.iter().any(|(m, k, _)| (m.clone(), k.clone()) == w), "{b} is taken");
        }
    }
}
