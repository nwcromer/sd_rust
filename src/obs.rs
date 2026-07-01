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

use anyhow::{Context, Result};
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

                let outcome = run_session(&client, &mut cmd_rx, &event_tx).await;
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
) -> SessionEnd {
    // Subscribe to events BEFORE querying state so nothing fired during setup is
    // lost (obws queues stream events internally).
    let events = match client.events() {
        Ok(e) => e,
        Err(e) => return SessionEnd::Disconnected(anyhow::anyhow!("event subscribe failed: {e}")),
    };
    tokio::pin!(events);

    send_initial_state(client, event_tx).await;

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
    let result = match cmd.action {
        ObsAction::RecordStart => client.recording().start().await.map(|_| ()),
        ObsAction::RecordStop => client.recording().stop().await.map(|_| ()),
        ObsAction::RecordTogglePause => client.recording().toggle_pause().await.map(|_| ()),
        ObsAction::RecordToggle => client.recording().toggle().await.map(|_| ()),
        ObsAction::ReplayStart => client.replay_buffer().start().await.map(|_| ()),
        ObsAction::ReplayStop => client.replay_buffer().stop().await.map(|_| ()),
        ObsAction::ReplayToggle => client.replay_buffer().toggle().await.map(|_| ()),
        ObsAction::ReplaySave => client.replay_buffer().save().await.map(|_| ()),
    };
    if let Err(e) = result {
        warn!("OBS: action {:?} failed: {e}", cmd.action);
        let _ = event_tx.send(ObsEvent::ActionFailed {
            key: cmd.key,
            detail: e.to_string(),
        });
    }
    // Success is silent; the resulting state arrives via an event.
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
