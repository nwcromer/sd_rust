# sd_rust — TODO

Central backlog. A `[code: file.rs]` tag points at the source file an item
relates to. TODOs live here, not as markers in the code.

## Deferred from the v1 spec

- **Run-any-command action** — generic command execution, not just `.desktop`
  launch. Trust-sensitive. `[code: launch.rs]`
- **Multi-page layouts** — more than one flat page of 15 keys. `[code: config.rs]`
- **GPU card override** — config option to pick a specific card instead of
  auto-detecting the discrete one. `[code: widgets.rs]`
- **NVIDIA / Intel GPU support** — v1 is AMD-only. `[code: widgets.rs]`
- **Event-driven audio** — subscribe to PipeWire mute-change events instead of
  the current libpulse poll. `[code: audio.rs]`
- **Dim-on-screen-lock / blank-on-system-screensaver** — v1 only blanks on its
  own idle timeout. `[code: runtime.rs]`
- **Per-key error icons** — v1 has a single global error icon. `[code: runtime.rs]`
- **Layout-aware macro mapping** — v1 maps on a fixed US QWERTY layout.
  `[code: keys.rs]`
- **Multi-device support** — v1 is single-device. `[code: device.rs]`

## Enhancements requested during development

- **Full-deck image** — slice one source image across the 3×5 grid so the keys
  together show one picture. `[code: render.rs]`
- **Text labels on graphs** — optional "CPU"/"GPU" overlay; needs a bitmap font
  or a font dependency (v1 is font-free). `[code: widgets.rs]`
- **Show a tray-only running app** — when an app is running but only in the
  system tray (no window), launch-or-raise currently does nothing; should
  re-show it. Generalize beyond Steam. `[code: launch.rs]`
- **Steam game launcher action** — launch a specific game by id (e.g.
  `steam://rungameid/570`); starts Steam first if needed. Generalize to other
  stores. `[code: launch.rs]`

## Polish / assets

- **Better OBS button icons** — the record/replay icons in
  `~/.config/sd_rust/icons/obs/` are hand-drawn placeholders (gray/red record
  dot, amber pause, green replay, blue save arrow).
- **Icon margin tuning** — content-aware trim+margin is implemented (8px); the
  margin amount may want tuning.
