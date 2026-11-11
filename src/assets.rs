//! Desktop integration written by the binary itself (icons, launcher, user service), so a plain
//! `cargo install` gives a working app-grid entry and tray icon with no packaging step.
use std::path::{Path, PathBuf};

pub const APP_ID: &str = "io.github.Wallspin";

const ICON: &str = include_str!("../data/icons/wallspin.svg");
const ICON_SYMBOLIC: &str = include_str!("../data/icons/wallspin-symbolic.svg");
const SERVICE: &str = include_str!("../contrib/wallspin.service");

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wallspin"))
}

/// Write `content` only when it differs (keeps mtimes stable, avoids icon-cache churn).
fn put(path: &Path, content: &str) {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, content) {
        tracing::warn!("writing {}: {e}", path.display());
    }
}

pub fn desktop_file() -> PathBuf {
    data_home().join("applications").join(format!("{APP_ID}.desktop"))
}

pub fn service_file() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("systemd/user/wallspin.service")
}

fn data_home() -> PathBuf {
    dirs::data_dir().unwrap_or_default()
}

pub fn install() {
    // Distro packages ship the icon, launcher and user unit under /usr; don't duplicate them.
    if exe().starts_with("/usr/") {
        return;
    }
    let icons = data_home().join("icons/hicolor");
    put(&icons.join(format!("scalable/apps/{APP_ID}.svg")), ICON);
    put(&icons.join(format!("symbolic/apps/{APP_ID}-symbolic.svg")), ICON_SYMBOLIC);
    let exe = exe();
    put(
        &desktop_file(),
        &format!(
            "[Desktop Entry]\nName=Wallspin\nComment=Wallpaper changer: topics, per-monitor images, clock\n\
             Exec={} prefs\nIcon={APP_ID}\nTerminal=false\nType=Application\n\
             Categories=GNOME;GTK;Settings;DesktopSettings;\nKeywords=wallpaper;background;variety;slideshow;\n\
             StartupNotify=true\n",
            exe.display()
        ),
    );
    // The user may have customised the unit; only create it if missing.
    let unit = service_file();
    if !unit.exists() {
        put(&unit, &SERVICE.replace("%h/.cargo/bin/wallspin", &exe.to_string_lossy()));
        let _ = std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    }
}

pub fn open_prefs() {
    let _ = std::process::Command::new(exe()).arg("prefs").spawn();
}
