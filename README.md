# wallspin

Wallpaper changer for GNOME (Wayland/X11), written in Rust. Inspired by [Variety](https://github.com/varietywalls/variety) and can import its settings.
One daemon, ~20 MiB steady RSS, bounded queues, and — importantly — no GNOME Shell GPU-memory leak.

## Why wallpaper changers leak on GNOME

GNOME Shell's `BackgroundTextureCache` (`ui/background.js`) maps *URI → GPU texture* and never
evicts; the only purge path is a file-monitor "changes done" event on the displayed file. Variety
(and anything that sets a new file per change) therefore pins one full-resolution texture per
change in GTT until you log out — seen as ever-growing GTT attributed to `systemd-logind`
(which hands gnome-shell its DRM fd). Check: `grep drm-memory-gtt /proc/$(pgrep -x gnome-shell)/fdinfo/*`.

wallspin only ever uses **one URI** (`wallspin-wallpaper.jpg`) and rewrites it **in place**:
GNOME's file monitor purges and reloads, so GTT stays flat. The file is rendered at screen size,
so the texture is never bigger than your monitors (an 8K source would otherwise cost 130 MB).

## Install

```sh
cargo install --path .
wallspin import-variety            # migrate ~/.config/variety (sources, bans, history, filters)
mkdir -p ~/.config/systemd/user && cp contrib/wallspin.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now wallspin
wallspin shortcuts                 # optional: Super+Alt+arrows
```

## Control surfaces

| Surface | Needs | What |
|---|---|---|
| Tray icon | any StatusNotifier host (AppIndicator ext, KDE, waybar) — auto-detected | left-click next, scroll history, menu: sources, add topic, suggested topics, lock mode |
| Notification | nothing | each change shows Next / ♥ Favorite / Trash / Ban buttons (`notify = false` to disable) |
| Shortcuts | nothing | `wallspin shortcuts` installs Super+Alt+→ ← ↑(fav) ↓(trash) Del(ban) |
| CLI | — | `next prev fav ban trash status download [n] reload quit` |

## Sources — topics, not URLs

```sh
wallspin source list
wallspin source topics                       # suggestions
wallspin source add wallhaven space --min-res 3440x1440
wallspin source add wallhaven "#landscape #sci-fi" --sort toplist --top 1M
wallspin source add artstation birez         # username or profile URL
wallspin source add bing                     # photo of the day (UHD)
wallspin source add apod                     # NASA APOD (keys.nasa, else DEMO_KEY)
wallspin source add unsplash mountains       # needs keys.unsplash
wallspin source add folder ~/Pictures/walls
wallspin source disable 3 | enable 3 | rm 3
```

The tray's *Add Wallhaven topic…* / *Add ArtStation artist…* open a small zenity dialog.

## Monitors

`monitors` in config, `wallspin monitors each|same|span`, or the tray's *Monitors* menu:

- `each` (default) — a different image per monitor, each preferring images of that monitor's
  aspect ratio and cropped to fit. History/prev/next restore the whole set; Fav/Ban/Trash act on
  the primary monitor's image.
- `same` — one image, zoomed onto each monitor.
- `span` — one image across the bounding box of all monitors.

Layout comes live from Mutter (`org.gnome.Mutter.DisplayConfig`), re-rendered on hotplug.

## Clock

`[clock]` in config (`wallspin clock on|off`, tray checkbox). Drawn in-process with your font
(fontconfig pattern, e.g. `Roboto Condensed:style=Light`), on the primary monitor. Each minute
only a ~2 MB compressed screen-sized base is decoded, the text drawn, the file rewritten.
Skipped while the screen is locked.

## Lock screen

GNOME Shell ≥ 3.38 draws the lock screen from the **desktop** wallpaper (blurred) and ignores
`org.gnome.desktop.screensaver picture-uri`. So:

- `lockscreen.mode = "same"` — default; lock shows the current wallpaper.
- `"separate"` — on `org.gnome.ScreenSaver.ActiveChanged(true)` the daemon swaps in an image from
  `lockscreen.folder` (default: favorites), and restores the desktop on unlock. Survives crashes
  (state persisted, restored on next start). Works on Wayland.
- `"off"` — don't touch.

## Files

- config: `~/.config/wallspin/config.toml` (`wallspin config-path`)
- state: `~/.local/share/wallspin/state.db` (history, bans, image origins)
- D-Bus: `io.github.Wallspin` `/io/github/Wallspin` iface `io.github.Wallspin1`

## Not yet

Settings window (GTK4/libadwaita), other desktops' backends (KDE/sway),
own SNI implementation on zbus 6 (to drop ksni's zbus 5).

## About this code

Wallspin was written with the help of an AI coding assistant (Claude), directed and tested by the
author on a real GNOME 51 desktop. Review it as you would any new project.
