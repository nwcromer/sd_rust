# sd_rust

A lean, config-driven daemon that turns an **Elgato Stream Deck MK.2** into a
programmable control surface on Linux (Wayland / KDE Plasma). Each of the 15
keys declares **what image it shows** and **what happens when it is pressed**.
It runs as a background **systemd user service**.

---

## Features

- **15 keys**, addressed by grid coordinate (`r1c1` … `r3c5`).
- **Actions:** launch an application, play a keyboard macro (chords + literal
  text via a uinput virtual keyboard), toggle mic / system-output mute, and six
  OBS actions (start/stop recording, pause-toggle, enable/disable/save the
  replay buffer).
- **State-driven keys:** mute keys and OBS record / replay keys swap their image
  to reflect *live* state (read from `wpctl` and OBS events, not just what
  sd_rust last did).
- **Widgets:** CPU, RAM, network, and AMD GPU usage, drawn as live graphs.
- **Images only** — no text/fonts. Icons come from explicit file paths or are
  auto-resolved from a `.desktop` entry / icon theme.
- **Idle blanking** with wake-on-press, **suspend/resume** repaint, **hot-plug**
  reconnect, and brief **error-icon feedback** when an action fails.

---

## Requirements

- Linux, **Wayland** (KDE Plasma). v1 is Wayland-only.
- An Elgato Stream Deck **MK.2** (`0fd9:00a5` — the scissor-key revision — or the
  classic `0fd9:0080`). Single device.
- **PipeWire / WirePlumber** (`wpctl`) for the mute actions.
- **OBS Studio** with **obs-websocket v5** (built in to OBS ≥ 28) for the OBS
  actions — optional.
- AMD GPU on `amdgpu` for the GPU widget (NVIDIA/Intel are TODO). All metrics are
  read directly from `/proc` and `/sys` — no extra services.
- A recent Rust toolchain (edition 2024).

---

## Build & install

```sh
cargo build --release
install -Dm755 target/release/sd_rust ~/.local/bin/sd_rust

# 1. udev rules (one-time, needs root) — see "Security" below.
sudo ~/.local/bin/sd_rust --create-udev-rules
sudo udevadm control --reload-rules && sudo udevadm trigger

# 2. write a config (see "Configuration").
mkdir -p ~/.config/sd_rust && $EDITOR ~/.config/sd_rust/config.toml

# 3. install + start the user service.
~/.local/bin/sd_rust --install-service
```

Useful commands:

```sh
systemctl --user status sd_rust
journalctl --user -u sd_rust -f
systemctl --user restart sd_rust   # apply config changes (see below)
~/.local/bin/sd_rust --remove-service
```

Run it in the foreground while iterating: `sd_rust --verbose`
(`--config <path>` to use a non-default config).

### No hot-reload

The config is read **once at startup**. To apply changes, restart the service:
`systemctl --user restart sd_rust`. (No file-watching, no SIGHUP — by design.)

---

## CLI

| Flag | Effect |
|------|--------|
| `--config <PATH>` | Use a specific config file (default `~/.config/sd_rust/config.toml`). |
| `--verbose` | Debug-level logging. `RUST_LOG` overrides. |
| `--create-udev-rules` | Write `/etc/udev/rules.d/70-sd_rust.rules` (needs root), then exit. |
| `--install-service` | Install + enable + start the `sd_rust` user service, then exit. |
| `--remove-service` | Stop + disable + remove the user service, then exit. |

---

## Configuration

TOML at `~/.config/sd_rust/config.toml`. Clean and flat — one page. Keys are
addressed by **grid coordinate** `rRcC`: rows `1..3` top-to-bottom, columns
`1..5` left-to-right (`r1c1` = top-left). Unconfigured keys are blank.

### Global options

```toml
# Backlight. brightness = awake level (0–100). After dim_timeout_secs of no
# input the deck dims to dim_brightness; after idle_timeout_secs it blanks fully
# (backlight off). 0 disables that stage. The first press while fully blanked is
# consumed (just wakes); a press while only dimmed acts normally.
brightness = 100                  # default 100
dim_timeout_secs = 0              # 0 = no dimming
dim_brightness = 30               # brightness when dimmed
idle_timeout_secs = 300           # blank after ~5 min; 0 disables

# Optional override for the failure-feedback icon (bundled default otherwise).
error_icon = "/home/me/.config/sd_rust/icons/error.png"
```

### OBS connection (optional)

```toml
[obs]
host = "localhost"
port = 4455
password = "secret"   # prefer the env var below; never commit a real password
```

The password may instead be supplied via the **`SD_RUST_OBS_PASSWORD`**
environment variable, which **overrides** the file value. The password is never
logged. If a plaintext password is left in the file, restrict it:
`chmod 600 ~/.config/sd_rust/config.toml` (sd_rust warns if it's group/other
readable).

### Keys

Every `[key.rRcC]` table has an `action`. The fields below are per action.

#### `launch_app` — launch a desktop application (or raise it)

```toml
[key.r1c1]
action = "launch_app"
app = "firefox"              # .desktop id or name (e.g. org.kde.konsole)
# icon = "/path/icon.png"    # optional; otherwise auto-resolved from the .desktop
# window_class = "firefox"   # optional; override the class used to find a running window
```

**Launch-or-raise:** if the app isn't running, it's launched; if it *is*
running, its existing window is raised instead of starting a second copy
(taskbar-style). Launching uses `systemd-run --user`, so the app runs outside
the daemon's sandbox and isn't killed when sd_rust restarts. (Generic "run any
command" is TODO.)

Raising goes through **KWin** over D-Bus (the only Wayland-safe way to activate
another app's window — no external tool needed). A running instance is detected
by process, and its window is matched by class against the app id, the
`.desktop` `StartupWMClass`, and the exec name (case-insensitive). For apps
whose window class matches none of those (some Electron/Java/wrapped apps), set
`window_class` explicitly. Apps open only in the system tray (no window), and
Flatpak apps, may fall back to launching a new instance. Raise is KDE/KWin-only;
on other compositors `launch_app` simply launches.

Icons may be **PNG/JPG/etc. or SVG** (SVG is rasterized via `resvg`), so theme
icons — which are usually SVG — work directly. Auto icon resolution uses the
app's `.desktop` `Icon=` and the icon theme.

#### `macro` — keyboard macro

```toml
[key.r1c2]
action = "macro"
steps = [
    { chord = "ctrl+shift+t" },   # modifier+key
    { text  = "chicken" },        # literal text
    { chord = "Return" },
]
# icon = "/path/icon.png"
```

Each step is either a **chord** or **literal text**, played in order through a
uinput virtual keyboard. Chord modifiers: `ctrl`/`control`, `shift`, `alt`,
`super`/`meta`/`win`. The main key is a single character or a named key
(`Return`/`Enter`, `Tab`, `Esc`, `Space`, `Backspace`, `Delete`, `Home`, `End`,
`PageUp`, `PageDown`, `Up`/`Down`/`Left`/`Right`, `F1`…`F12`). Characters are
mapped on a **US layout**; the active layout is applied by the compositor.

#### `mic_mute` / `system_mute` — toggle mute (state-driven)

```toml
[key.r1c3]
action = "mic_mute"            # or "system_mute"
icon_unmuted = "/path/mic-on.png"
icon_muted   = "/path/mic-off.png"
```

Pressing toggles mute via `wpctl`; the image follows the live mute state (polled
~1 s and refreshed instantly on toggle).

#### OBS recording

```toml
[key.r2c1]
action = "obs_record_pause"        # the recommended recording-status key
icon_stopped      = "/path/rec-off.png"
icon_recording    = "/path/rec-on.png"
icon_paused       = "/path/rec-paused.png"
icon_disconnected = "/path/rec-disc.png"   # optional; falls back to icon_stopped
```

Recording actions and the state each reflects:

| `action` | Press does | Image follows |
|----------|-----------|----------------|
| `obs_record_start` | start recording | recording state |
| `obs_record_stop`  | stop recording  | recording state |
| `obs_record_pause` | toggle pause    | recording state (stopped/recording/paused) |

Recording state is event-driven, so changes made in the OBS GUI are reflected.

#### OBS replay buffer

```toml
[key.r2c2]
action = "obs_replay_start"        # or obs_replay_stop
icon_armed        = "/path/replay-on.png"
icon_disarmed     = "/path/replay-off.png"
icon_disconnected = "/path/replay-disc.png"   # optional; falls back to icon_disarmed

[key.r2c3]
action = "obs_replay_save"         # not a toggle, no state — static image
icon = "/path/replay-save.png"
```

| `action` | Press does | Image follows |
|----------|-----------|----------------|
| `obs_replay_start` | enable the replay buffer  | armed/disarmed |
| `obs_replay_stop`  | disable the replay buffer | armed/disarmed |
| `obs_replay_save`  | save the buffer | (static; no success feedback) |

While OBS is disconnected, OBS keys show their `icon_disconnected` (or the
sensible fallback). A failed OBS action (OBS not connected, or e.g. saving with
the buffer off) briefly flashes the error icon.

#### `widget` — live metric graph

```toml
[key.r3c5]
action = "widget"
widget = "gpu"            # cpu | ram | network | gpu
refresh_secs = 2          # optional, default 1
# interface = "eth0"      # network only; auto-detects the default route if unset
```

Widgets are graphics only (no numbers), drawn as filled area silhouettes:

- **CPU / GPU** — colored by load: green ≤10%, red ≥90%, green→yellow→red between
  (per pixel, so a tall column runs green→red bottom-to-top). CPU is the all-core
  aggregate from `/proc/stat`; GPU reads `gpu_busy_percent` from the
  auto-selected **discrete** AMD card (by largest VRAM). Both are **absolute**
  0–100%.
- **RAM** — used-memory fraction, fixed color.
- **Network** — down/up split (download grows up from the center, upload down),
  pastel cyan/purple, smoothed. Scaled **absolutely on a log curve against the
  interface's link speed** when it's known (`/sys/class/net/<iface>/speed`); if
  the link speed isn't reported (Wi-Fi, virtual), it falls back to a relative
  auto-scale. Any nonzero traffic always shows at least one pixel.

The graphs sit on a small black margin like the icons.

#### `sleep` — turn the deck off / on

```toml
[key.r3c5]
action = "sleep"
icon = "/usr/share/icons/breeze-dark/actions/22/system-suspend.svg"
```

Press to turn the backlight **off**; press again to turn it back on. Unlike the
idle blank, it stays off regardless of `idle_timeout_secs` until this key is
pressed again — and while off, **only this key** wakes the deck (other presses
are ignored).

### Example

```toml
idle_timeout_secs = 300

[obs]
host = "localhost"
port = 4455

[key.r1c1]
action = "launch_app"
app = "firefox"

[key.r1c2]
action = "mic_mute"
icon_unmuted = "/home/me/.config/sd_rust/icons/mic-on.png"
icon_muted   = "/home/me/.config/sd_rust/icons/mic-off.png"

[key.r2c1]
action = "obs_record_pause"
icon_stopped   = "/home/me/.config/sd_rust/icons/rec-off.png"
icon_recording = "/home/me/.config/sd_rust/icons/rec-on.png"
icon_paused    = "/home/me/.config/sd_rust/icons/rec-paused.png"

[key.r3c5]
action = "widget"
widget = "cpu"
```

---

## Security

sd_rust runs as a **normal user**. Root is needed only once, for
`--create-udev-rules`. The daemon never needs root, and the systemd unit is
sandboxed (`ProtectSystem=strict`, `ProtectHome=read-only`, `NoNewPrivileges`,
a syscall filter, etc.). Launched applications are started via
`systemd-run --user` so they run *outside* that sandbox.

Two udev grants are installed, each scoped via `uaccess` (a logind ACL tied to
your **active local session** — not a world-readable chmod):

1. **Stream Deck HID** (`0fd9:00a5`) — so the daemon can drive the deck.
2. **`/dev/uinput`** — required by the keyboard-macro feature.

> ⚠️ **uinput grant.** The `/dev/uinput` grant lets your session create a virtual
> keyboard that can inject keystrokes into **any** application. This is inherent
> to the uinput macro mechanism — there is no narrower kernel interface for
> synthesizing a real keyboard. We scope it as tightly as the mechanism allows
> (active-session `uaccess`, no static world/group write). Because the config
> defines exactly what gets typed (and what apps get launched), **treat your
> config file as trust-sensitive**, like an executable dotfile.

**OBS password:** read from `[obs] password` or, preferably, the
`SD_RUST_OBS_PASSWORD` environment variable (which wins). Never logged. If left
in the file, `chmod 600` it. obs-websocket is plain `ws://` (no TLS), so for a
non-local OBS host the password crosses the network in cleartext — tunnel it.

No network exposure beyond the configured local OBS websocket. No telemetry.

---

## Deferred to v2

Generic run-any-command action; multi-page layouts; configurable awake
brightness; a GPU-card override and NVIDIA/Intel GPU support; libpulse/event-
driven mute (vs the current `wpctl` poll); dim-on-lock / blank-on-screensaver;
per-key error icons; layout-aware macro mapping; multi-device support.
