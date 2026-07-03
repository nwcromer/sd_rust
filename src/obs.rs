//! OBS integration over obs-websocket v5 (`obws`).
//!
//! OBS runs on a **dedicated OS thread with its own current-thread tokio
//! runtime**, so the async client never touches the synchronous Stream Deck main
//! loop. The two communicate over channels:
//!   - commands  main → OBS  (tokio mpsc, awaited in the session `select!`)
//!   - events    OBS → main  (std mpsc, drained non-blockingly each loop tick)
//!
//! Connection auto-reconnects with exponential backoff; a session that stays up
//! past a dwell threshold resets the backoff (so a user restarting OBS
//! reconnects fast, but a flapping endpoint backs off). State is event-driven
//! (so changes made in the OBS GUI are reflected), with a queried snapshot on
//! connect. While disconnected, keys show the `Disconnected` state.
//!
//! sd_rust manages the replay buffer's start/stop (in addition to recording).

use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use tokio::sync::mpsc::{self, Receiver as TokioReceiver, Sender as TokioSender};
use tokio::time::sleep;
use tokio_stream::StreamExt;

use crate::config::ObsConfig;
use crate::state::{RecordState, ReplayState};

const BACKOFF_INITIAL: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Sessions shorter than this are treated as flapping (backoff keeps growing);
/// longer ones reset it.
const STABLE_SESSION_DWELL: Duration = Duration::from_secs(30);
/// Command channel depth — commands are infrequent (button presses).
const CMD_CHANNEL_DEPTH: usize = 32;
/// Capture-liveness poll cadence. Capture state only changes on user action, so
/// a slow poll is plenty; only runs when `show_capture_status` is on.
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// One of the six OBS actions (§5), tagged with the key that triggered it so a
/// failure can be flashed back on the right key.
#[derive(Debug, Clone, Copy)]
pub struct ObsCommand {
    pub action: ObsAction,
    pub key: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum ObsAction {
    RecordStart,
    RecordStop,
    RecordTogglePause,
    RecordToggle,
    ReplayStart,
    ReplayStop,
    ReplayToggle,
    ReplaySave,
}

/// Events from the OBS thread to the main loop.
#[derive(Debug, Clone)]
pub enum ObsEvent {
    Connected,
    Disconnected,
    Record(RecordState),
    Replay(ReplayState),
    /// An action issued from `key` failed (e.g. save with the buffer off).
    ActionFailed { key: usize, detail: String },
    /// Capture-liveness poll result: `true` = OBS is capturing (or has no
    /// capture source), `false` = an enabled capture is present but blind
    /// (0x0). Only emitted when `show_capture_status` is on.
    CaptureLive(bool),
}

/// Handle for the main loop to issue commands to the OBS thread.
pub struct ObsHandle {
    cmd_tx: TokioSender<ObsCommand>,
}

impl ObsHandle {
    /// Queue a command. Non-blocking; drops (with a warning) only if the
    /// channel is somehow full, which would mean the OBS thread is wedged.
    pub fn send(&self, command: ObsCommand) {
        if let Err(e) = self.cmd_tx.try_send(command) {
            warn!("OBS: dropping command {:?} ({e})", command.action);
        }
    }
}

/// Spawn the OBS thread. Returns a command handle and the event receiver the
/// main loop drains each tick.
pub fn spawn(config: ObsConfig, password: Option<String>) -> (ObsHandle, StdReceiver<ObsEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CHANNEL_DEPTH);
    let (event_tx, event_rx) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("sd_rust-obs".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!("OBS: failed to start tokio runtime: {e}; OBS disabled");
                    return;
                }
            };
            runtime.block_on(obs_main_loop(config, password, cmd_rx, event_tx));
        })
        .expect("failed to spawn OBS thread");

    (ObsHandle { cmd_tx }, event_rx)
}

async fn obs_main_loop(
    config: ObsConfig,
    password: Option<String>,
    mut cmd_rx: TokioReceiver<ObsCommand>,
    event_tx: StdSender<ObsEvent>,
) {
    // The password is never logged. Warn (once) about cleartext-over-network if
    // the endpoint isn't local — obs-websocket is plain ws:// with no TLS.
    if password.is_some() && !host_is_local(&config.host) {
        warn!(
            "OBS: host {:?} is not local — obs-websocket is unencrypted ws://, so \
             the password is sent in cleartext. Tunnel via SSH/VPN if that matters.",
            config.host
        );
    }

    let mut backoff = BACKOFF_INITIAL;
    loop {
        match try_connect(&config, password.as_deref()).await {
            Ok(client) => {
                info!("OBS: connected to {}:{}", config.host, config.port);
                let _ = event_tx.send(ObsEvent::Connected);
                let started = Instant::now();

                let outcome =
                    run_session(&client, &mut cmd_rx, &event_tx, config.show_capture_status).await;
                let _ = event_tx.send(ObsEvent::Disconnected);

                match outcome {
                    SessionEnd::CommandChannelClosed => {
                        debug!("OBS: main loop gone; OBS thread exiting");
                        return;
                    }
                    SessionEnd::Disconnected(e) => info!("OBS: disconnected ({e})"),
                }

                if started.elapsed() >= STABLE_SESSION_DWELL {
                    backoff = BACKOFF_INITIAL;
                } else {
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
            Err(e) => {
                debug!("OBS: connect failed ({e:#}); retrying in {backoff:?}");
                sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

async fn try_connect(config: &ObsConfig, password: Option<&str>) -> Result<obws::Client> {
    obws::Client::connect(&config.host, config.port, password)
        .await
        .context("connecting to obs-websocket")
}

enum SessionEnd {
    /// The main loop dropped the command sender — time to exit the thread.
    CommandChannelClosed,
    /// Lost the OBS connection — reconnect.
    Disconnected(anyhow::Error),
}

async fn run_session(
    client: &obws::Client,
    cmd_rx: &mut TokioReceiver<ObsCommand>,
    event_tx: &StdSender<ObsEvent>,
    show_capture_status: bool,
) -> SessionEnd {
    // Subscribe to events BEFORE querying state so nothing fired during setup is
    // lost (obws queues stream events internally).
    let events = match client.events() {
        Ok(e) => e,
        Err(e) => return SessionEnd::Disconnected(anyhow::anyhow!("event subscribe failed: {e}")),
    };
    tokio::pin!(events);

    send_initial_state(client, event_tx).await;

    // Capture-liveness poll, only when the user opted in. The interval's first
    // tick fires immediately, so the indicator is correct without waiting a cycle.
    let mut capture_poll = show_capture_status.then(|| {
        let mut i = tokio::time::interval(CAPTURE_POLL_INTERVAL);
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        i
    });
    let mut last_capture_live: Option<bool> = None;

    loop {
        tokio::select! {
            // Prioritize commands so button presses win races with events.
            biased;
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => handle_command(client, cmd, event_tx).await,
                None => return SessionEnd::CommandChannelClosed,
            },
            evt = events.next() => match evt {
                Some(evt) => handle_event(evt, event_tx),
                None => return SessionEnd::Disconnected(anyhow::anyhow!("event stream ended")),
            },
            _ = async { capture_poll.as_mut().unwrap().tick().await }, if capture_poll.is_some() => {
                match scan_captures(client).await {
                    Ok((found, live)) => {
                        let capturing = !found || live;
                        if last_capture_live != Some(capturing) {
                            last_capture_live = Some(capturing);
                            let _ = event_tx.send(ObsEvent::CaptureLive(capturing));
                        }
                    }
                    Err(e) => debug!("OBS: capture poll failed: {e:#}"),
                }
            }
        }
    }
}

/// Query and emit the recording + replay-buffer state right after connecting,
/// so keys are correct even with no events yet.
async fn send_initial_state(client: &obws::Client, event_tx: &StdSender<ObsEvent>) {
    match client.recording().status().await {
        Ok(status) => {
            let state = if status.active {
                if status.paused {
                    RecordState::Paused
                } else {
                    RecordState::Recording
                }
            } else {
                RecordState::Stopped
            };
            let _ = event_tx.send(ObsEvent::Record(state));
        }
        Err(e) => debug!("OBS: initial recording status failed: {e}"),
    }

    match client.replay_buffer().status().await {
        Ok(active) => {
            let state = if active {
                ReplayState::Armed
            } else {
                ReplayState::Disarmed
            };
            let _ = event_tx.send(ObsEvent::Replay(state));
        }
        // Replay buffer may be disabled in OBS settings — not fatal.
        Err(e) => debug!("OBS: initial replay status failed: {e}"),
    }
}

async fn handle_command(client: &obws::Client, cmd: ObsCommand, event_tx: &StdSender<ObsEvent>) {
    if let Err(e) = run_action(client, cmd.action).await {
        warn!("OBS: action {:?} failed: {e:#}", cmd.action);
        let _ = event_tx.send(ObsEvent::ActionFailed {
            key: cmd.key,
            detail: e.to_string(),
        });
    }
    // Success is silent; the resulting state arrives via an event.
}

async fn run_action(client: &obws::Client, action: ObsAction) -> Result<()> {
    match action {
        // Recording *starts* are guarded so a blind capture can't silently
        // record nothing (see `ensure_capture_live`).
        ObsAction::RecordStart => {
            ensure_capture_live(client).await?;
            client.recording().start().await?;
        }
        ObsAction::RecordStop => {
            client.recording().stop().await?;
        }
        ObsAction::RecordTogglePause => {
            client.recording().toggle_pause().await?;
        }
        // Read the direction ourselves (not the atomic toggle()) so paused
        // resumes, and the capture guard runs only on the start edge.
        ObsAction::RecordToggle => {
            let status = client.recording().status().await?;
            if status.paused {
                client.recording().toggle_pause().await?; // resume, don't stop
            } else if status.active {
                client.recording().stop().await?;
            } else {
                ensure_capture_live(client).await?;
                client.recording().start().await?;
            }
        }
        ObsAction::ReplayStart => {
            client.replay_buffer().start().await?;
        }
        ObsAction::ReplayStop => {
            client.replay_buffer().stop().await?;
        }
        ObsAction::ReplayToggle => {
            client.replay_buffer().toggle().await?;
        }
        ObsAction::ReplaySave => {
            client.replay_buffer().save().await?;
        }
    }
    Ok(())
}

/// Screen/window video captures — the Linux PipeWire portal sources, whose
/// kinds contain `capture-source`. Excludes audio captures (they'd read 0x0).
fn is_capture_kind(kind: &str) -> bool {
    kind.contains("capture-source")
}

/// Record-start guard: bail (→ error flash) if OBS has an enabled screen/window
/// capture but every one is blind (0x0) — the Wayland "dismissed the
/// Share-screen picker, so OBS records nothing" case. Fail-open: with no
/// capture source present at all (static/media scene), it never blocks.
async fn ensure_capture_live(client: &obws::Client) -> Result<()> {
    // A capture can briefly read 0x0 while its portal stream (re)negotiates; not
    // worth a settle loop — the error flash just prompts a retry.
    let (found, live) = scan_captures(client).await?;
    if found && !live {
        bail!(
            "OBS isn't capturing anything — its screen/window capture is 0x0 \
             (was the \"Share screen\" picker dismissed?); refusing to record"
        );
    }
    Ok(())
}

/// Scan the current program scene for enabled screen/window captures, returning
/// `(found, live)`: whether any enabled capture is present, and whether at least
/// one is producing video. Scope is the scene's top-level items plus one level
/// of enabled groups (OBS groups can't nest); nested scenes aren't descended,
/// and disabled items are ignored since they composite nothing.
async fn scan_captures(client: &obws::Client) -> Result<(bool, bool)> {
    let scene = client
        .scenes()
        .current_program_scene()
        .await
        .context("reading OBS current program scene")?;
    let scene_id = obws::requests::scenes::SceneId::Name(&scene.id.name);

    let items = client
        .scene_items()
        .list(scene_id)
        .await
        .context("listing OBS scene items")?;

    let mut found = false;
    let mut live = false;
    for item in &items {
        if let Some(l) =
            capture_liveness(client, scene_id, item.input_kind.as_deref(), item.id).await?
        {
            found = true;
            live |= l;
        }
        // Descend into a group only if the group itself is enabled — a disabled
        // group composites none of its members.
        if item.is_group == Some(true)
            && client
                .scene_items()
                .enabled(scene_id, item.id)
                .await
                .with_context(|| format!("reading OBS group {:?} enabled state", item.source_name))?
        {
            let group_id = obws::requests::scenes::SceneId::Name(&item.source_name);
            let members = client
                .scene_items()
                .list_group(group_id)
                .await
                .with_context(|| format!("listing OBS group {:?}", item.source_name))?;
            for m in &members {
                if let Some(l) =
                    capture_liveness(client, group_id, m.input_kind.as_deref(), m.id).await?
                {
                    found = true;
                    live |= l;
                }
            }
        }
    }
    Ok((found, live))
}

/// Liveness of a scene item viewed as a capture source: `None` — not a
/// screen/window capture, or one that's disabled/hidden (ignore it);
/// `Some(true)` — an enabled capture with real (non-zero) dimensions;
/// `Some(false)` — an enabled capture reporting 0x0, i.e. no live stream.
/// `scene` is the item's containing scene or group.
async fn capture_liveness(
    client: &obws::Client,
    scene: obws::requests::scenes::SceneId<'_>,
    input_kind: Option<&str>,
    item_id: i64,
) -> Result<Option<bool>> {
    let Some(kind) = input_kind else {
        return Ok(None);
    };
    if !is_capture_kind(kind) {
        return Ok(None);
    }
    // A disabled item isn't composited, so it must not count either way (can't
    // block, can't mask a dead visible capture). Skip before reading dimensions.
    if !client
        .scene_items()
        .enabled(scene, item_id)
        .await
        .context("reading OBS scene item enabled state")?
    {
        return Ok(None);
    }
    let t = client
        .scene_items()
        .transform(scene, item_id)
        .await
        .context("reading OBS scene item transform")?;
    // Live captures report the surface's pixel size; a stream-less one is 0x0.
    // `>= 1.0` avoids float-equality pitfalls on the f32 the API returns.
    Ok(Some(t.source_width >= 1.0 && t.source_height >= 1.0))
}

fn handle_event(event: obws::events::Event, event_tx: &StdSender<ObsEvent>) {
    use obws::events::{Event, OutputState};
    match event {
        Event::RecordStateChanged { active, state, .. } => {
            let mapped = match state {
                OutputState::Started | OutputState::Resumed => RecordState::Recording,
                OutputState::Paused => RecordState::Paused,
                OutputState::Stopped => RecordState::Stopped,
                // Transient (Starting/Stopping/…): fall back to the active flag.
                _ => {
                    if active {
                        RecordState::Recording
                    } else {
                        RecordState::Stopped
                    }
                }
            };
            let _ = event_tx.send(ObsEvent::Record(mapped));
        }
        Event::ReplayBufferStateChanged { active, .. } => {
            let state = if active {
                ReplayState::Armed
            } else {
                ReplayState::Disarmed
            };
            let _ = event_tx.send(ObsEvent::Replay(state));
        }
        _ => {}
    }
}

/// Whether the OBS host is loopback (so the cleartext-password warning can be
/// suppressed for local connections).
fn host_is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_host_detection() {
        assert!(host_is_local("localhost"));
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("::1"));
        assert!(!host_is_local("192.168.1.10"));
        assert!(!host_is_local("obs.example.com"));
    }
}
