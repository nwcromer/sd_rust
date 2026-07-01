//! KDE Plasma on-screen-display popups via the `org.kde.osdService` D-Bus
//! interface. Used to flash a "Microphone: Muted" style OSD when sd_rust toggles
//! a mute, matching what KDE shows for its own mute keys.
//!
//! The OSD takes themed icon *names* (not file paths) — KDE renders them, so
//! SVG theme icons are fine here even though the deck keys are raster-only.
//!
//! A session-bus connection is cached; it's reset on failure so we recover if
//! the bus drops. Failures are swallowed — the OSD is a nice-to-have and must
//! never block the mute action.

use std::sync::{LazyLock, Mutex};

use log::debug;
use zbus::blocking::Connection;
use zbus::zvariant::DynamicType;

const DEST: &str = "org.kde.plasmashell";
const PATH: &str = "/org/kde/osdService";
const IFACE: &str = "org.kde.osdService";

static SESSION: LazyLock<Mutex<Option<Connection>>> = LazyLock::new(|| Mutex::new(None));

fn call<T>(method: &str, body: &T)
where
    T: serde::Serialize + DynamicType,
{
    // Recover from lock poisoning: the cached connection is still authoritative.
    let mut session = SESSION.lock().unwrap_or_else(|e| e.into_inner());

    if session.is_none() {
        match Connection::session() {
            Ok(c) => *session = Some(c),
            Err(e) => {
                debug!("OSD: session bus unavailable: {e}");
                return;
            }
        }
    }

    let conn = session.as_ref().expect("set above");
    if let Err(e) = conn.call_method(Some(DEST), PATH, Some(IFACE), method, body) {
        debug!("OSD: {method} failed: {e}; dropping cached connection");
        *session = None; // reconnect next time
    }
}

/// Show a text OSD with a themed icon (the form KDE uses for mute toggles).
fn show_text(icon: &str, text: &str) {
    call("showText", &(icon, text));
}

fn show_mute_status(label: &str, muted: bool, muted_icon: &str, unmuted_icon: &str) {
    let icon = if muted { muted_icon } else { unmuted_icon };
    let status = if muted { "Muted" } else { "Unmuted" };
    show_text(icon, &format!("{label}: {status}"));
}

/// Flash the mic-mute OSD.
pub fn show_mic_mute(muted: bool) {
    show_mute_status(
        "Microphone",
        muted,
        "microphone-sensitivity-muted",
        "microphone-sensitivity-high",
    );
}

/// Flash the system-output-mute OSD.
pub fn show_system_mute(muted: bool) {
    show_mute_status("Output", muted, "audio-volume-muted", "audio-volume-high");
}
