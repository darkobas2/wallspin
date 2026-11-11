//! Preferences window (GTK4 + libadwaita), a separate process from the daemon.
//!
//! Every change is applied immediately: the config is re-read from disk, modified, saved, and
//! the daemon is told to reload. Re-reading first means we never clobber changes the daemon
//! made itself (tray toggles, sources added from the tray).
use crate::config::{ClockConfig, Config, LockMode, MonitorMode, Provider, Source, Wallhaven};
use crate::{assets, daemon, gnome, providers};
use adw::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

struct State {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    reload_pending: Cell<bool>,
    sources_group: adw::PreferencesGroup,
    source_rows: RefCell<Vec<gtk::Widget>>,
    /// Defaults applied to newly added Wallhaven topics.
    new_min_res: Cell<u32>,
    new_sort: Cell<u32>,
}

type S = Rc<State>;

const MIN_RES: &[(&str, Option<&str>)] = &[
    ("Any", None),
    ("1920 × 1080", Some("1920x1080")),
    ("2560 × 1440", Some("2560x1440")),
    ("3440 × 1440", Some("3440x1440")),
    ("3840 × 2160", Some("3840x2160")),
];
const SORTS: &[(&str, &str, Option<&str>)] = &[
    ("Most relevant", "relevance", None),
    ("Random", "random", None),
    ("Top this month", "toplist", Some("1M")),
    ("Top this year", "toplist", Some("1y")),
];

pub fn run(page: Option<String>) -> i32 {
    assets::install();
    let app = adw::Application::builder().application_id(assets::APP_ID).build();
    app.connect_activate(move |app| build(app, page.as_deref()));
    // Don't hand our CLI arguments to GApplication.
    app.run_with_args(&["wallspin"]).get() as i32
}

fn build(app: &adw::Application, page: Option<&str>) {
    if let Some(w) = app.active_window() {
        w.present();
        return;
    }
    let cfg = Config::load().unwrap_or_default();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Wallspin")
        .icon_name(assets::APP_ID)
        .default_width(760)
        .default_height(820)
        .build();
    let stack = adw::ViewStack::new();
    let header = adw::HeaderBar::new();
    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    header.set_title_widget(Some(&switcher));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let sources_group = adw::PreferencesGroup::builder()
        .title("Your sources")
        .description("Switch sources on or off. Images from enabled sources are mixed together.")
        .build();
    let s: S = Rc::new(State {
        window: window.clone(),
        toasts,
        reload_pending: Cell::new(false),
        sources_group,
        source_rows: RefCell::new(Vec::new()),
        new_min_res: Cell::new(2),
        new_sort: Cell::new(0),
    });

    stack.add_titled_with_icon(&page_wallpaper(&s, &cfg), Some("wallpaper"), "Wallpaper", "preferences-desktop-wallpaper-symbolic");
    stack.add_titled_with_icon(&page_sources(&s), Some("sources"), "Sources", "folder-pictures-symbolic");
    stack.add_titled_with_icon(&page_downloads(&s, &cfg), Some("downloads"), "Downloads", "folder-download-symbolic");
    stack.add_titled_with_icon(&page_controls(&s, &cfg), Some("controls"), "Controls", "input-keyboard-symbolic");
    if let Some(p) = page {
        stack.set_visible_child_name(p);
    }
    window.present();
}

// ===================== plumbing =====================

/// Apply a change to the on-disk config and nudge the daemon (debounced).
fn edit(s: &S, f: impl FnOnce(&mut Config)) {
    let mut cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => return toast(s, &format!("Config error: {e:#}")),
    };
    f(&mut cfg);
    if let Err(e) = cfg.save() {
        return toast(s, &format!("Saving failed: {e:#}"));
    }
    if !s.reload_pending.replace(true) {
        let s2 = s.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            s2.reload_pending.set(false);
            cli(&s2, &["reload"], false);
        });
    }
}

fn toast(s: &S, msg: &str) {
    s.toasts.add_toast(adw::Toast::new(msg));
}

/// Run `wallspin <args>` asynchronously; optionally toast its first output line.
fn cli(s: &S, args: &[&str], show: bool) {
    cli_then(s, args, show, |_| {});
}

fn cli_then(s: &S, args: &[&str], show: bool, then: impl FnOnce(bool) + 'static) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("wallspin"));
    let mut argv: Vec<&std::ffi::OsStr> = vec![exe.as_os_str()];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    run_proc(s, &argv, show, then);
}

fn run_proc(s: &S, argv: &[&std::ffi::OsStr], show: bool, then: impl FnOnce(bool) + 'static) {
    let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE;
    let proc = match gio::Subprocess::newv(argv, flags) {
        Ok(p) => p,
        Err(e) => return toast(s, &format!("{e}")),
    };
    let s = s.clone();
    let p2 = proc.clone();
    proc.communicate_utf8_async(None, None::<&gio::Cancellable>, move |res| {
        let ok = p2.is_successful();
        if let Ok((Some(out), _)) = res {
            let line = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
            if show && !line.is_empty() {
                toast(&s, &line.replace("Error: ", ""));
            } else if !ok && line.contains("daemon not running") {
                toast(&s, "The background service isn't running — start it on the Controls page");
            }
        }
        then(ok);
    });
}

fn sh_ok(argv: &[&str]) -> bool {
    std::process::Command::new(argv[0]).args(&argv[1..]).output().map(|o| o.status.success()).unwrap_or(false)
}

// ===================== row helpers =====================

fn switch_row(title: &str, subtitle: &str, active: bool, on: impl Fn(bool) + 'static) -> adw::SwitchRow {
    let r = adw::SwitchRow::builder().title(title).subtitle(subtitle).active(active).build();
    r.connect_active_notify(move |r| on(r.is_active()));
    r
}

#[allow(clippy::too_many_arguments)]
fn spin_row(title: &str, subtitle: &str, min: f64, max: f64, step: f64, value: f64, on: impl Fn(f64) + 'static) -> adw::SpinRow {
    let r = adw::SpinRow::with_range(min, max, step);
    r.set_title(title);
    r.set_subtitle(subtitle);
    r.set_value(value);
    r.connect_value_notify(move |r| on(r.value()));
    r
}

fn combo_row(title: &str, subtitle: &str, items: &[&str], selected: u32, on: impl Fn(u32) + 'static) -> adw::ComboRow {
    let r = adw::ComboRow::builder().title(title).subtitle(subtitle).build();
    r.set_model(Some(&gtk::StringList::new(items)));
    r.set_selected(selected);
    r.connect_selected_notify(move |r| on(r.selected()));
    r
}

fn entry_row(title: &str, text: &str, on: impl Fn(String) + 'static) -> adw::EntryRow {
    let r = adw::EntryRow::builder().title(title).text(text).build();
    r.connect_changed(move |r| on(r.text().to_string()));
    r
}

fn button(label: &str, css: &[&str]) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.set_valign(gtk::Align::Center);
    for c in css {
        b.add_css_class(c);
    }
    b
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b
}

fn folder_row(s: &S, title: &str, path: Option<PathBuf>, empty: &str, on: impl Fn(PathBuf) + 'static) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| empty.to_string()))
        .activatable(true)
        .build();
    let b = icon_button("document-open-symbolic", "Choose folder");
    row.add_suffix(&b);
    row.set_activatable_widget(Some(&b));
    let on = Rc::new(on);
    let (s, r) = (s.clone(), row.clone());
    b.connect_clicked(move |_| {
        let dialog = gtk::FileDialog::builder().title("Choose a folder").modal(true).build();
        if let Some(p) = r.subtitle().map(|t| PathBuf::from(t.as_str())).filter(|p| p.is_dir()) {
            dialog.set_initial_folder(Some(&gio::File::for_path(p)));
        }
        let (r, on) = (r.clone(), on.clone());
        dialog.select_folder(Some(&s.window), None::<&gio::Cancellable>, move |res| {
            if let Some(p) = res.ok().and_then(|f| f.path()) {
                r.set_subtitle(&p.display().to_string());
                on(p);
            }
        });
    });
    row
}

// ===================== fonts =====================

/// Our config uses fontconfig patterns ("Roboto Condensed:style=Light" or "…:weight=50").
fn fc_to_pango(pattern: &str, size: f32) -> pango::FontDescription {
    let mut parts = pattern.split(':');
    let family = parts.next().unwrap_or("Sans");
    let mut style = String::new();
    for p in parts {
        if let Some(v) = p.strip_prefix("style=") {
            style = v.to_string();
        } else if let Some(v) = p.strip_prefix("weight=") {
            style = match v.parse::<i32>().unwrap_or(80) {
                0..=20 => "Thin",
                21..=45 => "Ultra-Light",
                46..=52 => "Light",
                53..=90 => "",
                91..=150 => "Medium",
                151..=190 => "Semi-Bold",
                191..=202 => "Bold",
                _ => "Heavy",
            }
            .to_string();
        } else if p == "slant=italic" {
            style.push_str(" Italic");
        }
    }
    // "Family, Style Size": the comma keeps multi-word families ("Roboto Condensed") intact.
    pango::FontDescription::from_string(&format!("{family}, {} {size}", style.trim()))
}

fn pango_to_fc(d: &pango::FontDescription) -> (String, f32) {
    let family = d.family().map(|f| f.to_string()).unwrap_or_else(|| "Sans".into());
    let w = match d.weight() {
        pango::Weight::Thin => 0,
        pango::Weight::Ultralight => 40,
        pango::Weight::Light => 50,
        pango::Weight::Semilight => 55,
        pango::Weight::Book => 75,
        pango::Weight::Medium => 100,
        pango::Weight::Semibold => 180,
        pango::Weight::Bold => 200,
        pango::Weight::Ultrabold => 205,
        pango::Weight::Heavy | pango::Weight::Ultraheavy => 210,
        _ => 80,
    };
    let slant = if d.style() == pango::Style::Italic { ":slant=italic" } else { "" };
    let width = match d.stretch() {
        pango::Stretch::UltraCondensed => ":width=50",
        pango::Stretch::ExtraCondensed => ":width=63",
        pango::Stretch::Condensed => ":width=75",
        pango::Stretch::SemiCondensed => ":width=87",
        pango::Stretch::SemiExpanded => ":width=113",
        pango::Stretch::Expanded => ":width=125",
        pango::Stretch::ExtraExpanded => ":width=150",
        pango::Stretch::UltraExpanded => ":width=200",
        _ => "",
    };
    let size = if d.size() > 0 { d.size() as f32 / pango::SCALE as f32 } else { 48.0 };
    (format!("{family}:weight={w}{slant}{width}"), size)
}

fn font_row(title: &str, pattern: &str, size: f32, on: impl Fn(String, f32) + 'static) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let b = gtk::FontDialogButton::new(Some(gtk::FontDialog::new()));
    b.set_valign(gtk::Align::Center);
    b.set_font_desc(&fc_to_pango(pattern, size));
    b.connect_font_desc_notify(move |b| {
        if let Some(d) = b.font_desc() {
            let (p, sz) = pango_to_fc(&d);
            on(p, sz);
        }
    });
    row.add_suffix(&b);
    row
}

// ===================== pages =====================

fn page_wallpaper(s: &S, cfg: &Config) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

    // --- now showing / quick actions
    let now = adw::PreferencesGroup::builder().title("Current wallpaper").build();
    let row = adw::ActionRow::builder().title("Actions").subtitle("Same as the tray and notification buttons").build();
    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (icon, tip, cmd) in [
        ("go-previous-symbolic", "Previous", "prev"),
        ("go-next-symbolic", "Next", "next"),
        ("starred-symbolic", "Add to favorites", "fav"),
        ("user-trash-symbolic", "Move to trash", "trash"),
        ("action-unavailable-symbolic", "Ban (never show again)", "ban"),
    ] {
        let b = icon_button(icon, tip);
        let s2 = s.clone();
        b.connect_clicked(move |_| cli(&s2, &[cmd], cmd == "fav" || cmd == "ban" || cmd == "trash"));
        bx.append(&b);
    }
    row.add_suffix(&bx);
    now.add(&row);
    page.add(&now);

    // --- changing
    let g = adw::PreferencesGroup::builder().title("Changing").build();
    let s2 = s.clone();
    g.add(&spin_row(
        "Change every",
        "Minutes between automatic changes. 0 = only when you ask.",
        0.0,
        1440.0,
        5.0,
        (cfg.change_interval / 60) as f64,
        move |v| edit(&s2, |c| c.change_interval = v as u64 * 60),
    ));
    let s2 = s.clone();
    g.add(&switch_row("Change at login", "Pick a new wallpaper when the service starts", cfg.change_on_start, move |v| {
        edit(&s2, |c| c.change_on_start = v)
    }));
    let s2 = s.clone();
    g.add(&spin_row(
        "Avoid repeats",
        "Don't show any of the last N wallpapers again",
        0.0,
        1000.0,
        10.0,
        cfg.avoid_recent as f64,
        move |v| edit(&s2, |c| c.avoid_recent = v as usize),
    ));
    page.add(&g);

    // --- monitors
    let g = adw::PreferencesGroup::builder().title("Monitors").build();
    let modes = [MonitorMode::Each, MonitorMode::Same, MonitorMode::Span];
    let s2 = s.clone();
    g.add(&combo_row(
        "Multiple monitors",
        "How images are placed when more than one screen is connected",
        &["Different image on each", "Same image on each", "One image across all"],
        modes.iter().position(|m| *m == cfg.monitors).unwrap_or(0) as u32,
        move |i| edit(&s2, |c| c.monitors = modes[i as usize]),
    ));
    page.add(&g);

    // --- clock
    page.add(&clock_group(s, &cfg.clock));

    // --- lock screen
    let g = adw::PreferencesGroup::builder()
        .title("Lock screen")
        .description("GNOME shows the desktop wallpaper, blurred, on the lock screen.")
        .build();
    let lock_modes = [LockMode::Same, LockMode::Separate, LockMode::Off];
    let s2 = s.clone();
    g.add(&combo_row(
        "Lock screen image",
        "",
        &["Same as desktop", "A different image while locked", "Don't touch"],
        lock_modes.iter().position(|m| *m == cfg.lockscreen.mode).unwrap_or(0) as u32,
        move |i| edit(&s2, |c| c.lockscreen.mode = lock_modes[i as usize]),
    ));
    let s2 = s.clone();
    g.add(&folder_row(s, "Lock images from", cfg.lockscreen.folder.clone(), "Favorites", move |p| {
        edit(&s2, |c| c.lockscreen.folder = Some(p))
    }));
    page.add(&g);
    page
}

fn clock_group(s: &S, clock: &ClockConfig) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title("Clock")
        .description("Time and date drawn onto the wallpaper of the main monitor")
        .build();
    let s2 = s.clone();
    g.add(&switch_row("Show clock", "", clock.enabled, move |v| edit(&s2, |c| c.clock.enabled = v)));
    let s2 = s.clone();
    g.add(&font_row("Time font", &clock.font, clock.size, move |p, sz| {
        edit(&s2, |c| {
            c.clock.font = p;
            c.clock.size = sz;
        })
    }));
    let s2 = s.clone();
    g.add(&font_row("Date font", &clock.date_font, clock.date_size, move |p, sz| {
        edit(&s2, |c| {
            c.clock.date_font = p;
            c.clock.date_size = sz;
        })
    }));
    let positions = ["bottom-right", "bottom-left", "top-right", "top-left"];
    let s2 = s.clone();
    g.add(&combo_row(
        "Position",
        "",
        &["Bottom right", "Bottom left", "Top right", "Top left"],
        positions.iter().position(|p| *p == clock.position).unwrap_or(0) as u32,
        move |i| edit(&s2, |c| c.clock.position = positions[i as usize].to_string()),
    ));
    let s2 = s.clone();
    g.add(&switch_row("Shadow", "Soft shadow behind the text for readability", clock.shadow, move |v| {
        edit(&s2, |c| c.clock.shadow = v)
    }));
    let s2 = s.clone();
    g.add(&entry_row("Time format (e.g. %H:%M or %I:%M %p)", &clock.time_format, move |t| {
        if !t.trim().is_empty() {
            edit(&s2, |c| c.clock.time_format = t)
        }
    }));
    let s2 = s.clone();
    g.add(&entry_row("Date format (e.g. %A, %B %d)", &clock.date_format, move |t| edit(&s2, |c| c.clock.date_format = t)));
    g
}

fn page_sources(s: &S) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

    // --- add
    let g = adw::PreferencesGroup::builder()
        .title("Add a source")
        .description("Type a topic and press Enter or the ✓ button. A few images are fetched right away.")
        .build();
    let s2 = s.clone();
    let wh = adw::EntryRow::builder().title("Wallhaven topic — e.g. space, #landscape, cyberpunk city").show_apply_button(true).build();
    wh.connect_apply(move |r| {
        let q = r.text().trim().to_string();
        if !q.is_empty() {
            add_source(&s2, wallhaven_topic(&s2, &q));
            r.set_text("");
        }
    });
    g.add(&wh);
    let s2 = s.clone();
    g.add(&combo_row(
        "New topics: minimum size",
        "",
        &MIN_RES.iter().map(|m| m.0).collect::<Vec<_>>(),
        s.new_min_res.get(),
        move |i| s2.new_min_res.set(i),
    ));
    let s2 = s.clone();
    g.add(&combo_row("New topics: order", "", &SORTS.iter().map(|m| m.0).collect::<Vec<_>>(), s.new_sort.get(), move |i| {
        s2.new_sort.set(i)
    }));

    let s2 = s.clone();
    let art = adw::EntryRow::builder().title("ArtStation artist — username or profile link").show_apply_button(true).build();
    art.connect_apply(move |r| {
        let u = r.text().trim().trim_end_matches('/').rsplit('/').next().unwrap_or("").trim_start_matches('@').to_string();
        if !u.is_empty() {
            add_source(&s2, Provider::Artstation { user: u });
            r.set_text("");
        }
    });
    g.add(&art);
    let s2 = s.clone();
    let uns = adw::EntryRow::builder().title("Unsplash topic — needs a free API key (Downloads page)").show_apply_button(true).build();
    uns.connect_apply(move |r| {
        let q = r.text().trim().to_string();
        if !q.is_empty() {
            add_source(&s2, Provider::Unsplash { query: q });
            r.set_text("");
        }
    });
    g.add(&uns);
    for (title, sub, p) in [
        ("Bing photo of the day", "A new landscape photo every day", Provider::Bing { market: "en-US".into() }),
        ("NASA Astronomy Picture of the Day", "Space images; many are smaller than big monitors", Provider::Apod),
    ] {
        let row = adw::ActionRow::builder().title(title).subtitle(sub).build();
        let b = button("Add", &["flat"]);
        let s2 = s.clone();
        b.connect_clicked(move |_| add_source(&s2, p.clone()));
        row.add_suffix(&b);
        g.add(&row);
    }
    let s2 = s.clone();
    let folder = folder_row(s, "Local folder", None, "Add a folder of your own images", move |p| {
        add_source(&s2, Provider::Folder { path: p })
    });
    g.add(&folder);
    page.add(&g);

    // --- suggestions
    let g = adw::PreferencesGroup::builder().title("Suggested topics").description("One click adds a Wallhaven topic").build();
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(6)
        .row_spacing(6)
        .homogeneous(false)
        .max_children_per_line(8)
        .build();
    for t in daemon::SUGGESTED_TOPICS {
        let b = button(t, &["pill"]);
        let (s2, t) = (s.clone(), t.to_string());
        b.connect_clicked(move |_| add_source(&s2, wallhaven_topic(&s2, &t)));
        flow.insert(&b, -1);
    }
    g.add(&flow);
    page.add(&g);

    page.add(&s.sources_group);
    rebuild_sources(s);
    page
}

fn wallhaven_topic(s: &S, q: &str) -> Provider {
    let (_, res) = MIN_RES[s.new_min_res.get() as usize];
    let (_, sort, top) = SORTS[s.new_sort.get() as usize];
    Provider::Wallhaven(Wallhaven {
        query: q.to_string(),
        min_resolution: res.map(str::to_string),
        sorting: sort.to_string(),
        top_range: top.map(str::to_string),
        ..Default::default()
    })
}

fn add_source(s: &S, p: Provider) {
    let label = p.label();
    edit(s, |c| match c.sources.iter_mut().find(|x| x.provider == p) {
        Some(x) => x.enabled = true,
        None => c.sources.push(Source { enabled: true, dir: None, provider: p }),
    });
    rebuild_sources(s);
    toast(s, &format!("Added {label}"));
    // After the debounced reload has gone out, fetch a first batch.
    let s2 = s.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || cli(&s2, &["download", "4"], false));
}

fn rebuild_sources(s: &S) {
    for w in s.source_rows.borrow_mut().drain(..) {
        s.sources_group.remove(&w);
    }
    let cfg = Config::load().unwrap_or_default();
    let mut rows = Vec::new();
    for (i, src) in cfg.sources.iter().enumerate() {
        let n = providers::scan_dir(&cfg.source_dir(src)).len();
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&src.provider.label()).as_str())
            .subtitle(if n == 1 { "1 image on disk".to_string() } else { format!("{n} images on disk") })
            .build();
        let sw = gtk::Switch::builder().active(src.enabled).valign(gtk::Align::Center).build();
        let s2 = s.clone();
        sw.connect_active_notify(move |sw| {
            let on = sw.is_active();
            edit(&s2, |c| {
                if let Some(x) = c.sources.get_mut(i) {
                    x.enabled = on;
                }
            });
        });
        row.add_suffix(&sw);
        row.set_activatable_widget(Some(&sw));
        if !matches!(src.provider, Provider::Favorites) {
            let del = icon_button("user-trash-symbolic", "Remove source (downloaded files are kept)");
            let s2 = s.clone();
            del.connect_clicked(move |_| {
                edit(&s2, |c| {
                    if i < c.sources.len() {
                        c.sources.remove(i);
                    }
                });
                rebuild_sources(&s2);
            });
            row.add_suffix(&del);
        }
        s.sources_group.add(&row);
        rows.push(row.upcast::<gtk::Widget>());
    }
    *s.source_rows.borrow_mut() = rows;
}

fn page_downloads(s: &S, cfg: &Config) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

    let g = adw::PreferencesGroup::builder().title("Downloading").build();
    let s2 = s.clone();
    g.add(&spin_row(
        "Download every",
        "Minutes between download rounds. 0 = never automatically.",
        0.0,
        1440.0,
        15.0,
        (cfg.download_interval / 60) as f64,
        move |v| edit(&s2, |c| c.download_interval = v as u64 * 60),
    ));
    let s2 = s.clone();
    g.add(&spin_row("Images per round", "", 1.0, 30.0, 1.0, cfg.download_batch as f64, move |v| {
        edit(&s2, |c| c.download_batch = v as usize)
    }));
    let s2 = s.clone();
    g.add(&spin_row(
        "Disk space limit (MB)",
        "Oldest downloads are deleted beyond this. Favorites are never deleted. 0 = no limit.",
        0.0,
        100_000.0,
        250.0,
        cfg.quota_mb as f64,
        move |v| edit(&s2, |c| c.quota_mb = v as u64),
    ));
    let row = adw::ActionRow::builder().title("Download now").subtitle("Fetch a few new images from enabled sources").build();
    let b = button("Download", &["flat"]);
    let s2 = s.clone();
    b.connect_clicked(move |_| cli(&s2, &["download"], true));
    row.add_suffix(&b);
    g.add(&row);
    page.add(&g);

    let g = adw::PreferencesGroup::builder().title("Folders").build();
    let s2 = s.clone();
    g.add(&folder_row(s, "Downloads", Some(cfg.download_folder.clone()), "", move |p| {
        edit(&s2, |c| c.download_folder = p)
    }));
    let s2 = s.clone();
    g.add(&folder_row(s, "Favorites", Some(cfg.favorites_folder.clone()), "", move |p| {
        edit(&s2, |c| c.favorites_folder = p)
    }));
    page.add(&g);

    let g = adw::PreferencesGroup::builder()
        .title("Image filters")
        .description("Images smaller than this are skipped (favorites are always allowed)")
        .build();
    let s2 = s.clone();
    g.add(&spin_row("Minimum width", "pixels", 0.0, 10_000.0, 10.0, cfg.min_width as f64, move |v| {
        edit(&s2, |c| c.min_width = v as u32)
    }));
    let s2 = s.clone();
    g.add(&spin_row("Minimum height", "pixels", 0.0, 10_000.0, 10.0, cfg.min_height as f64, move |v| {
        edit(&s2, |c| c.min_height = v as u32)
    }));
    let s2 = s.clone();
    g.add(&switch_row("Landscape only", "Skip portrait images", cfg.landscape_only, move |v| {
        edit(&s2, |c| c.landscape_only = v)
    }));
    page.add(&g);

    let g = adw::PreferencesGroup::builder()
        .title("API keys")
        .description("Optional. Stored in plain text in ~/.config/wallspin/config.toml.")
        .build();
    for (title, val, which) in [
        ("Wallhaven (for NSFW/sketchy content)", cfg.keys.wallhaven.clone(), 0),
        ("Unsplash (required for Unsplash topics)", cfg.keys.unsplash.clone(), 1),
        ("NASA (avoids the shared DEMO_KEY rate limit)", cfg.keys.nasa.clone(), 2),
    ] {
        let r = adw::PasswordEntryRow::builder().title(title).text(val.as_str()).build();
        let s2 = s.clone();
        r.connect_changed(move |r| {
            let v = r.text().trim().to_string();
            edit(&s2, |c| match which {
                0 => c.keys.wallhaven = v,
                1 => c.keys.unsplash = v,
                _ => c.keys.nasa = v,
            })
        });
        g.add(&r);
    }
    page.add(&g);
    page
}

fn page_controls(s: &S, cfg: &Config) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();

    // --- background service
    let g = adw::PreferencesGroup::builder()
        .title("Background service")
        .description("Wallspin changes wallpapers from a small background service (systemd user unit).")
        .build();
    let status = adw::ActionRow::builder().title("Status").build();
    let restart = button("Start", &["suggested-action"]);
    status.add_suffix(&restart);
    g.add(&status);
    let refresh: Rc<dyn Fn()> = {
        let (status, restart) = (status.clone(), restart.clone());
        Rc::new(move || {
            let running = sh_ok(&["systemctl", "--user", "is-active", "--quiet", "wallspin"]);
            status.set_subtitle(if running { "Running" } else { "Not running" });
            restart.set_label(if running { "Restart" } else { "Start" });
            if running {
                restart.remove_css_class("suggested-action");
            } else {
                restart.add_css_class("suggested-action");
            }
        })
    };
    refresh();
    let (s2, r2) = (s.clone(), refresh.clone());
    restart.connect_clicked(move |_| {
        let r3 = r2.clone();
        let argv = ["systemctl", "--user", "restart", "wallspin"].map(std::ffi::OsStr::new);
        run_proc(&s2, &argv, true, move |_| {
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || r3());
        });
    });
    let enabled = sh_ok(&["systemctl", "--user", "is-enabled", "--quiet", "wallspin"]);
    let s2 = s.clone();
    g.add(&switch_row("Start at login", "", enabled, move |on| {
        let argv = ["systemctl", "--user", if on { "enable" } else { "disable" }, "wallspin"].map(std::ffi::OsStr::new);
        run_proc(&s2, &argv, false, |_| {});
    }));
    page.add(&g);

    // --- feedback / tray
    let g = adw::PreferencesGroup::builder().title("Notifications and tray").build();
    let s2 = s.clone();
    g.add(&switch_row(
        "Notification on each change",
        "With Next, Favorite, Trash and Ban buttons — works on any GNOME",
        cfg.notify,
        move |v| edit(&s2, |c| c.notify = v),
    ));
    let s2 = s.clone();
    g.add(&switch_row(
        "Tray icon",
        "Needs a tray host (on GNOME: the AppIndicator extension). Takes effect after a service restart.",
        cfg.tray,
        move |v| edit(&s2, |c| c.tray = v),
    ));
    page.add(&g);

    // --- shortcuts
    let g = adw::PreferencesGroup::builder()
        .title("Keyboard shortcuts")
        .description("Global GNOME shortcuts that work everywhere")
        .build();
    for (_, name, accel) in gnome::SHORTCUTS {
        let short = name.trim_start_matches("Wallpaper: ");
        let title = short[..1].to_uppercase() + &short[1..];
        let row = adw::ActionRow::builder().title(title.as_str()).build();
        let label = gtk::ShortcutLabel::new(accel);
        label.set_valign(gtk::Align::Center);
        row.add_suffix(&label);
        g.add(&row);
    }
    let row = adw::ActionRow::builder().title("Install shortcuts").subtitle("Adds them to Settings → Keyboard → Custom Shortcuts").build();
    let add = button("Install", &["flat"]);
    let del = button("Remove", &["flat"]);
    let s2 = s.clone();
    add.connect_clicked(move |_| cli(&s2, &["shortcuts"], true));
    let s2 = s.clone();
    del.connect_clicked(move |_| cli(&s2, &["shortcuts", "--remove"], true));
    row.add_suffix(&del);
    row.add_suffix(&add);
    g.add(&row);
    page.add(&g);

    // --- files
    let g = adw::PreferencesGroup::builder().title("Files").build();
    let row = adw::ActionRow::builder()
        .title("Configuration file")
        .subtitle(crate::config::config_path().display().to_string())
        .build();
    let b = icon_button("document-edit-symbolic", "Open in text editor");
    b.connect_clicked(|_| {
        let f = gio::File::for_path(crate::config::config_path());
        let _ = gio::AppInfo::launch_default_for_uri(&f.uri(), None::<&gio::AppLaunchContext>);
    });
    row.add_suffix(&b);
    g.add(&row);
    page.add(&g);
    page
}
