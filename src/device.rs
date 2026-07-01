//! Stream Deck device layer: connect, upload key images, read presses, control
//! brightness, and reconnect after unplug/replug.
//!
//! v1 is single-device: we connect to the **first** Stream Deck the
//! `elgato-streamdeck` crate enumerates. We do NOT hardcode a `Kind`/PID —
//! the unit on the target machine reports USB `0fd9:00a5`, which the crate
//! classifies as `Kind::Mk2Scissor` (Mk2 with scissor keys), *not* the classic
//! `Kind::Mk2` (`0x0080`). Both share the same 3×5 / 15-key / 72×72 geometry,
//! so we accept any deck whose grid matches and drive everything off the
//! reported `Kind`. (Single device for now; see TODO.md.)

use std::time::Duration;

use anyhow::{bail, Context, Result};
use elgato_streamdeck::{list_devices, new_hidapi, StreamDeck, StreamDeckInput};
use image::DynamicImage;
use log::{info, warn};

use crate::config::{GRID_COLS, GRID_ROWS, KEY_COUNT};

pub struct Device {
    deck: StreamDeck,
}

impl Device {
    /// Connect to the first enumerated Stream Deck and verify it has the
    /// expected 3×5 grid. Resets the device to a known (cleared) state.
    pub fn connect() -> Result<Device> {
        let hidapi = new_hidapi().context("failed to initialize HID API")?;
        let devices = list_devices(&hidapi);

        let (kind, serial) = devices
            .into_iter()
            .next()
            .context("no Stream Deck found — is it plugged in? (and are the udev rules installed?)")?;

        let deck = StreamDeck::connect(&hidapi, kind, &serial)
            .context("failed to open the Stream Deck")?;

        // The config grid is fixed at 3×5 (15 keys). Refuse a device that
        // doesn't match rather than silently mis-mapping coordinates.
        if kind.row_count() != GRID_ROWS
            || kind.column_count() != GRID_COLS
            || kind.key_count() as usize != KEY_COUNT
        {
            bail!(
                "connected Stream Deck ({:?}) has a {}x{} / {}-key layout; v1 \
                 supports only the MK.2's {}x{} / {}-key grid",
                kind,
                kind.row_count(),
                kind.column_count(),
                kind.key_count(),
                GRID_ROWS,
                GRID_COLS,
                KEY_COUNT
            );
        }

        let product = deck.product().unwrap_or_else(|_| "Stream Deck".into());
        let serial_no = deck.serial_number().unwrap_or_else(|_| "?".into());
        info!("connected to {product} ({kind:?}, serial {serial_no})");

        let device = Device { deck };
        device.deck.reset().context("failed to reset the Stream Deck")?;
        Ok(device)
    }

    /// Queue an image for a key. The crate resizes/encodes to the device's
    /// native format (72×72 JPEG for the MK.2); we pass an already-fitted tile
    /// so no aspect distortion occurs.
    ///
    /// IMPORTANT: in elgato-streamdeck 0.13 `set_button_image` only *caches* the
    /// image — nothing reaches the device until [`flush`](Self::flush) is
    /// called. Always flush after a batch of `set_key_image` calls.
    pub fn set_key_image(&self, index: usize, image: DynamicImage) -> Result<()> {
        self.deck
            .set_button_image(index as u8, image)
            .with_context(|| format!("failed to set image on key {index}"))
    }

    /// Send all queued key images to the device. No-op if nothing is queued.
    pub fn flush(&self) -> Result<()> {
        self.deck.flush().context("failed to flush images to the Stream Deck")
    }

    /// Set brightness as a percentage (0 fully blanks the panel).
    pub fn set_brightness(&self, percent: u8) -> Result<()> {
        self.deck
            .set_brightness(percent)
            .context("failed to set brightness")
    }

    /// Block up to `timeout` for input. Returns the newly-pressed key indices
    /// (rising edges only) by diffing against `prev_state`, which is updated in
    /// place. A timeout yields an empty Vec.
    pub fn poll_presses(
        &self,
        timeout: Duration,
        prev_state: &mut [bool; KEY_COUNT],
    ) -> Result<Vec<usize>> {
        let input = self
            .deck
            .read_input(Some(timeout))
            .context("failed to read Stream Deck input")?;

        let mut pressed = Vec::new();
        if let StreamDeckInput::ButtonStateChange(states) = input {
            for (i, &down) in states.iter().enumerate().take(KEY_COUNT) {
                if down && !prev_state[i] {
                    pressed.push(i);
                }
                prev_state[i] = down;
            }
        }
        Ok(pressed)
    }
}

/// Re-open the deck after a read/connect failure (unplug, hub power-cycle,
/// suspend), retrying forever with exponential backoff. Blocks the caller —
/// there's nothing to service until a deck is back (same backoff shape as the
/// OBS reconnect).
pub fn reconnect() -> Device {
    const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let mut backoff = INITIAL_BACKOFF;
    loop {
        // Sleep before every attempt (including the first) so a device that
        // opens but immediately fails can't spin the loop into a busy wait.
        std::thread::sleep(backoff);
        match Device::connect() {
            Ok(device) => {
                info!("Stream Deck reconnected");
                return device;
            }
            Err(e) => {
                warn!("Stream Deck reconnect failed ({e:#}); retrying in {backoff:?}");
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}
