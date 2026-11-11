//! The daemon: a single actor owns all mutable state; everything else
//! (D-Bus API, timers, tray, notifications, screensaver, downloader) talks to it
//! through bounded channels. No shared locks, no per-request threads.
use crate::config::{self, Config, LockMode, MonitorMode, Provider, Source, Wallhaven};
use crate::db::Db;
use crate::{clock, gnome, providers};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

pub const BUS_NAME: &str = "io.github.Wallspin.Daemon";
pub const OBJ_PATH: &str = "/io/github/Wallspin";
pub const IFACE: &str = "io.github.Wallspin1";

const HISTORY_MAX: usize = 300;
const POOL_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy)]
pub enum PromptKind {
    Wallhaven,
    Artstation,
    Unsplash,
}

#[derive(Debug)]
pub enum Cmd {
    Next,
    Prev,
    Fav,
    Ban,
    Trash,
    Status,
    Download(u32),
    Reload,
    Locked(bool),
    Downloaded(usize),
    ToggleSource(usize),
    AddSource(Provider),
    SetLock(LockMode),
    Prompt(PromptKind),
    SetMonitors(MonitorMode),
    ToggleClock,
    ClockTick,
    Rerender,
    Quit,
}

type Reply = oneshot::Sender<Result<String, String>>;

pub struct Msg {
    pub cmd: Cmd,
    pub reply: Option<Reply>,
}

pub type Tx = mpsc::Sender<Msg>;

fn send(tx: &Tx, cmd: Cmd) {
    // Fire-and-forget from sync contexts (tray callbacks, signals). Drop if the queue is full.
    let _ = tx.try_send(Msg { cmd, reply: None });
}

async fn ask(tx: &Tx, cmd: Cmd) -> Result<String, String> {
    let (rtx, rrx) = oneshot::channel();
    tx.send(Msg { cmd, reply: Some(rtx) }).await.map_err(|_| "daemon stopping".to_string())?;
    rrx.await.map_err(|_| "no reply".to_string())?
}

// ===================== entry =====================

pub async fn run() -> Result<()> {
    let cfg = Config::load()?;
    let db = Db::open(&config::db_path())?;
    let (tx, rx) = mpsc::channel::<Msg>(64);
    let (cfg_tx, cfg_rx) = watch::channel(cfg.clone());
    let (dl_tx, dl_rx) = mpsc::channel::<DlReq>(8);

    // D-Bus: claim our name (fails if another instance runs) and serve the API.
    let conn = zbus::connection::Builder::session()
        .name(BUS_NAME)
        .serve_at(OBJ_PATH, Api { tx: tx.clone() })
        .build()
        .await
        .context("D-Bus setup failed (is another instance running?)")?;

    let notifier = Notifier { conn: conn.clone(), id: Arc::new(AtomicU32::new(0)) };
    tokio::spawn(listen_notification_actions(conn.clone(), notifier.id.clone(), tx.clone()));
    tokio::spawn(listen_screensaver(conn.clone(), tx.clone()));
    tokio::spawn(listen_monitors(conn.clone(), tx.clone()));
    tokio::spawn(minute_ticks(tx.clone()));

    let tray = if cfg.tray { spawn_tray(tx.clone(), &cfg).await } else { None };

    let downloader = Downloader {
        cfg: cfg_rx.clone(),
        db: Db::open(&config::db_path())?,
        http: providers::client(),
        queues: HashMap::new(),
        last_page: HashMap::new(),
        rr: 0,
        actor: tx.clone(),
    };
    tokio::spawn(downloader.run(dl_rx));
    tokio::spawn(timer_loop(cfg_rx.clone(), tx.clone(), |c| c.change_interval, || Cmd::Next));
    tokio::spawn(timer_loop(cfg_rx.clone(), tx.clone(), |c| c.download_interval, || Cmd::Download(0)));

    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            let _ = ask(&tx, Cmd::Quit).await;
        });
    }

    let mut hist = db.recent_history(HISTORY_MAX)?;
    for f in &mut hist {
        f.retain(|p| p.exists());
    }
    hist.retain(|f| !f.is_empty());
    let pos = hist.len().saturating_sub(1);
    let mut actor = Actor {
        fonts: None,
        cfg,
        db,
        hist,
        pos,
        pool: Vec::new(),
        pool_at: None,
        dims: HashMap::new(),
        locked: false,
        base: None,
        gnome_set: None,
        conn,
        cfg_tx,
        dl_tx,
        notifier,
        tray,
        tx: tx.clone(),
    };
    actor.load_fonts();
    actor.startup().await;
    actor.run(rx).await;
    info!("bye");
    Ok(())
}

async fn timer_loop(mut cfg: watch::Receiver<Config>, tx: Tx, secs: fn(&Config) -> u64, cmd: fn() -> Cmd) {
    loop {
        let s = secs(&cfg.borrow_and_update());
        if s == 0 {
            // Disabled: wait for a config change.
            if cfg.changed().await.is_err() {
                return;
            }
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(s.max(30))) => send(&tx, cmd()),
            r = cfg.changed() => if r.is_err() { return },
        }
    }
}

/// Tick on every wall-clock minute boundary (the actor ignores it when the clock is off).
async fn minute_ticks(tx: Tx) {
    use chrono::Timelike;
    loop {
        let now = chrono::Local::now();
        let ms = 60_000u64.saturating_sub(now.second() as u64 * 1000 + now.timestamp_subsec_millis() as u64);
        tokio::time::sleep(Duration::from_millis(ms + 150)).await;
        send(&tx, Cmd::ClockTick);
    }
}

/// Re-render when monitors are plugged/unplugged or rearranged.
async fn listen_monitors(conn: zbus::Connection, tx: Tx) {
    let Ok(rule) = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.Mutter.DisplayConfig")
        .member("MonitorsChanged")
        .build()
    else {
        return;
    };
    let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else { return };
    while stream.next().await.is_some() {
        // Mutter emits bursts while reconfiguring; settle first.
        tokio::time::sleep(Duration::from_secs(2)).await;
        while tokio::time::timeout(Duration::from_millis(300), stream.next()).await.is_ok() {}
        info!("monitors changed");
        send(&tx, Cmd::Rerender);
    }
}

// ===================== actor =====================

/// One image per monitor, primary first.
type Frame = Vec<PathBuf>;

struct Actor {
    cfg: Config,
    db: Db,
    /// Navigation history (oldest first) and the index of what's on screen.
    hist: Vec<Frame>,
    pos: usize,
    pool: Vec<PathBuf>,
    pool_at: Option<Instant>,
    dims: HashMap<PathBuf, (u32, u32)>,
    locked: bool,
    fonts: Option<clock::Fonts>,
    /// Composed canvas of what's on screen, kept (compressed) only while the clock is on.
    base: Option<Arc<clock::Base>>,
    /// Last (uri, picture-options) written to GNOME; avoid redundant gsettings writes.
    gnome_set: Option<(PathBuf, String)>,
    conn: zbus::Connection,
    cfg_tx: watch::Sender<Config>,
    dl_tx: mpsc::Sender<DlReq>,
    notifier: Notifier,
    tray: Option<ksni::Handle<Tray>>,
    tx: Tx,
}

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Desktop,
    Lock,
}

impl Actor {
    fn load_fonts(&mut self) {
        self.fonts = None;
        if self.cfg.clock.enabled {
            match clock::Fonts::load(&self.cfg.clock) {
                Ok(f) => self.fonts = Some(f),
                Err(e) => warn!("clock disabled: {e:#}"),
            }
        }
    }

    fn clock_on(&self) -> bool {
        self.cfg.clock.enabled && self.fonts.is_some()
    }

    async fn startup(&mut self) {
        self.refresh_pool(true);
        info!("pool: {} images, {} in history", self.pool.len(), self.hist.len());
        let stale_lock = self.db.get_state("lock_swapped").ok().flatten().is_some();
        if self.cfg.change_on_start {
            if let Err(e) = self.next().await {
                warn!("initial change: {e:#}");
            }
        } else {
            // Re-render what was showing: points GNOME at our single file, restores after a
            // crash mid-lock, refreshes the clock and fills other monitors.
            let _ = stale_lock;
            if let Err(e) = self.rerender().await {
                warn!("restore: {e:#}");
            }
        }
        let _ = self.db.set_state("lock_swapped", None);
        if self.cfg.download_interval > 0 && self.remote_count() < 30 {
            let _ = self.dl_tx.try_send(DlReq { n: self.cfg.download_batch.max(1) as u32, only: None, reply: None });
        }
        self.update_tray().await;
    }

    async fn run(&mut self, mut rx: mpsc::Receiver<Msg>) {
        while let Some(Msg { cmd, reply }) = rx.recv().await {
            let quit = matches!(cmd, Cmd::Quit);
            let res = self.handle(cmd).await.map_err(|e| format!("{e:#}"));
            if let Err(e) = &res {
                warn!("{e}");
            }
            if let Some(r) = reply {
                let _ = r.send(res);
            }
            if quit {
                break;
            }
        }
    }

    async fn handle(&mut self, cmd: Cmd) -> Result<String> {
        match cmd {
            Cmd::Next => self.next().await,
            Cmd::Prev => self.prev().await,
            Cmd::Fav => self.favorite().await,
            Cmd::Ban => self.ban().await,
            Cmd::Trash => self.trash().await,
            Cmd::Status => Ok(self.status()),
            Cmd::Download(n) => {
                let n = if n == 0 { self.cfg.download_batch.max(1) as u32 } else { n };
                self.dl_tx.try_send(DlReq { n, only: None, reply: None }).ok();
                Ok(format!("downloading up to {n} image(s)"))
            }
            Cmd::Downloaded(n) => {
                if n > 0 {
                    self.pool_at = None;
                }
                Ok(String::new())
            }
            Cmd::Reload => {
                let old = self.cfg.clone();
                self.cfg = Config::load()?;
                self.cfg_tx.send_replace(self.cfg.clone());
                self.pool_at = None;
                if old.clock != self.cfg.clock {
                    self.load_fonts();
                }
                if old.clock != self.cfg.clock || old.monitors != self.cfg.monitors || old.display_mode != self.cfg.display_mode
                {
                    self.rerender().await?;
                }
                self.update_tray().await;
                Ok("config reloaded".into())
            }
            Cmd::Locked(on) => self.on_lock(on).await,
            Cmd::ClockTick => {
                if self.clock_on() && !self.locked {
                    self.clock_tick().await?;
                }
                Ok(String::new())
            }
            Cmd::Rerender => {
                if !self.locked {
                    self.rerender().await?;
                }
                Ok(String::new())
            }
            Cmd::SetMonitors(m) => {
                self.cfg.monitors = m;
                self.save_cfg().await?;
                self.rerender().await?;
                Ok(format!("monitors: {m:?}"))
            }
            Cmd::ToggleClock => {
                self.cfg.clock.enabled = !self.cfg.clock.enabled;
                self.load_fonts();
                self.save_cfg().await?;
                self.rerender().await?;
                Ok(format!("clock {}", if self.cfg.clock.enabled { "on" } else { "off" }))
            }
            Cmd::ToggleSource(i) => {
                let s = self.cfg.sources.get_mut(i).context("no such source")?;
                s.enabled = !s.enabled;
                let msg = format!("{} {}", if s.enabled { "enabled" } else { "disabled" }, s.provider.label());
                let fetch = s.enabled && s.provider.is_remote();
                let dir = self.cfg.source_dir(&self.cfg.sources[i]);
                self.save_cfg().await?;
                if fetch && providers::scan_dir(&dir).is_empty() {
                    self.dl_tx.try_send(DlReq { n: 3, only: Some(dir), reply: None }).ok();
                }
                Ok(msg)
            }
            Cmd::AddSource(p) => {
                let label = p.label();
                let idx = match self.cfg.sources.iter().position(|s| s.provider == p) {
                    Some(i) => {
                        self.cfg.sources[i].enabled = true;
                        i
                    }
                    None => {
                        self.cfg.sources.push(Source { enabled: true, dir: None, provider: p });
                        self.cfg.sources.len() - 1
                    }
                };
                let dir = self.cfg.source_dir(&self.cfg.sources[idx]);
                self.save_cfg().await?;
                self.dl_tx.try_send(DlReq { n: 4, only: Some(dir), reply: None }).ok();
                self.notifier.plain(&format!("Added source: {label}"), "Fetching a few images now…").await;
                Ok(format!("added {label}"))
            }
            Cmd::SetLock(m) => {
                self.cfg.lockscreen.mode = m;
                self.save_cfg().await?;
                Ok(format!("lock screen mode: {m:?}"))
            }
            Cmd::Prompt(kind) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    if let Some(p) = prompt_source(kind).await {
                        send(&tx, Cmd::AddSource(p));
                    }
                });
                Ok(String::new())
            }
            Cmd::Quit => {
                if self.locked && self.db.get_state("lock_swapped")?.is_some() {
                    self.rerender().await?;
                    self.db.set_state("lock_swapped", None)?;
                }
                Ok("stopping".into())
            }
        }
    }

    async fn save_cfg(&mut self) -> Result<()> {
        self.cfg.save()?;
        self.cfg_tx.send_replace(self.cfg.clone());
        self.pool_at = None;
        self.update_tray().await;
        Ok(())
    }

    /// Primary monitor's image.
    fn current(&self) -> Option<PathBuf> {
        self.hist.get(self.pos).and_then(|f| f.first().cloned())
    }

    fn remote_count(&self) -> usize {
        self.pool.iter().filter(|p| p.starts_with(&self.cfg.download_folder)).count()
    }

    // ---------- pool ----------

    fn refresh_pool(&mut self, force: bool) {
        if !force && self.pool_at.is_some_and(|t| t.elapsed() < POOL_TTL) {
            return;
        }
        let mut seen = HashSet::new();
        let mut pool = Vec::new();
        let dirs: Vec<(PathBuf, bool)> = self
            .cfg
            .sources
            .iter()
            .filter(|s| s.enabled)
            .map(|s| (self.cfg.source_dir(s), !matches!(s.provider, Provider::Favorites)))
            .collect();
        for (dir, filter) in dirs {
            for p in providers::scan_dir(&dir) {
                if !seen.insert(p.clone()) {
                    continue;
                }
                let Some(d) = self.dims_of(&p) else { continue };
                if d.0 as u64 * d.1 as u64 > clock::MAX_PIXELS {
                    continue;
                }
                if filter && !providers::dims_ok(&self.cfg, d.0, d.1) {
                    continue;
                }
                if self.is_banned(&p) {
                    continue;
                }
                pool.push(p);
            }
        }
        // Bound the dims cache to files that still exist in some source.
        self.dims.retain(|k, _| seen.contains(k));
        self.pool = pool;
        self.pool_at = Some(Instant::now());
    }

    fn dims_of(&mut self, p: &Path) -> Option<(u32, u32)> {
        if let Some(d) = self.dims.get(p) {
            return Some(*d);
        }
        let sz = imagesize::size(p).ok()?;
        let d = (sz.width as u32, sz.height as u32);
        self.dims.insert(p.to_path_buf(), d);
        Some(d)
    }

    fn is_banned(&self, p: &Path) -> bool {
        self.db.is_banned(&p.to_string_lossy()).unwrap_or(false)
            || self.db.origin(p).ok().flatten().is_some_and(|o| self.db.is_banned(&o).unwrap_or(false))
    }

    /// Random image, avoiding recent ones and `exclude`; when `aspect` is given, prefer images
    /// within ~20% of it so they crop well onto that monitor.
    fn pick(&mut self, exclude: &[PathBuf], aspect: Option<f64>) -> Option<PathBuf> {
        self.refresh_pool(false);
        let recent: HashSet<&PathBuf> =
            self.hist.iter().rev().take(self.cfg.avoid_recent).flatten().chain(exclude.iter()).collect();
        let mut cands: Vec<PathBuf> = self.pool.iter().filter(|p| !recent.contains(p) && p.exists()).cloned().collect();
        if cands.is_empty() {
            let cur = self.current();
            cands = self.pool.iter().filter(|p| Some(*p) != cur.as_ref() && !exclude.contains(p) && p.exists()).cloned().collect();
        }
        if let Some(a) = aspect {
            let fitting: Vec<PathBuf> = cands
                .iter()
                .filter(|p| self.dims.get(*p).is_some_and(|d| ((d.0 as f64 / d.1 as f64) / a - 1.0).abs() < 0.2))
                .cloned()
                .collect();
            if !fitting.is_empty() {
                cands = fitting;
            }
        }
        if cands.is_empty() {
            return None;
        }
        Some(cands.swap_remove(rand::random_range(0..cands.len())))
    }

    /// Choose a fresh frame for the current monitor setup.
    async fn pick_frame(&mut self) -> Option<Frame> {
        let aspects: Vec<Option<f64>> = match self.cfg.monitors {
            MonitorMode::Each => match clock::detect_screens(&self.conn).await {
                Ok(s) => s.rects.iter().map(|r| Some(r.w as f64 / r.h as f64)).collect(),
                Err(_) => vec![None],
            },
            MonitorMode::Same => vec![clock::detect_screens(&self.conn).await.ok().map(|s| s.rects[0].w as f64 / s.rects[0].h as f64)],
            MonitorMode::Span => vec![clock::detect_screens(&self.conn).await.ok().map(|s| s.canvas.0 as f64 / s.canvas.1 as f64)],
        };
        let mut frame = Vec::new();
        for a in aspects {
            if let Some(p) = self.pick(&frame, a) {
                frame.push(p);
            }
        }
        (!frame.is_empty()).then_some(frame)
    }

    // ---------- commands ----------

    async fn next(&mut self) -> Result<String> {
        anyhow::ensure!(!self.locked, "screen is locked");
        if self.pos + 1 < self.hist.len() {
            self.pos += 1;
            self.show(false).await?;
            return Ok(self.describe());
        }
        let frame = self.pick_frame().await.context("no images available — add a source or run `wallspin download`")?;
        self.hist.push(frame);
        if self.hist.len() > HISTORY_MAX {
            let cut = self.hist.len() - HISTORY_MAX;
            self.hist.drain(..cut);
        }
        self.pos = self.hist.len() - 1;
        self.show(true).await?;
        if self.remote_count() < 10 && self.cfg.sources.iter().any(|s| s.enabled && s.provider.is_remote()) {
            self.dl_tx.try_send(DlReq { n: self.cfg.download_batch.max(1) as u32, only: None, reply: None }).ok();
        }
        Ok(self.describe())
    }

    async fn prev(&mut self) -> Result<String> {
        anyhow::ensure!(!self.locked, "screen is locked");
        anyhow::ensure!(self.pos > 0, "no earlier wallpaper");
        self.pos -= 1;
        self.show(false).await?;
        Ok(self.describe())
    }

    fn describe(&self) -> String {
        self.hist.get(self.pos).map(|f| f.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }

    async fn favorite(&mut self) -> Result<String> {
        let cur = self.current().context("nothing shown yet")?;
        if cur.starts_with(&self.cfg.favorites_folder) {
            return Ok("already a favorite".into());
        }
        std::fs::create_dir_all(&self.cfg.favorites_folder)?;
        let dest = self.cfg.favorites_folder.join(cur.file_name().context("bad name")?);
        std::fs::copy(&cur, &dest)?;
        if let Some(o) = self.db.origin(&cur)? {
            self.db.set_origin(&dest, &o)?;
        }
        self.pool_at = None;
        self.notifier.plain("♥ Added to favorites", &file_name(&cur)).await;
        Ok(format!("saved {}", dest.display()))
    }

    /// Never show the primary image again (and never re-download it), then move on.
    async fn ban(&mut self) -> Result<String> {
        let cur = self.current().context("nothing shown yet")?;
        self.db.ban(&cur.to_string_lossy())?;
        if let Some(o) = self.db.origin(&cur)? {
            self.db.ban(&o)?;
        }
        if cur.starts_with(&self.cfg.download_folder) {
            let _ = std::fs::remove_file(&cur);
            self.db.forget_path(&cur)?;
        }
        self.drop_image(&cur);
        self.next().await.map(|n| format!("banned; now {n}"))
    }

    /// Move the primary image to the desktop trash (recoverable), then move on.
    async fn trash(&mut self) -> Result<String> {
        let cur = self.current().context("nothing shown yet")?;
        let st = tokio::process::Command::new("gio").arg("trash").arg(&cur).status().await?;
        anyhow::ensure!(st.success(), "gio trash failed");
        self.drop_image(&cur);
        self.next().await.map(|n| format!("trashed; now {n}"))
    }

    /// Forget every frame that used `img` and step back so `next` picks fresh.
    fn drop_image(&mut self, img: &Path) {
        let before = self.hist.len();
        self.hist.retain(|f| !f.iter().any(|p| p == img));
        let removed = before - self.hist.len();
        self.pos = self.pos.saturating_sub(removed.min(1)).min(self.hist.len().saturating_sub(1));
        // Jump to the end so `next` picks a new frame rather than replaying history.
        self.pos = self.hist.len().saturating_sub(1);
        self.pool.retain(|p| p != img);
        self.pool_at = None;
    }

    fn status(&mut self) -> String {
        self.refresh_pool(false);
        let mut s = format!(
            "current:  {}\nhistory:  {}/{}\npool:     {} images\nmonitors: {:?}\nclock:    {}\nlocked:   {}\nlock mode: {:?}\nsources:\n",
            self.describe().replace('\n', "\n          "),
            self.pos + 1,
            self.hist.len(),
            self.pool.len(),
            self.cfg.monitors,
            if self.clock_on() { "on" } else { "off" },
            self.locked,
            self.cfg.lockscreen.mode,
        );
        for (i, src) in self.cfg.sources.iter().enumerate() {
            let n = providers::scan_dir(&self.cfg.source_dir(src)).len();
            s.push_str(&format!(
                "  [{}] {} {}  ({n} local)\n",
                i + 1,
                if src.enabled { "✓" } else { " " },
                src.provider.label()
            ));
        }
        s
    }

    // ---------- applying ----------

    /// Put `hist[pos]` on screen.
    async fn show(&mut self, record: bool) -> Result<()> {
        self.fill_frame().await;
        let frame = self.hist[self.pos].clone();
        self.apply(&frame, Slot::Desktop).await?;
        if record {
            self.db.push_history(&frame)?;
        }
        info!("wallpaper: {}", self.describe().replace('\n', " | "));
        if self.cfg.notify {
            let src = self.source_label_for(&frame[0]);
            self.notifier.wallpaper(&frame[0], &src, frame.len() - 1).await;
        }
        self.update_tray().await;
        Ok(())
    }

    /// In `each` mode, top up a frame from history that has fewer images than monitors
    /// (older entry, or a monitor was plugged in).
    async fn fill_frame(&mut self) {
        if self.cfg.monitors != MonitorMode::Each || self.hist.is_empty() {
            return;
        }
        let Ok(screens) = clock::detect_screens(&self.conn).await else { return };
        let mut frame = self.hist[self.pos].clone();
        for r in screens.rects.iter().skip(frame.len()) {
            if let Some(p) = self.pick(&frame, Some(r.w as f64 / r.h as f64)) {
                frame.push(p);
            }
        }
        self.hist[self.pos] = frame;
    }

    /// Re-apply what's on screen (monitor/clock/mode change, startup, unlock).
    async fn rerender(&mut self) -> Result<()> {
        if self.hist.is_empty() {
            return Ok(());
        }
        self.fill_frame().await;
        let frame = self.hist[self.pos].clone();
        self.apply(&frame, Slot::Desktop).await
    }

    /// Render `frame` into the single wallpaper file and point GNOME at it.
    ///
    /// GNOME Shell caches one GPU texture per wallpaper URI and never evicts it (only a
    /// file-change event on the displayed file purges it). So we never hand GNOME a new
    /// URI: every change, clock tick and lock image is rewritten *in place* into the same
    /// screen-sized file, and GNOME's file monitor swaps the texture.
    async fn apply(&mut self, frame: &[PathBuf], slot: Slot) -> Result<()> {
        let clock_on = slot == Slot::Desktop && self.clock_on();
        let mode = self.cfg.monitors;
        let screens = clock::detect_screens(&self.conn).await.unwrap_or_else(|e| {
            warn!("monitor layout unavailable ({e:#}); assuming 1920x1080");
            clock::Screens { canvas: (1920, 1080), rects: vec![clock::Rect { x: 0, y: 0, w: 1920, h: 1080 }] }
        });
        let primary = screens.rects[0];
        let (canvas, rects, layout_primary) = match mode {
            MonitorMode::Each => (screens.canvas, screens.rects.clone(), primary),
            MonitorMode::Span => {
                (screens.canvas, vec![clock::Rect { x: 0, y: 0, w: screens.canvas.0, h: screens.canvas.1 }], primary)
            }
            MonitorMode::Same => {
                let r = clock::Rect { x: 0, y: 0, w: primary.w, h: primary.h };
                ((primary.w, primary.h), vec![r], r)
            }
        };
        let layout = clock::Layout { canvas, primary: layout_primary };
        let images = frame.to_vec();
        let fonts = if clock_on { self.fonts.clone() } else { None };
        let ccfg = self.cfg.clock.clone();
        let out = self.out_path()?;
        let out2 = out.clone();
        let base = tokio::task::spawn_blocking(move || -> Result<clock::Base> {
            let (base, raw) = clock::compose(&images, canvas, &rects, layout)?;
            clock::render_to(raw, layout, fonts.as_ref().map(|f| (f, &ccfg)), &out2)?;
            Ok(base)
        })
        .await??;
        // Keep the compressed canvas only while it's needed for minute redraws.
        self.base = (slot == Slot::Desktop && clock_on).then(|| Arc::new(base));
        crate::trim_heap();
        let options = if mode == MonitorMode::Same { "zoom" } else { "spanned" };
        self.set_gnome(&out, options).await
    }

    async fn set_gnome(&mut self, target: &Path, options: &str) -> Result<()> {
        let want = (target.to_path_buf(), options.to_string());
        if self.gnome_set.as_ref() == Some(&want) {
            return Ok(()); // same URI: the in-place rewrite already triggered GNOME's reload
        }
        gnome::set_desktop(target, options).await?;
        if self.cfg.lockscreen.mode != LockMode::Off {
            gnome::set_screensaver(target, options).await?;
        }
        self.gnome_set = Some(want);
        Ok(())
    }

    /// Redraw only the clock on the kept base.
    async fn clock_tick(&mut self) -> Result<()> {
        let (Some(base), Some(fonts)) = (self.base.clone(), self.fonts.clone()) else { return Ok(()) };
        let ccfg = self.cfg.clock.clone();
        let out = self.out_path()?;
        let out2 = out.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let img = base.decode()?;
            clock::render_to(img, base.layout, Some((&fonts, &ccfg)), &out2)
        })
        .await??;
        crate::trim_heap();
        let options = if self.cfg.monitors == MonitorMode::Same { "zoom" } else { "spanned" };
        self.set_gnome(&out, options).await
    }

    /// Where the wallpaper file lives: copy_to if set (shared/system-readable), else the cache dir.
    fn render_dir(&self) -> PathBuf {
        self.cfg.copy_to.clone().unwrap_or_else(|| dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join(config::APP))
    }

    fn out_path(&self) -> Result<PathBuf> {
        let dir = self.render_dir();
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join("wallspin-wallpaper.jpg"))
    }

    async fn on_lock(&mut self, on: bool) -> Result<String> {
        if on == self.locked {
            return Ok(String::new());
        }
        self.locked = on;
        if self.cfg.lockscreen.mode != LockMode::Separate {
            if !on && self.clock_on() {
                self.clock_tick().await?; // time moved on while locked
            }
            return Ok(String::new());
        }
        if on {
            let cur: Vec<PathBuf> = self.hist.get(self.pos).cloned().unwrap_or_default();
            let from_dir = self.cfg.lockscreen.folder.clone().unwrap_or_else(|| self.cfg.favorites_folder.clone());
            let mut cands: Vec<PathBuf> = providers::scan_dir(&from_dir).into_iter().filter(|p| !cur.contains(p)).collect();
            let img = if cands.is_empty() { self.pick(&cur, None) } else { Some(cands.swap_remove(rand::random_range(0..cands.len()))) };
            if let Some(img) = img {
                self.db.set_state("lock_swapped", Some(&img.to_string_lossy()))?;
                self.apply(&[img.clone()], Slot::Lock).await?;
                info!("lock screen: {}", img.display());
            }
        } else if self.db.get_state("lock_swapped")?.is_some() {
            self.rerender().await?;
            self.db.set_state("lock_swapped", None)?;
        } else if self.clock_on() {
            self.clock_tick().await?;
        }
        Ok(String::new())
    }

    fn source_label_for(&self, p: &Path) -> String {
        self.cfg
            .sources
            .iter()
            .find(|s| p.starts_with(self.cfg.source_dir(s)))
            .map(|s| s.provider.label())
            .unwrap_or_default()
    }

    async fn update_tray(&self) {
        let Some(h) = &self.tray else { return };
        let cur = self.current().map(|p| file_name(&p)).unwrap_or_default();
        let sources: Vec<(String, bool)> = self.cfg.sources.iter().map(|s| (s.provider.label(), s.enabled)).collect();
        let lock = self.cfg.lockscreen.mode;
        let monitors = self.cfg.monitors;
        let clock = self.cfg.clock.enabled;
        h.update(move |t| {
            t.current = cur;
            t.sources = sources;
            t.lock = lock;
            t.monitors = monitors;
            t.clock = clock;
        })
        .await;
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ===================== downloader =====================

struct DlReq {
    n: u32,
    /// Restrict to the source whose folder is this.
    only: Option<PathBuf>,
    reply: Option<Reply>,
}

struct Downloader {
    cfg: watch::Receiver<Config>,
    db: Db,
    http: reqwest::Client,
    /// Candidate queues per source folder; refilled lazily from the provider.
    queues: HashMap<PathBuf, VecDeque<providers::Item>>,
    last_page: HashMap<PathBuf, u32>,
    rr: usize,
    actor: Tx,
}

impl Downloader {
    async fn run(mut self, mut rx: mpsc::Receiver<DlReq>) {
        while let Some(req) = rx.recv().await {
            let cfg = self.cfg.borrow().clone();
            // Drop queues for sources that no longer exist.
            let live: HashSet<PathBuf> = cfg.sources.iter().map(|s| cfg.source_dir(s)).collect();
            self.queues.retain(|k, _| live.contains(k));
            let got = self.round(&cfg, req.n, req.only.as_deref()).await;
            let keep: Vec<PathBuf> = self.db.last_frame().unwrap_or_default();
            match providers::enforce_quota(&cfg, &keep) {
                Ok(n) if n > 0 => info!("quota: pruned {n} old downloads"),
                Err(e) => warn!("quota: {e:#}"),
                _ => {}
            }
            info!("downloaded {got} image(s)");
            send(&self.actor, Cmd::Downloaded(got));
            if let Some(r) = req.reply {
                let _ = r.send(Ok(format!("downloaded {got}")));
            }
        }
    }

    async fn round(&mut self, cfg: &Config, n: u32, only: Option<&Path>) -> usize {
        let sources: Vec<&Source> = cfg
            .sources
            .iter()
            .filter(|s| s.enabled && s.provider.is_remote())
            .filter(|s| only.is_none_or(|o| cfg.source_dir(s) == o))
            .collect();
        if sources.is_empty() {
            return 0;
        }
        let mut got = 0usize;
        let mut failures = 0usize;
        while got < n as usize && failures < sources.len() * 2 {
            let s = sources[self.rr % sources.len()];
            self.rr = self.rr.wrapping_add(1);
            match self.one(cfg, s).await {
                Ok(true) => got += 1,
                Ok(false) => failures += 1,
                Err(e) => {
                    warn!("{}: {e:#}", s.provider.label());
                    failures += 1;
                }
            }
        }
        got
    }

    /// Download one new image from `s`. Ok(false) = provider had nothing new.
    async fn one(&mut self, cfg: &Config, s: &Source) -> Result<bool> {
        let dir = cfg.source_dir(s);
        let mut refilled = false;
        loop {
            let item = self.queues.get_mut(&dir).and_then(|q| q.pop_front());
            let Some(item) = item else {
                if refilled {
                    return Ok(false);
                }
                refilled = true;
                self.refill(cfg, s, &dir).await?;
                continue;
            };
            if self.db.is_banned(&item.origin)? || dir.join(&item.filename).exists() {
                continue;
            }
            if let (Some(w), Some(h)) = (item.width, item.height)
                && !providers::dims_ok(cfg, w, h) {
                    continue;
                }
            let path = match providers::download(&self.http, &item, &dir).await {
                Ok(p) => p,
                Err(e) => {
                    warn!("download {}: {e:#}", item.url);
                    continue;
                }
            };
            // Providers without dimensions in their API: check the header now.
            if item.width.is_none() {
                let ok = imagesize::size(&path).is_ok_and(|d| providers::dims_ok(cfg, d.width as u32, d.height as u32));
                if !ok {
                    info!("skip {} (below min size / not landscape)", item.filename);
                    let _ = std::fs::remove_file(&path);
                    self.db.ban(&item.origin)?; // don't fetch it again
                    continue;
                }
            }
            self.db.set_origin(&path, &item.origin)?;
            info!("+ {}", path.display());
            return Ok(true);
        }
    }

    async fn refill(&mut self, cfg: &Config, s: &Source, dir: &Path) -> Result<()> {
        let page = match &s.provider {
            Provider::Wallhaven(w) if w.sorting != "random" => {
                let last = *self.last_page.get(dir).unwrap_or(&1);
                rand::random_range(1..=last.clamp(1, 10))
            }
            _ => 1,
        };
        let (mut items, last) = providers::fetch(&self.http, &s.provider, &cfg.keys, page).await?;
        if let Some(l) = last {
            self.last_page.insert(dir.to_path_buf(), l);
        }
        // Fisher–Yates so we don't always take the top results.
        for i in (1..items.len()).rev() {
            items.swap(i, rand::random_range(0..=i));
        }
        self.queues.insert(dir.to_path_buf(), items.into());
        Ok(())
    }
}

// ===================== D-Bus API =====================

struct Api {
    tx: Tx,
}

impl Api {
    async fn call(&self, cmd: Cmd) -> zbus::fdo::Result<String> {
        ask(&self.tx, cmd).await.map_err(zbus::fdo::Error::Failed)
    }
}

#[zbus::interface(name = "io.github.Wallspin1")]
impl Api {
    async fn next(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Next).await
    }
    async fn prev(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Prev).await
    }
    async fn favorite(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Fav).await
    }
    async fn ban(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Ban).await
    }
    async fn trash(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Trash).await
    }
    async fn status(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Status).await
    }
    async fn download(&self, n: u32) -> zbus::fdo::Result<String> {
        self.call(Cmd::Download(n)).await
    }
    async fn reload(&self) -> zbus::fdo::Result<String> {
        self.call(Cmd::Reload).await
    }
    async fn quit(&self) -> zbus::fdo::Result<String> {
        let r = self.call(Cmd::Quit).await;
        // Let the reply go out before the process exits.
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(0);
        });
        r
    }
}

// ===================== screensaver =====================

async fn listen_screensaver(conn: zbus::Connection, tx: Tx) {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.gnome.ScreenSaver")
        .member("ActiveChanged")
        .build();
    let rule = match rule {
        Ok(r) => r,
        Err(e) => return warn!("screensaver rule: {e}"),
    };
    let mut stream = match zbus::MessageStream::for_match_rule(rule, &conn, None).await {
        Ok(s) => s,
        Err(e) => return warn!("screensaver listener: {e}"),
    };
    while let Some(Ok(msg)) = stream.next().await {
        if let Ok(active) = msg.body().deserialize::<bool>() {
            info!("screen {}", if active { "locked" } else { "unlocked" });
            send(&tx, Cmd::Locked(active));
        }
    }
}

// ===================== notifications =====================

struct Notifier {
    conn: zbus::Connection,
    /// Id of our last notification so we replace instead of stacking.
    id: Arc<AtomicU32>,
}

impl Notifier {
    async fn wallpaper(&self, p: &Path, source: &str, others: usize) {
        let actions = ["next", "Next", "fav", "♥ Favorite", "trash", "Trash", "ban", "Ban"];
        let mut body = if source.is_empty() { file_name(p) } else { format!("{source}\n{}", file_name(p)) };
        if others > 0 {
            body.push_str(&format!("\n+{others} on other monitor(s) · buttons act on the main one"));
        }
        self.notify("Wallpaper changed", &body, &actions, Some(p)).await;
    }

    async fn plain(&self, summary: &str, body: &str) {
        self.notify(summary, body, &[], None).await;
    }

    async fn notify(&self, summary: &str, body: &str, actions: &[&str], image: Option<&Path>) {
        let mut hints: HashMap<&str, zbus::Variant> = HashMap::new();
        hints.insert("transient", zbus::Variant::from(true));
        hints.insert("desktop-entry", zbus::Variant::from(crate::assets::APP_ID));
        if let Some(p) = image {
            hints.insert("image-path", zbus::Variant::from(gnome::file_uri(p)));
        }
        let replaces = self.id.load(Ordering::Relaxed);
        let r = self
            .conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &("Wallspin", replaces, crate::assets::APP_ID, summary, body, actions, hints, 6000i32),
            )
            .await;
        match r.and_then(|m| m.body().deserialize::<u32>()) {
            Ok(id) => self.id.store(id, Ordering::Relaxed),
            Err(e) => warn!("notify: {e}"),
        }
    }
}

async fn listen_notification_actions(conn: zbus::Connection, id: Arc<AtomicU32>, tx: Tx) {
    let Ok(rule) = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.Notifications")
        .member("ActionInvoked")
        .build()
    else {
        return;
    };
    let Ok(mut stream) = zbus::MessageStream::for_match_rule(rule, &conn, None).await else { return };
    while let Some(Ok(msg)) = stream.next().await {
        let Ok((nid, action)) = msg.body().deserialize::<(u32, String)>() else { continue };
        if nid != id.load(Ordering::Relaxed) {
            continue;
        }
        let cmd = match action.as_str() {
            "next" => Cmd::Next,
            "fav" => Cmd::Fav,
            "trash" => Cmd::Trash,
            "ban" => Cmd::Ban,
            _ => continue,
        };
        send(&tx, cmd);
    }
}

// ===================== "add source" dialogs =====================

async fn zenity_entry(title: &str, text: &str) -> Option<String> {
    let out = tokio::process::Command::new("zenity")
        .args(["--entry", "--title", title, "--text", text])
        .output()
        .await
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

async fn prompt_source(kind: PromptKind) -> Option<Provider> {
    match kind {
        PromptKind::Wallhaven => {
            let q = zenity_entry(
                "Add Wallhaven topic",
                "Search terms or tags\n(e.g.  space   ·   #landscape   ·   cyberpunk city   ·   id:711)",
            )
            .await?;
            Some(Provider::Wallhaven(Wallhaven { query: q, ..Default::default() }))
        }
        PromptKind::Artstation => {
            let u = zenity_entry("Add ArtStation artist", "Artist username (from artstation.com/<username>)").await?;
            let u = u.trim().trim_end_matches('/').rsplit('/').next().unwrap_or("").trim_start_matches('@').to_string();
            (!u.is_empty()).then_some(Provider::Artstation { user: u })
        }
        PromptKind::Unsplash => {
            let q = zenity_entry("Add Unsplash topic", "Search terms (needs keys.unsplash in config)").await?;
            Some(Provider::Unsplash { query: q })
        }
    }
}

// ===================== tray =====================

pub const SUGGESTED_TOPICS: &[&str] = &[
    "space",
    "nebula",
    "landscape",
    "mountains",
    "forest",
    "ocean",
    "nature",
    "city",
    "night",
    "cyberpunk",
    "sci-fi",
    "fantasy",
    "abstract",
    "minimalism",
    "architecture",
    "digital art",
];

pub struct Tray {
    tx: Tx,
    current: String,
    sources: Vec<(String, bool)>,
    lock: LockMode,
    monitors: MonitorMode,
    clock: bool,
}

async fn spawn_tray(tx: Tx, cfg: &Config) -> Option<ksni::Handle<Tray>> {
    use ksni::TrayMethods;
    let tray = Tray {
        tx,
        current: String::new(),
        sources: cfg.sources.iter().map(|s| (s.provider.label(), s.enabled)).collect(),
        lock: cfg.lockscreen.mode,
        monitors: cfg.monitors,
        clock: cfg.clock.enabled,
    };
    match tray.spawn().await {
        Ok(h) => Some(h),
        Err(e) => {
            info!("no tray host ({e}); use notifications / shortcuts / CLI");
            None
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        config::APP.into()
    }
    fn title(&self) -> String {
        "Wallspin".into()
    }
    fn icon_name(&self) -> String {
        format!("{}-symbolic", crate::assets::APP_ID)
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Wallspin".into(),
            description: self.current.clone(),
            ..Default::default()
        }
    }
    /// Left click: next wallpaper.
    fn activate(&mut self, _x: i32, _y: i32) {
        send(&self.tx, Cmd::Next);
    }
    /// Scroll: walk history.
    fn scroll(&mut self, delta: i32, _o: ksni::Orientation) {
        send(&self.tx, if delta > 0 { Cmd::Prev } else { Cmd::Next });
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        fn item(label: &str, icon: &str, cmd: fn() -> Cmd) -> ksni::MenuItem<Tray> {
            StandardItem {
                label: label.into(),
                icon_name: icon.into(),
                activate: Box::new(move |t: &mut Tray| send(&t.tx, cmd())),
                ..Default::default()
            }
            .into()
        }

        let mut sources: Vec<ksni::MenuItem<Self>> = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, (label, on))| {
                CheckmarkItem {
                    label: label.clone(),
                    checked: *on,
                    activate: Box::new(move |t: &mut Tray| send(&t.tx, Cmd::ToggleSource(i))),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        sources.push(MenuItem::Separator);
        sources.push(item("Add Wallhaven topic…", "list-add", || Cmd::Prompt(PromptKind::Wallhaven)));
        sources.push(item("Add ArtStation artist…", "list-add", || Cmd::Prompt(PromptKind::Artstation)));
        sources.push(item("Add Unsplash topic…", "list-add", || Cmd::Prompt(PromptKind::Unsplash)));
        sources.push(item("Add Bing photo of the day", "list-add", || {
            Cmd::AddSource(Provider::Bing { market: "en-US".into() })
        }));
        sources.push(item("Add NASA picture of the day", "list-add", || Cmd::AddSource(Provider::Apod)));

        let suggested: Vec<ksni::MenuItem<Self>> = SUGGESTED_TOPICS
            .iter()
            .map(|topic| {
                let topic = topic.to_string();
                StandardItem {
                    label: topic.clone(),
                    activate: Box::new(move |t: &mut Tray| {
                        send(
                            &t.tx,
                            Cmd::AddSource(Provider::Wallhaven(Wallhaven { query: topic.clone(), ..Default::default() })),
                        )
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        let lock_modes = [LockMode::Same, LockMode::Separate, LockMode::Off];
        let lock_menu = RadioGroup {
            selected: lock_modes.iter().position(|m| *m == self.lock).unwrap_or(0),
            select: Box::new(move |t: &mut Tray, i| send(&t.tx, Cmd::SetLock(lock_modes[i]))),
            options: vec![
                RadioItem { label: "Same as desktop".into(), ..Default::default() },
                RadioItem { label: "Separate image (from favorites)".into(), ..Default::default() },
                RadioItem { label: "Don't touch".into(), ..Default::default() },
            ],
        };

        let mon_modes = [MonitorMode::Each, MonitorMode::Same, MonitorMode::Span];
        let mon_menu = RadioGroup {
            selected: mon_modes.iter().position(|m| *m == self.monitors).unwrap_or(0),
            select: Box::new(move |t: &mut Tray, i| send(&t.tx, Cmd::SetMonitors(mon_modes[i]))),
            options: vec![
                RadioItem { label: "Different image on each".into(), ..Default::default() },
                RadioItem { label: "Same image on each".into(), ..Default::default() },
                RadioItem { label: "One image across all".into(), ..Default::default() },
            ],
        };

        vec![
            StandardItem { label: self.current.clone(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            item("Next", "go-next", || Cmd::Next),
            item("Previous", "go-previous", || Cmd::Prev),
            item("♥ Favorite", "emblem-favorite", || Cmd::Fav),
            item("Trash", "user-trash", || Cmd::Trash),
            item("Ban (never show again)", "action-unavailable", || Cmd::Ban),
            MenuItem::Separator,
            SubMenu { label: "Sources".into(), submenu: sources, ..Default::default() }.into(),
            SubMenu { label: "Add suggested topic".into(), submenu: suggested, ..Default::default() }.into(),
            SubMenu { label: "Monitors".into(), submenu: vec![mon_menu.into()], ..Default::default() }.into(),
            CheckmarkItem {
                label: "Clock on wallpaper".into(),
                checked: self.clock,
                activate: Box::new(|t: &mut Tray| send(&t.tx, Cmd::ToggleClock)),
                ..Default::default()
            }
            .into(),
            SubMenu { label: "Lock screen".into(), submenu: vec![lock_menu.into()], ..Default::default() }.into(),
            item("Download more now", "emblem-downloads", || Cmd::Download(0)),
            MenuItem::Separator,
            StandardItem {
                label: "Preferences…".into(),
                icon_name: "preferences-system-symbolic".into(),
                activate: Box::new(|_: &mut Tray| crate::assets::open_prefs()),
                ..Default::default()
            }
            .into(),
            item("Quit", "application-exit", || Cmd::Quit),
        ]
    }
}
