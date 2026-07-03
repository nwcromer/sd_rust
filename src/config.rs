//! Configuration: TOML schema, loading, and validation.
//!
//! The config is read **once at startup** (no hot-reload — to apply changes the
//! user runs `systemctl --user restart sd_rust`).
//!
//! Keys are addressed by **grid coordinate** `rRcC` (rows 1..=3 top-to-bottom,
//! columns 1..=5 left-to-right; `r1c1` = top-left). Unconfigured keys are blank.
//!
//! Security: the config is trust-sensitive — it can launch applications and
//! synthesize keystrokes (uinput). Treat it like an executable dotfile. The OBS
//! password is read here but **never logged**; an env var overrides the file
//! value (see [`resolve_obs_password`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Environment variable that overrides the `[obs] password` from the file.
/// Env wins so secrets can be injected without leaving them in dotfile backups.
pub const OBS_PASSWORD_ENV: &str = "SD_RUST_OBS_PASSWORD";

/// Default idle-blank timeout when `idle_timeout_secs` is unset (~5 min).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Fixed Stream Deck MK.2 grid. v1 is single-device and these match both the
/// `Mk2` and `Mk2Scissor` kinds (verified against the `elgato-streamdeck`
/// crate's `Kind` table). The device layer asserts the connected device
/// actually has this geometry before honoring the map.
pub const GRID_ROWS: u8 = 3;
pub const GRID_COLS: u8 = 5;
pub const KEY_COUNT: usize = (GRID_ROWS as usize) * (GRID_COLS as usize);

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Blank the whole deck after this many seconds of no input. `0` disables
    /// blanking entirely. Defaults to [`DEFAULT_IDLE_TIMEOUT_SECS`].
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,

    /// Awake brightness percent (0–100).
    #[serde(default = "default_brightness")]
    pub brightness: u8,

    /// Dim to [`dim_brightness`](Self::dim_brightness) after this many seconds of
    /// no input. `0` disables dimming (the deck stays at full brightness until it
    /// blanks). Should be less than `idle_timeout_secs` to take effect.
    #[serde(default)]
    pub dim_timeout_secs: u64,

    /// Brightness percent (0–100) when dimmed.
    #[serde(default = "default_dim_brightness")]
    pub dim_brightness: u8,

    /// Idle screensaver kind. Defaults to none (dim straight to blank).
    #[serde(default)]
    pub screensaver: Screensaver,

    /// Show the screensaver after this many seconds of no input. `0` disables it.
    /// The idle hierarchy is dim < screensaver < blank: whichever crossed
    /// threshold is deepest wins, so the screensaver never runs once blanked.
    #[serde(default)]
    pub screensaver_timeout_secs: u64,

    /// Optional global override for the failure-feedback icon. When unset the
    /// bundled default error icon is used.
    #[serde(default)]
    pub error_icon: Option<PathBuf>,

    /// OBS connection. Absent → OBS integration disabled.
    #[serde(default)]
    pub obs: Option<ObsConfig>,

    /// Key map, addressed by `rRcC` grid coordinate. A single flat page of 15
    /// keys (no page structure).
    #[serde(default)]
    pub key: BTreeMap<String, KeyConfig>,
}

fn default_idle_timeout() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECS
}
fn default_brightness() -> u8 {
    100
}
fn default_dim_brightness() -> u8 {
    30
}

impl Config {
    pub fn idle_timeout(&self) -> Option<Duration> {
        match self.idle_timeout_secs {
            0 => None,
            n => Some(Duration::from_secs(n)),
        }
    }

    /// Idle duration before dimming (`None` if disabled).
    pub fn dim_timeout(&self) -> Option<Duration> {
        match self.dim_timeout_secs {
            0 => None,
            n => Some(Duration::from_secs(n)),
        }
    }

    /// Awake brightness, clamped to 0–100.
    pub fn brightness(&self) -> u8 {
        self.brightness.min(100)
    }

    /// Dimmed brightness, clamped to 0–100.
    pub fn dim_brightness(&self) -> u8 {
        self.dim_brightness.min(100)
    }

    /// Idle duration before the screensaver (`None` if no kind or timeout set).
    pub fn screensaver_timeout(&self) -> Option<Duration> {
        match (self.screensaver, self.screensaver_timeout_secs) {
            (Screensaver::None, _) | (_, 0) => None,
            (_, n) => Some(Duration::from_secs(n)),
        }
    }
}

/// Idle screensaver kind, shown between the dim and blank stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Screensaver {
    /// No screensaver — the deck dims straight to blank.
    #[default]
    None,
    /// "Matrix"-style green digital rain across the whole deck.
    Matrix,
}

/// OBS obs-websocket v5 connection settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObsConfig {
    #[serde(default = "default_obs_host")]
    pub host: String,
    #[serde(default = "default_obs_port")]
    pub port: u16,
    /// Plaintext password from the file. Prefer the `SD_RUST_OBS_PASSWORD` env
    /// var (see [`resolve_obs_password`]). **Never logged.**
    #[serde(default)]
    pub password: Option<String>,
    /// Poll OBS capture liveness (~1s) so record keys can show
    /// `icon_not_capturing` when connected but capturing nothing. Opt-in.
    #[serde(default)]
    pub show_capture_status: bool,
    /// Shown on any stopped record key while `show_capture_status` is on and
    /// OBS's screen/window capture is blind (0x0). Falls back to `icon_stopped`.
    #[serde(default)]
    pub icon_not_capturing: Option<PathBuf>,
}

fn default_obs_host() -> String {
    "localhost".to_string()
}
fn default_obs_port() -> u16 {
    4455
}

/// One configured key. The `action` tag selects the variant; the remaining
/// fields are the per-action options (icons, macro steps, widget settings).
///
/// State-driven keys (mute toggles, OBS record/replay) carry per-state icons;
/// the renderer picks the image from live state. A key that omits the per-state
/// icons falls back to a blank tile for that state.
///
/// OBS variants pull their per-state icon group in with `#[serde(flatten)]`; the
/// two mute variants spell their icons out inline.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum KeyConfig {
    /// Launch a `.desktop` application, or raise it if already running (§5.1).
    /// Icon auto-resolves from the app's `.desktop` if `icon` unset.
    LaunchApp {
        /// `.desktop` id or application name (e.g. `firefox`, `org.kde.konsole`).
        app: String,
        #[serde(default)]
        icon: Option<PathBuf>,
        /// Override the window class used to find a running instance to raise.
        /// Defaults to the app id / `StartupWMClass` / exec name from the
        /// `.desktop`; set this for apps whose window class matches none of
        /// those (some Electron/Java/wrapped apps).
        #[serde(default)]
        window_class: Option<String>,
    },

    /// Keyboard macro: an ordered sequence of modifier+key chords and literal
    /// text, injected via a uinput virtual keyboard.
    Macro {
        steps: Vec<MacroStep>,
        #[serde(default)]
        icon: Option<PathBuf>,
    },

    /// Toggle microphone mute (PipeWire). State-driven: muted vs unmuted.
    MicMute {
        #[serde(default)]
        icon_muted: Option<PathBuf>,
        #[serde(default)]
        icon_unmuted: Option<PathBuf>,
    },

    /// Toggle system output mute (PipeWire). State-driven: muted vs unmuted.
    SystemMute {
        #[serde(default)]
        icon_muted: Option<PathBuf>,
        #[serde(default)]
        icon_unmuted: Option<PathBuf>,
    },

    /// OBS: start recording. Reflects live recording state if state icons given.
    ObsRecordStart {
        #[serde(flatten)]
        icons: RecordIcons,
    },
    /// OBS: stop recording.
    ObsRecordStop {
        #[serde(flatten)]
        icons: RecordIcons,
    },
    /// OBS: pause/resume recording (toggles pause). Natural "recording status"
    /// surface — give it `icon_recording`/`icon_paused`/`icon_stopped` for the
    /// three-state display.
    ObsRecordPause {
        #[serde(flatten)]
        icons: RecordIcons,
    },
    /// OBS: toggle recording (start if stopped, stop if recording). Three-state
    /// display like `obs_record_pause`.
    ObsRecordToggle {
        #[serde(flatten)]
        icons: RecordIcons,
    },

    /// OBS: enable (start) the replay buffer. Reflects armed/disarmed state.
    ObsReplayStart {
        #[serde(flatten)]
        icons: ReplayIcons,
    },
    /// OBS: disable (stop) the replay buffer.
    ObsReplayStop {
        #[serde(flatten)]
        icons: ReplayIcons,
    },
    /// OBS: toggle the replay buffer (arm if off, disarm if on). Reflects
    /// armed/disarmed state.
    ObsReplayToggle {
        #[serde(flatten)]
        icons: ReplayIcons,
    },
    /// OBS: save the replay buffer. Pressing saves; the image reflects the live
    /// armed/disarmed state so you can see whether a save will work.
    ObsReplaySave {
        #[serde(flatten)]
        icons: ReplayIcons,
    },

    /// A live, timer-rendered metric widget (graph only, no text).
    Widget {
        widget: WidgetKind,
        /// Re-render interval in seconds. Defaults per widget in `widgets.rs`.
        #[serde(default)]
        refresh_secs: Option<u64>,
        /// Network only: interface to graph. Unset → auto-detect default route.
        #[serde(default)]
        interface: Option<String>,
    },

    /// Toggle the deck's backlight off ("sleep") / on. Unlike the idle blank,
    /// it stays off regardless of the idle timeout until any key is pressed
    /// (that waking press is consumed and doesn't trigger its action).
    Sleep {
        #[serde(default)]
        icon: Option<PathBuf>,
    },
}

/// A single macro step: either a chord (modifier+key) or literal text.
/// Untagged so the TOML is terse: `{ chord = "ctrl+shift+t" }` or
/// `{ text = "chicken" }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum MacroStep {
    /// e.g. `"ctrl+shift+t"`, `"Return"`, `"alt+F4"`. Parsed in `keys.rs`.
    Chord { chord: String },
    /// Literal characters to type, e.g. `"chicken"`.
    Text { text: String },
}

/// Recording state icons (three states + disconnected fallback).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecordIcons {
    #[serde(default)]
    pub icon_stopped: Option<PathBuf>,
    #[serde(default)]
    pub icon_recording: Option<PathBuf>,
    #[serde(default)]
    pub icon_paused: Option<PathBuf>,
    /// Shown while OBS is disconnected. Falls back to `icon_stopped`.
    #[serde(default)]
    pub icon_disconnected: Option<PathBuf>,
}

/// Replay-buffer state icons (armed / disarmed + disconnected fallback).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReplayIcons {
    /// Buffer running/armed.
    #[serde(default)]
    pub icon_armed: Option<PathBuf>,
    /// Buffer stopped/disarmed.
    #[serde(default)]
    pub icon_disarmed: Option<PathBuf>,
    #[serde(default)]
    pub icon_disconnected: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Cpu,
    Ram,
    Network,
    Gpu,
}

/// A key's `rRcC` coordinate resolved to a flat key index (`0` = top-left,
/// row-major) — matches the Stream Deck's button numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeyCoord {
    pub row: u8,
    pub col: u8,
}

impl KeyCoord {
    /// Flat index into the device's button array (row-major, 0-based).
    pub fn index(self) -> usize {
        ((self.row - 1) as usize) * (GRID_COLS as usize) + (self.col - 1) as usize
    }

    /// Parse an `rRcC` coordinate string, validating it is on the grid.
    pub fn parse(s: &str) -> Result<Self> {
        let rest = s
            .strip_prefix('r')
            .with_context(|| format!("key coordinate {s:?} must start with 'r' (e.g. r1c1)"))?;
        let (row_str, col_str) = rest
            .split_once('c')
            .with_context(|| format!("key coordinate {s:?} must be of the form rRcC (e.g. r2c3)"))?;
        let row: u8 = row_str
            .parse()
            .with_context(|| format!("invalid row in key coordinate {s:?}"))?;
        let col: u8 = col_str
            .parse()
            .with_context(|| format!("invalid column in key coordinate {s:?}"))?;
        if !(1..=GRID_ROWS).contains(&row) || !(1..=GRID_COLS).contains(&col) {
            bail!(
                "key coordinate {s:?} is off the {GRID_ROWS}x{GRID_COLS} grid \
                 (rows 1..={GRID_ROWS}, cols 1..={GRID_COLS})"
            );
        }
        Ok(KeyCoord { row, col })
    }
}

/// Default config path: `~/.config/sd_rust/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("sd_rust").join("config.toml"))
}

/// Load and validate the config from `path`.
pub fn load_config(path: &Path) -> Result<Config> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config file: {}", path.display()))?;

    // Validate every coordinate parses & is on-grid, and reject duplicate
    // indices (e.g. both `r1c1` and a future alias mapping to key 0).
    let mut seen = [false; KEY_COUNT];
    for coord_str in config.key.keys() {
        let coord = KeyCoord::parse(coord_str)
            .with_context(|| format!("in [key.{coord_str}]"))?;
        let idx = coord.index();
        if seen[idx] {
            bail!("duplicate key mapping for index {idx} ([key.{coord_str}])");
        }
        seen[idx] = true;
    }

    warn_if_obs_password_world_readable(path, &config);
    Ok(config)
}

/// Resolve the effective OBS password: env var wins over the file value.
/// Empty values are treated as unset. Returns `None` when neither is provided
/// (obs-websocket may be configured without auth).
///
/// Kept separate from logging paths so the secret is only ever read here and
/// handed straight to the obws client.
pub fn resolve_obs_password(cfg: &ObsConfig) -> Option<String> {
    std::env::var(OBS_PASSWORD_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.password.clone().filter(|s| !s.is_empty()))
}

/// Warn (with the `chmod 600` fix) if the config carries a plaintext OBS
/// password in the **file** but is group/other-readable. A password supplied
/// only via the env var does not trip this.
fn warn_if_obs_password_world_readable(path: &Path, config: &Config) {
    use std::os::unix::fs::PermissionsExt;

    let file_has_password = config
        .obs
        .as_ref()
        .and_then(|o| o.password.as_deref())
        .is_some_and(|p| !p.is_empty());
    if !file_has_password {
        return;
    }
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        log::warn!(
            "config file {} contains a plaintext [obs] password but is readable \
             by group/other (mode {:o}). Restrict it with: chmod 600 {}",
            path.display(),
            mode & 0o7777,
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coord_parses_and_indexes_row_major() {
        assert_eq!(KeyCoord::parse("r1c1").unwrap().index(), 0);
        assert_eq!(KeyCoord::parse("r1c5").unwrap().index(), 4);
        assert_eq!(KeyCoord::parse("r2c1").unwrap().index(), 5);
        assert_eq!(KeyCoord::parse("r3c5").unwrap().index(), 14);
    }

    #[test]
    fn coord_rejects_off_grid_and_malformed() {
        assert!(KeyCoord::parse("r0c1").is_err());
        assert!(KeyCoord::parse("r4c1").is_err());
        assert!(KeyCoord::parse("r1c6").is_err());
        assert!(KeyCoord::parse("x1c1").is_err());
        assert!(KeyCoord::parse("r1").is_err());
        assert!(KeyCoord::parse("rc").is_err());
    }

    #[test]
    fn env_password_overrides_file() {
        let cfg = ObsConfig {
            host: "localhost".into(),
            port: 4455,
            password: Some("from-file".into()),
            show_capture_status: false,
            icon_not_capturing: None,
        };
        // SAFETY: single-threaded test; no other thread reads the env here.
        unsafe { std::env::set_var(OBS_PASSWORD_ENV, "from-env") };
        assert_eq!(resolve_obs_password(&cfg).as_deref(), Some("from-env"));
        unsafe { std::env::set_var(OBS_PASSWORD_ENV, "") };
        assert_eq!(resolve_obs_password(&cfg).as_deref(), Some("from-file"));
        unsafe { std::env::remove_var(OBS_PASSWORD_ENV) };
        assert_eq!(resolve_obs_password(&cfg).as_deref(), Some("from-file"));
    }

    #[test]
    fn screensaver_timeout_needs_both_a_kind_and_a_nonzero_timeout() {
        let with = |toml: &str| toml::from_str::<Config>(toml).unwrap().screensaver_timeout();
        // Kind defaults to none → no screensaver even with a timeout set.
        assert_eq!(with("screensaver_timeout_secs = 60"), None);
        // Kind set but timeout 0 (default) → disabled.
        assert_eq!(with(r#"screensaver = "matrix""#), None);
        // Both set → enabled.
        assert_eq!(
            with("screensaver = \"matrix\"\nscreensaver_timeout_secs = 60"),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn parses_a_representative_config() {
        let toml = r#"
            idle_timeout_secs = 120

            [obs]
            host = "localhost"
            port = 4455

            [key.r1c1]
            action = "launch_app"
            app = "firefox"

            [key.r1c2]
            action = "macro"
            steps = [ { chord = "ctrl+shift+t" }, { text = "chicken" } ]

            [key.r1c3]
            action = "mic_mute"
            icon_muted = "/icons/mic-off.png"
            icon_unmuted = "/icons/mic-on.png"

            [key.r3c5]
            action = "widget"
            widget = "gpu"
            refresh_secs = 2
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.idle_timeout_secs, 120);
        assert_eq!(cfg.key.len(), 4);
        assert!(matches!(cfg.key["r1c1"], KeyConfig::LaunchApp { .. }));
        assert!(matches!(cfg.key["r1c2"], KeyConfig::Macro { .. }));
    }

    /// Verifies `#[serde(flatten)]` of the icon structs works inside the
    /// internally-tagged `KeyConfig` enum (a serde combination worth proving
    /// rather than assuming).
    #[test]
    fn parses_flattened_obs_state_icons() {
        let toml = r#"
            [key.r2c1]
            action = "obs_record_pause"
            icon_stopped = "/icons/rec-stopped.png"
            icon_recording = "/icons/rec-on.png"
            icon_paused = "/icons/rec-paused.png"

            [key.r2c2]
            action = "obs_replay_start"
            icon_armed = "/icons/replay-on.png"
            icon_disarmed = "/icons/replay-off.png"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        match &cfg.key["r2c1"] {
            KeyConfig::ObsRecordPause { icons } => {
                assert_eq!(
                    icons.icon_recording.as_deref(),
                    Some(Path::new("/icons/rec-on.png"))
                );
                assert_eq!(
                    icons.icon_paused.as_deref(),
                    Some(Path::new("/icons/rec-paused.png"))
                );
            }
            other => panic!("expected ObsRecordPause, got {other:?}"),
        }
        match &cfg.key["r2c2"] {
            KeyConfig::ObsReplayStart { icons } => {
                assert_eq!(
                    icons.icon_armed.as_deref(),
                    Some(Path::new("/icons/replay-on.png"))
                );
            }
            other => panic!("expected ObsReplayStart, got {other:?}"),
        }
    }
}
