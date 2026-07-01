//! Suspend/resume handling (§11): listen for logind's `PrepareForSleep` signal
//! and tell the main loop to repaint after the machine wakes.
//!
//! A dedicated thread subscribes to the signal over the **system** D-Bus via
//! `zbus` directly (no `gdbus` subprocess, no text parsing). The signal body is
//! a single bool — `true` = about to sleep,
//! `false` = just resumed; we forward only resumes.

use std::sync::mpsc::{self, Receiver};

use log::{info, warn};

/// Spawn the resume monitor. Sends `()` on the returned channel each time the
/// system resumes from sleep. If the system bus is unavailable the thread logs
/// and exits — suspend/resume repaint is then simply unavailable, which is not
/// fatal to the daemon.
pub fn spawn_resume_monitor() -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("sd_rust-resume".into())
        .spawn(move || {
            use zbus::blocking::{Connection, MessageIterator};
            use zbus::MatchRule;

            let conn = match Connection::system() {
                Ok(c) => c,
                Err(e) => {
                    warn!("resume monitor: failed to connect to system bus: {e}");
                    return;
                }
            };

            // Sender/interface/member are hardcoded valid D-Bus identifiers, so
            // the builder can't realistically fail; the arms are defensive.
            let rule = match MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.login1")
                .and_then(|b| b.interface("org.freedesktop.login1.Manager"))
                .and_then(|b| b.member("PrepareForSleep"))
                .map(|b| b.build())
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("resume monitor: failed to build match rule: {e}");
                    return;
                }
            };

            let iter = match MessageIterator::for_match_rule(rule, &conn, None) {
                Ok(i) => i,
                Err(e) => {
                    warn!("resume monitor: failed to subscribe to PrepareForSleep: {e}");
                    return;
                }
            };

            for msg in iter {
                let Ok(msg) = msg else { continue };
                // body is a single bool: true = going to sleep, false = resumed.
                if let Ok(going_to_sleep) = msg.body().deserialize::<bool>()
                    && !going_to_sleep
                {
                    info!("detected system resume");
                    if tx.send(()).is_err() {
                        break; // main loop gone
                    }
                }
            }
        })
        .expect("failed to spawn resume monitor thread");
    rx
}
