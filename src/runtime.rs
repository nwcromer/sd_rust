//! The daemon runtime: owns the device, the per-key model + live state, and the
//! main loop.
//!
//! Architecture: a single **synchronous** main loop drives the Stream Deck
//! (blocking HID reads with a short timeout). Async work
//! that can't block the deck — OBS (obws/tokio) — lives on its own thread and
//! talks to us over channels. The suspend/resume listener (zbus) is a third
//! thread. tokio therefore exists only inside the OBS thread.
//!
//! Rendering: each tick we compute a cheap [`Visual`] descriptor per key and
//! only re-upload keys whose descriptor changed. Re-encoding/uploading 15 JPEGs
//! every 100 ms would waste USB bandwidth and flicker.
//
// Layered in over later milestones: OBS state, widgets, idle-blank,
// suspend/resume, and failure feedback.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{debug, info, warn};

use std::collections::HashMap;
use std::sync::mpsc::Receiver as StdReceiver;

use crate::audio::{self, Target};
use crate::config::{self, Config, KeyConfig, KeyCoord, GRID_COLS, KEY_COUNT};
use crate::device::{self, Device};
use crate::keys::MacroKeyboard;
use crate::launch;
use crate::obs::{self, ObsAction, ObsCommand, ObsEvent, ObsHandle};
use crate::osd;
use crate::render::{self, Tile};
use crate::state::{LiveState, RecordState, ReplayState};
use crate::widgets::Widget;

/// How long each `poll_presses` call blocks — also the loop tick.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// How often to re-read mute state from `wpctl`. Kept short so a mute toggled
/// elsewhere (KDE shortcut, panel) reflects on the deck quickly; our own
/// toggles refresh immediately and don't wait for this.
const AUDIO_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long an error icon flashes on a key after a failed action (§9).
const ERROR_FLASH: Duration = Duration::from_millis(800);

/// Backlight state driven by idle timeouts: full while active, dimmed after the
/// dim timeout, fully off (blanked) after the idle timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Display {
    Full,
    Dimmed,
    Blanked,
}

type KeyMap = Vec<Option<KeyConfig>>;

/// A cheap, comparable descriptor of what a key currently shows. Two equal
/// descriptors render identically, so the repaint pass can skip unchanged keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visual {
    /// A static image that never changes once painted (icon from file/theme).
    Static,
    /// A mute toggle key; `None` = state not yet known.
    Mute(Option<bool>),
    /// An OBS recording key.
    Record(RecordState),
    /// An OBS replay-buffer key.
    Replay(ReplayState),
    /// A widget, identified by its render generation (bumped each refresh).
    Widget(u64),
    /// A transient failure-feedback flash (§9), overriding the normal image.
    Error,
}

pub struct Runtime {
    device: Device,
    keys: KeyMap,
    keyboard: Option<MacroKeyboard>,
    state: LiveState,
    /// Last-painted descriptor per key (`None` = never painted / force repaint).
    last_visual: [Option<Visual>; KEY_COUNT],
    prev_state: [bool; KEY_COUNT],
    last_audio_poll: Instant,
    /// Whether any configured key needs mic / system mute state at all (skip the
    /// `wpctl` polls entirely otherwise).
    needs_mic: bool,
    needs_system: bool,
    /// OBS command handle + event stream. `None` when `[obs]` isn't configured.
    obs: Option<ObsHandle>,
    obs_events: Option<StdReceiver<ObsEvent>>,
    obs_connected: bool,
    /// Live metric widgets, keyed by their key index.
    widgets: HashMap<usize, Widget>,
    /// Idle dim/blank settings + current backlight state.
    brightness: u8,
    dim_brightness: u8,
    dim_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    last_input: Instant,
    display: Display,
    /// Manual "sleep" (via a Sleep key) — forces the backlight off regardless of
    /// the idle timers until the Sleep key is pressed again.
    slept: bool,
    /// Resume-from-suspend notifications (logind `PrepareForSleep`).
    resume_rx: StdReceiver<()>,
    /// Per-key error-flash expiry; `Some(t)` shows the error icon until `t`.
    error_until: [Option<Instant>; KEY_COUNT],
    /// The failure-feedback tile (config override or bundled default) — a single
    /// global error icon.
    error_tile: Tile,
}

pub fn run(config: Config) -> Result<()> {
    let keys = build_key_map(&config);

    let keyboard = match MacroKeyboard::new() {
        Ok(kb) => Some(kb),
        Err(e) => {
            warn!("keyboard macros disabled: {e:#}");
            None
        }
    };

    let needs_mic = keys.iter().flatten().any(|k| matches!(k, KeyConfig::MicMute { .. }));
    let needs_system = keys.iter().flatten().any(|k| matches!(k, KeyConfig::SystemMute { .. }));

    // Spawn the OBS thread only if `[obs]` is configured. The password is
    // resolved here (env wins) and handed straight to the OBS thread.
    let (obs, obs_events) = match &config.obs {
        Some(obs_cfg) => {
            let password = config::resolve_obs_password(obs_cfg);
            let (handle, events) = obs::spawn(obs_cfg.clone(), password);
            (Some(handle), Some(events))
        }
        None => (None, None),
    };

    // Build a widget instance per widget key (each samples immediately so the
    // first paint shows real data).
    let mut widgets = HashMap::new();
    for (index, key) in keys.iter().enumerate() {
        if let Some(KeyConfig::Widget { widget, refresh_secs, interface }) = key {
            widgets.insert(index, Widget::new(*widget, *refresh_secs, interface.clone()));
        }
    }

    // Failure-feedback icon: the config override if set & loadable, else the
    // bundled default. Resolved once at startup.
    let error_tile = match &config.error_icon {
        Some(path) => render::load_icon(path).unwrap_or_else(|e| {
            warn!("error_icon: {e:#}; using the bundled default");
            render::default_error_icon()
        }),
        None => render::default_error_icon(),
    };

    let resume_rx = crate::suspend::spawn_resume_monitor();

    let device = Device::connect()?;
    device.set_brightness(config.brightness()).ok();
    let mut rt = Runtime {
        device,
        keys,
        keyboard,
        state: LiveState::default(),
        last_visual: [None; KEY_COUNT],
        prev_state: [false; KEY_COUNT],
        last_audio_poll: Instant::now(),
        needs_mic,
        needs_system,
        obs,
        obs_events,
        obs_connected: false,
        widgets,
        brightness: config.brightness(),
        dim_brightness: config.dim_brightness(),
        dim_timeout: config.dim_timeout(),
        idle_timeout: config.idle_timeout(),
        last_input: Instant::now(),
        display: Display::Full,
        slept: false,
        resume_rx,
        error_until: [None; KEY_COUNT],
        error_tile,
    };

    rt.refresh_audio();
    rt.repaint();
    info!("running — stop with `systemctl --user stop sd_rust` (or Ctrl+C)");
    rt.main_loop()
}

impl Runtime {
    fn main_loop(&mut self) -> Result<()> {
        loop {
            // Resume-from-suspend: restore the backlight so the deck reflects
            // whatever changed while asleep (but stay off if manually slept).
            if self.resume_rx.try_recv().is_ok() {
                self.restore_display();
                self.refresh_audio();
            }

            let pressed = match self.device.poll_presses(INPUT_POLL, &mut self.prev_state) {
                Ok(pressed) => pressed,
                Err(e) => {
                    warn!("Stream Deck read failed ({e:#}); reconnecting");
                    self.device = device::reconnect();
                    self.prev_state = [false; KEY_COUNT];
                    self.restore_display();
                    continue;
                }
            };

            if !pressed.is_empty() {
                self.last_input = Instant::now();
                if self.display == Display::Blanked {
                    // Off (idle blank or manual sleep): ANY press wakes the deck,
                    // and that press is CONSUMED — it doesn't trigger its action.
                    self.slept = false;
                    self.set_display(Display::Full);
                } else if pressed.iter().any(|&i| self.is_sleep_key(i)) {
                    // Sleep key while awake → blank the deck until any press wakes it.
                    self.slept = true;
                    self.set_display(Display::Blanked);
                    debug!("sleep");
                } else {
                    // Normal press: un-dim if dimmed, then run the action(s).
                    self.set_display(Display::Full);
                    for index in pressed {
                        self.on_press(index);
                    }
                }
            } else if !self.slept {
                // Idle → dim, then blank, by elapsed time (suspended while slept).
                let desired = self.idle_display();
                if desired != self.display {
                    self.set_display(desired);
                }
            }

            if self.last_audio_poll.elapsed() >= AUDIO_POLL_INTERVAL {
                self.refresh_audio();
            }
            self.drain_obs();

            // While fully blanked the panel is dark and frozen — skip widget
            // re-render and repaint to save USB bandwidth (state stays fresh via
            // the polls above and is repainted on wake).
            if self.display != Display::Blanked {
                self.tick_widgets();
                self.expire_flashes();
                self.repaint();
            }
        }
    }

    /// Restore the backlight after a resume or reconnect. If the deck is
    /// manually slept, keep it off (honoring the Sleep contract — only the Sleep
    /// key wakes it); otherwise wake to full brightness and repaint everything.
    fn restore_display(&mut self) {
        if self.slept {
            self.device.set_brightness(0).ok();
            self.display = Display::Blanked;
        } else {
            self.device.set_brightness(self.brightness).ok();
            self.display = Display::Full;
            self.last_input = Instant::now();
            self.force_full_repaint();
        }
    }

    /// Whether the key at `index` is a Sleep-toggle key.
    fn is_sleep_key(&self, index: usize) -> bool {
        matches!(self.keys[index], Some(KeyConfig::Sleep { .. }))
    }

    /// The backlight state implied by how long we've been idle.
    fn idle_display(&self) -> Display {
        let idle = self.last_input.elapsed();
        if let Some(t) = self.idle_timeout
            && idle >= t
        {
            return Display::Blanked;
        }
        if let Some(t) = self.dim_timeout
            && idle >= t
        {
            return Display::Dimmed;
        }
        Display::Full
    }

    /// Apply a backlight state. Waking from blank forces a full repaint so any
    /// state that changed while dark is shown.
    fn set_display(&mut self, state: Display) {
        if self.display == state {
            return;
        }
        let level = match state {
            Display::Full => self.brightness,
            Display::Dimmed => self.dim_brightness,
            Display::Blanked => 0,
        };
        if self.device.set_brightness(level).is_err() {
            return; // device likely gone; retry next tick
        }
        let was_blanked = self.display == Display::Blanked;
        self.display = state;
        debug!("display → {state:?} (brightness {level})");
        if was_blanked {
            self.force_full_repaint();
        }
    }

    /// Clear error flashes whose duration elapsed so the key reverts.
    fn expire_flashes(&mut self) {
        let now = Instant::now();
        for slot in &mut self.error_until {
            if matches!(slot, Some(t) if now >= *t) {
                *slot = None;
            }
        }
    }

    /// Start a failure-feedback flash on a key (§9).
    fn flash_error(&mut self, index: usize) {
        if index < KEY_COUNT {
            self.error_until[index] = Some(Instant::now() + ERROR_FLASH);
        }
    }

    /// Re-sample any widgets whose refresh interval elapsed (the generation
    /// bump makes the repaint pass redraw them).
    fn tick_widgets(&mut self) {
        for widget in self.widgets.values_mut() {
            widget.maybe_tick();
        }
    }

    /// Apply any queued OBS events to live state (non-blocking).
    fn drain_obs(&mut self) {
        let events: Vec<ObsEvent> = match &self.obs_events {
            Some(rx) => rx.try_iter().collect(),
            None => return,
        };
        for event in events {
            match event {
                ObsEvent::Connected => {
                    self.obs_connected = true;
                    debug!("OBS: connected");
                }
                ObsEvent::Disconnected => {
                    self.obs_connected = false;
                    self.state.record = RecordState::Disconnected;
                    self.state.replay = ReplayState::Disconnected;
                }
                ObsEvent::Record(s) => self.state.record = s,
                ObsEvent::Replay(s) => self.state.replay = s,
                ObsEvent::ActionFailed { key, detail } => {
                    warn!("OBS action on {} failed: {detail}", index_to_coord(key));
                    self.flash_error(key);
                }
            }
        }
    }

    fn on_press(&mut self, index: usize) {
        let coord = index_to_coord(index);
        let Some(key) = self.keys[index].clone() else {
            debug!("key {coord} pressed (unconfigured)");
            return;
        };
        debug!("key {coord} pressed → {}", action_name(&key));
        if let Err(e) = self.dispatch(index, &key) {
            warn!("key {coord} action failed: {e:#}");
            self.flash_error(index);
        }
    }

    /// Perform a key's action. Returns Err on failure so the caller can give
    /// failure feedback (§9); success is silent.
    fn dispatch(&mut self, index: usize, key: &KeyConfig) -> Result<()> {
        match key {
            KeyConfig::LaunchApp { app, window_class, .. } => {
                launch::launch_or_raise(app, window_class.as_deref())
            }
            KeyConfig::Macro { steps, .. } => match self.keyboard.as_mut() {
                Some(kb) => kb.play(steps),
                None => anyhow::bail!("keyboard macros are disabled (/dev/uinput not accessible)"),
            },
            KeyConfig::MicMute { .. } => {
                audio::toggle_mute(Target::Source)?;
                // Refresh immediately so the key reflects the new state now, and
                // flash the KDE OSD like a normal mic-mute key.
                self.state.mic_muted = audio::is_muted(Target::Source).ok();
                if let Some(muted) = self.state.mic_muted {
                    osd::show_mic_mute(muted);
                }
                Ok(())
            }
            KeyConfig::SystemMute { .. } => {
                audio::toggle_mute(Target::Sink)?;
                self.state.system_muted = audio::is_muted(Target::Sink).ok();
                if let Some(muted) = self.state.system_muted {
                    osd::show_system_mute(muted);
                }
                Ok(())
            }
            KeyConfig::ObsRecordStart { .. } => self.send_obs(index, ObsAction::RecordStart),
            KeyConfig::ObsRecordStop { .. } => self.send_obs(index, ObsAction::RecordStop),
            KeyConfig::ObsRecordPause { .. } => self.send_obs(index, ObsAction::RecordTogglePause),
            KeyConfig::ObsRecordToggle { .. } => self.send_obs(index, ObsAction::RecordToggle),
            KeyConfig::ObsReplayStart { .. } => self.send_obs(index, ObsAction::ReplayStart),
            KeyConfig::ObsReplayStop { .. } => self.send_obs(index, ObsAction::ReplayStop),
            KeyConfig::ObsReplayToggle { .. } => self.send_obs(index, ObsAction::ReplayToggle),
            KeyConfig::ObsReplaySave { .. } => self.send_obs(index, ObsAction::ReplaySave),
            // Widget keys aren't pressable; Sleep is handled in the main loop.
            KeyConfig::Widget { .. } | KeyConfig::Sleep { .. } => Ok(()),
        }
    }

    /// Queue an OBS command, failing fast (for §9 feedback) if OBS isn't
    /// configured or currently connected. Async command failures (e.g. saving
    /// with the buffer off) come back later as `ObsEvent::ActionFailed`.
    fn send_obs(&self, key: usize, action: ObsAction) -> Result<()> {
        let handle = self
            .obs
            .as_ref()
            .context("OBS is not configured ([obs] missing from config)")?;
        if !self.obs_connected {
            anyhow::bail!("OBS is not connected");
        }
        handle.send(ObsCommand { action, key });
        Ok(())
    }

    /// Re-read mute state for whichever targets are actually used.
    fn refresh_audio(&mut self) {
        self.last_audio_poll = Instant::now();
        if self.needs_mic {
            match audio::is_muted(Target::Source) {
                Ok(m) => self.state.mic_muted = Some(m),
                Err(e) => debug!("mic mute poll failed: {e:#}"),
            }
        }
        if self.needs_system {
            match audio::is_muted(Target::Sink) {
                Ok(m) => self.state.system_muted = Some(m),
                Err(e) => debug!("system mute poll failed: {e:#}"),
            }
        }
    }

    /// Re-upload only keys whose [`Visual`] changed since last paint.
    ///
    /// `set_key_image` only queues images in the crate's cache; the trailing
    /// `flush` is what actually sends them to the device. Without it the deck
    /// keeps showing whatever was on it (e.g. the factory logo after reset).
    fn repaint(&mut self) {
        let mut dirty = false;
        for index in 0..KEY_COUNT {
            let visual = self.visual_for(index);
            if self.last_visual[index] == Some(visual) {
                continue;
            }
            let tile = self.render_tile(index, visual);
            match self.device.set_key_image(index, render::tile_to_image(tile)) {
                Ok(()) => {
                    debug!("repaint {} → {visual:?}", index_to_coord(index));
                    self.last_visual[index] = Some(visual);
                    dirty = true;
                }
                Err(e) => debug!("failed to queue key {index}: {e:#}"),
            }
        }
        if dirty && let Err(e) = self.device.flush() {
            // The device likely went away; force a full repaint so the next pass
            // (after reconnect) re-sends everything.
            warn!("failed to flush images ({e:#}); will repaint");
            self.force_full_repaint();
        }
    }

    /// Force every key to repaint on the next pass (used after reconnect/resume).
    fn force_full_repaint(&mut self) {
        self.last_visual = [None; KEY_COUNT];
    }

    /// The desired descriptor for a key given current live state.
    fn visual_for(&self, index: usize) -> Visual {
        // A live error flash overrides whatever the key would normally show.
        if matches!(self.error_until[index], Some(t) if Instant::now() < t) {
            return Visual::Error;
        }
        match self.keys[index].as_ref() {
            None => Visual::Static, // blank, painted once
            Some(key) => match key {
                KeyConfig::LaunchApp { .. }
                | KeyConfig::Macro { .. }
                | KeyConfig::Sleep { .. } => Visual::Static,
                KeyConfig::MicMute { .. } => Visual::Mute(self.state.mic_muted),
                KeyConfig::SystemMute { .. } => Visual::Mute(self.state.system_muted),
                KeyConfig::ObsRecordStart { .. }
                | KeyConfig::ObsRecordStop { .. }
                | KeyConfig::ObsRecordPause { .. }
                | KeyConfig::ObsRecordToggle { .. } => Visual::Record(self.state.record),
                KeyConfig::ObsReplayStart { .. }
                | KeyConfig::ObsReplayStop { .. }
                | KeyConfig::ObsReplayToggle { .. }
                | KeyConfig::ObsReplaySave { .. } => Visual::Replay(self.state.replay),
                // Widgets repaint when their render generation changes.
                KeyConfig::Widget { .. } => {
                    Visual::Widget(self.widgets.get(&index).map(|w| w.generation()).unwrap_or(0))
                }
            },
        }
    }

    /// Render the tile for a key in its current visual state.
    fn render_tile(&self, index: usize, visual: Visual) -> Tile {
        if visual == Visual::Error {
            return self.error_tile.clone();
        }
        let Some(key) = self.keys[index].as_ref() else {
            return render::blank_tile();
        };
        match key {
            KeyConfig::LaunchApp { app, icon, .. } => {
                let path = icon.clone().or_else(|| launch::resolve_icon_path(app));
                opt_icon(path.as_deref())
            }
            KeyConfig::Macro { icon, .. } | KeyConfig::Sleep { icon } => opt_icon(icon.as_deref()),
            KeyConfig::MicMute { icon_muted, icon_unmuted } => {
                opt_icon(mute_icon(self.state.mic_muted, icon_muted, icon_unmuted))
            }
            KeyConfig::SystemMute { icon_muted, icon_unmuted } => {
                opt_icon(mute_icon(self.state.system_muted, icon_muted, icon_unmuted))
            }
            KeyConfig::ObsRecordStart { icons }
            | KeyConfig::ObsRecordStop { icons }
            | KeyConfig::ObsRecordPause { icons }
            | KeyConfig::ObsRecordToggle { icons } => opt_icon(record_icon(self.state.record, icons)),
            KeyConfig::ObsReplayStart { icons }
            | KeyConfig::ObsReplayStop { icons }
            | KeyConfig::ObsReplayToggle { icons }
            | KeyConfig::ObsReplaySave { icons } => opt_icon(replay_icon(self.state.replay, icons)),
            // Widget tiles are produced and cached by the widget module.
            KeyConfig::Widget { .. } => self
                .widgets
                .get(&index)
                .map(|w| w.tile())
                .unwrap_or_else(render::blank_tile),
        }
    }
}

/// Pick the mute icon for a state (muted/unmuted/unknown).
fn mute_icon<'a>(
    muted: Option<bool>,
    icon_muted: &'a Option<std::path::PathBuf>,
    icon_unmuted: &'a Option<std::path::PathBuf>,
) -> Option<&'a Path> {
    match muted {
        Some(true) => icon_muted.as_deref(),
        Some(false) => icon_unmuted.as_deref(),
        None => None, // unknown until first poll → blank
    }
}

/// Pick the recording icon for a state, falling back disconnected→stopped.
fn record_icon(state: RecordState, icons: &crate::config::RecordIcons) -> Option<&Path> {
    match state {
        RecordState::Recording => icons.icon_recording.as_deref(),
        RecordState::Paused => icons.icon_paused.as_deref(),
        RecordState::Stopped => icons.icon_stopped.as_deref(),
        RecordState::Disconnected => icons
            .icon_disconnected
            .as_deref()
            .or(icons.icon_stopped.as_deref()),
    }
}

/// Pick the replay icon for a state, falling back disconnected→disarmed.
fn replay_icon(state: ReplayState, icons: &crate::config::ReplayIcons) -> Option<&Path> {
    match state {
        ReplayState::Armed => icons.icon_armed.as_deref(),
        ReplayState::Disarmed => icons.icon_disarmed.as_deref(),
        ReplayState::Disconnected => icons
            .icon_disconnected
            .as_deref()
            .or(icons.icon_disarmed.as_deref()),
    }
}

/// Load an optional icon path, falling back to blank on absence or error.
fn opt_icon(path: Option<&Path>) -> Tile {
    match path {
        Some(p) => render::load_icon(p).unwrap_or_else(|e| {
            warn!("{e:#}");
            render::blank_tile()
        }),
        None => render::blank_tile(),
    }
}

fn build_key_map(config: &Config) -> KeyMap {
    let mut map: KeyMap = vec![None; KEY_COUNT];
    for (coord_str, key) in &config.key {
        if let Ok(coord) = KeyCoord::parse(coord_str) {
            map[coord.index()] = Some(key.clone());
        }
    }
    map
}

fn action_name(key: &KeyConfig) -> &'static str {
    match key {
        KeyConfig::LaunchApp { .. } => "launch_app",
        KeyConfig::Macro { .. } => "macro",
        KeyConfig::MicMute { .. } => "mic_mute",
        KeyConfig::SystemMute { .. } => "system_mute",
        KeyConfig::ObsRecordStart { .. } => "obs_record_start",
        KeyConfig::ObsRecordStop { .. } => "obs_record_stop",
        KeyConfig::ObsRecordPause { .. } => "obs_record_pause",
        KeyConfig::ObsRecordToggle { .. } => "obs_record_toggle",
        KeyConfig::ObsReplayStart { .. } => "obs_replay_start",
        KeyConfig::ObsReplayStop { .. } => "obs_replay_stop",
        KeyConfig::ObsReplayToggle { .. } => "obs_replay_toggle",
        KeyConfig::ObsReplaySave { .. } => "obs_replay_save",
        KeyConfig::Widget { .. } => "widget",
        KeyConfig::Sleep { .. } => "sleep",
    }
}

fn index_to_coord(index: usize) -> String {
    let row = index / (GRID_COLS as usize) + 1;
    let col = index % (GRID_COLS as usize) + 1;
    format!("r{row}c{col}")
}
