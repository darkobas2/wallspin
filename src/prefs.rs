//! Preferences window (GTK4/libadwaita). Work in progress.

pub fn run() -> i32 {
    crate::assets::install();
    eprintln!("The preferences window is not finished yet; use the tray menu or `wallspin --help`.");
    1
}
