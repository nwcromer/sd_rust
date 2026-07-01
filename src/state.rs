//! Live state that drives state-dependent key rendering (§6). Updated by the
//! audio poller and the OBS event thread; read by the runtime's repaint pass.

/// OBS recording state (three states + a disconnected fallback for when OBS
/// isn't reachable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordState {
    Disconnected,
    Stopped,
    Recording,
    Paused,
}

/// OBS replay-buffer state (armed/running vs disarmed/stopped + disconnected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayState {
    Disconnected,
    Armed,
    Disarmed,
}

/// The full live state. `None` mute fields mean "not yet known" (before the
/// first poll); OBS fields start `Disconnected` until the OBS thread connects.
#[derive(Debug, Clone, Copy)]
pub struct LiveState {
    pub mic_muted: Option<bool>,
    pub system_muted: Option<bool>,
    pub record: RecordState,
    pub replay: ReplayState,
}

impl Default for LiveState {
    fn default() -> Self {
        LiveState {
            mic_muted: None,
            system_muted: None,
            record: RecordState::Disconnected,
            replay: ReplayState::Disconnected,
        }
    }
}
