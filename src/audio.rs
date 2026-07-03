//! Audio mute control + state via a **persistent** PipeWire/PulseAudio
//! connection (libpulse), rather than spawning `wpctl` per query.
//!
//! One connection is held for the daemon's lifetime and queried in-process, so
//! reading or toggling mute is a sub-millisecond round-trip instead of forking
//! `wpctl` (a fresh process *and* a fresh server connection) on every poll.
//!
//! The libpulse standard mainloop is single-threaded (`Rc`/`RefCell`), so the
//! controller lives on the synchronous main-loop thread and is never sent across
//! threads. The query/wait machinery is ported from the sibling `pcp_rust`
//! project; we only need default-device mute, so the rest is dropped.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use libpulse_binding as pulse;
use log::info;
use pulse::callbacks::ListResult;
use pulse::context::{self, FlagSet};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::Proplist;
use pulse::time::MicroSeconds;

/// Which default audio device to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Default output (speakers/headphones) → system-output mute.
    Sink,
    /// Default input (microphone) → mic mute.
    Source,
}

impl Target {
    /// The PulseAudio special name for this target's *default* device.
    fn pa_name(self) -> &'static str {
        match self {
            Target::Sink => "@DEFAULT_SINK@",
            Target::Source => "@DEFAULT_SOURCE@",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Target::Sink => "system output",
            Target::Source => "microphone",
        }
    }
}

/// A persistent connection used to read and toggle default-device mute state.
pub struct AudioController {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<context::Context>>,
}

impl AudioController {
    /// Open a connection to the local PulseAudio / PipeWire-pulse server and
    /// wait for it to become ready.
    pub fn connect() -> Result<Self> {
        let mainloop = Rc::new(RefCell::new(
            Mainloop::new().context("failed to create PulseAudio mainloop")?,
        ));

        let mut proplist = Proplist::new().context("failed to create proplist")?;
        proplist
            .set_str(pulse::proplist::properties::APPLICATION_NAME, "sd_rust")
            .map_err(|()| anyhow::anyhow!("failed to set application name"))?;

        let context = Rc::new(RefCell::new(
            context::Context::new_with_proplist(mainloop.borrow().deref(), "sd_rust", &proplist)
                .context("failed to create PulseAudio context")?,
        ));

        context
            .borrow_mut()
            .connect(None, FlagSet::NOFLAGS, None)
            .context("failed to connect to PulseAudio")?;

        // Drive the mainloop until the context reaches Ready, bounded by a
        // wall-clock deadline so a wedged handshake can't hang the daemon. We
        // poll non-blocking + sleep rather than iterate(true), which can park in
        // poll() forever if the peer accepts the socket but never replies.
        const CONNECT_DEADLINE: Duration = Duration::from_secs(3);
        let started = Instant::now();
        loop {
            match mainloop.borrow_mut().iterate(false) {
                IterateResult::Success(_) => {}
                IterateResult::Err(e) => bail!("mainloop iterate error: {e}"),
                IterateResult::Quit(_) => bail!("mainloop quit unexpectedly"),
            }
            match context.borrow().get_state() {
                context::State::Ready => break,
                context::State::Failed | context::State::Terminated => {
                    bail!("PulseAudio connection failed");
                }
                _ if started.elapsed() >= CONNECT_DEADLINE => {
                    bail!("PulseAudio connect did not reach Ready within {CONNECT_DEADLINE:?}");
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }

        info!("connected to PulseAudio/PipeWire");
        Ok(Self { mainloop, context })
    }

    /// True while the connection is usable. Reads the state cached from the last
    /// mainloop iteration (no I/O); a server death is observed the next time a
    /// query iterates the loop (which then errors).
    pub fn is_connected(&self) -> bool {
        matches!(self.context.borrow().get_state(), context::State::Ready)
    }

    /// Current mute state of the target's default device.
    pub fn is_muted(&self, target: Target) -> Result<bool> {
        let out: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let done = Rc::new(RefCell::new(false));
        {
            let (out, done) = (out.clone(), done.clone());
            let introspect = self.context.borrow().introspect();
            // get_*_info_by_name yields one Item then End; set done on every
            // result (there is only one device) and record its mute flag.
            match target {
                Target::Sink => {
                    introspect.get_sink_info_by_name(target.pa_name(), move |r| {
                        if let ListResult::Item(s) = r {
                            *out.borrow_mut() = Some(s.mute);
                        }
                        *done.borrow_mut() = true;
                    });
                }
                Target::Source => {
                    introspect.get_source_info_by_name(target.pa_name(), move |r| {
                        if let ListResult::Item(s) = r {
                            *out.borrow_mut() = Some(s.mute);
                        }
                        *done.borrow_mut() = true;
                    });
                }
            }
        }
        self.wait_until(|| *done.borrow())?;
        (*out.borrow()).with_context(|| format!("{} not found", target.label()))
    }

    /// Toggle mute on the target's default device; returns the new mute state.
    pub fn toggle_mute(&self, target: Target) -> Result<bool> {
        let new_mute = !self.is_muted(target)?;
        self.set_mute(target, new_mute)?;
        Ok(new_mute)
    }

    /// Set mute on the target's default device, waiting for the server to
    /// acknowledge — a bare fire-and-forget write can be lost if nothing drives
    /// the mainloop before the connection is next idle/closed. The ack callback
    /// captures a clone of `result`, so a late firing (on a wait timeout) writes
    /// into still-live memory rather than dangling.
    fn set_mute(&self, target: Target, mute: bool) -> Result<()> {
        let result: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let _op = {
            let result = result.clone();
            let cb = Box::new(move |ok: bool| *result.borrow_mut() = Some(ok)) as Box<dyn FnMut(bool)>;
            let mut introspect = self.context.borrow().introspect();
            match target {
                Target::Sink => introspect.set_sink_mute_by_name(target.pa_name(), mute, Some(cb)),
                Target::Source => introspect.set_source_mute_by_name(target.pa_name(), mute, Some(cb)),
            }
        };
        self.wait_until(|| result.borrow().is_some())?;
        match *result.borrow() {
            Some(true) => Ok(()),
            _ => bail!("PulseAudio rejected set-mute on {}", target.label()),
        }
    }

    /// Drive the mainloop until `is_done`, or fail if the context drops or the
    /// deadline trips. Blocks inside `poll()` (waking the instant PA replies) but
    /// with a bounded timeout so a server that never answers can't hang us.
    fn wait_until<F: Fn() -> bool>(&self, is_done: F) -> Result<()> {
        const DEADLINE: Duration = Duration::from_millis(250);
        let started = Instant::now();
        loop {
            if is_done() {
                return Ok(());
            }
            let Some(remaining) = DEADLINE
                .checked_sub(started.elapsed())
                .filter(|r| !r.is_zero())
            else {
                bail!("PulseAudio call exceeded {DEADLINE:?} deadline");
            };
            let timeout = MicroSeconds(remaining.as_micros().min(i32::MAX as u128) as u64);
            {
                let mut ml = self.mainloop.borrow_mut();
                ml.prepare(Some(timeout))
                    .map_err(|e| anyhow::anyhow!("PulseAudio mainloop prepare failed: {e:?}"))?;
                ml.poll()
                    .map_err(|e| anyhow::anyhow!("PulseAudio mainloop poll failed: {e:?}"))?;
                ml.dispatch()
                    .map_err(|e| anyhow::anyhow!("PulseAudio mainloop dispatch failed: {e:?}"))?;
            }
            // The context can leave Ready mid-op (server restart); surface that
            // so callers don't use stale data.
            match self.context.borrow().get_state() {
                context::State::Ready => {}
                other => bail!("PulseAudio context not ready: {other:?}"),
            }
        }
    }
}

impl Drop for AudioController {
    fn drop(&mut self) {
        self.context.borrow_mut().disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_pa_names() {
        assert_eq!(Target::Sink.pa_name(), "@DEFAULT_SINK@");
        assert_eq!(Target::Source.pa_name(), "@DEFAULT_SOURCE@");
    }
}
