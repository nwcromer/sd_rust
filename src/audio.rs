//! Audio mute control + state via PipeWire's `wpctl` CLI (§14: no libpulse). We
//! shell out rather than link a PipeWire binding to keep deps minimal.
//!
//! State is read from `wpctl get-volume`, whose output ends in ` [MUTED]` when
//! the default sink/source is muted (verified against the installed wpctl).

use std::process::Command;

use anyhow::{bail, Context, Result};

/// Which default audio device to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Default output (speakers/headphones) → system-output mute.
    Sink,
    /// Default input (microphone) → mic mute.
    Source,
}

impl Target {
    /// The wpctl special id for this target's *default* device.
    fn id(self) -> &'static str {
        match self {
            Target::Sink => "@DEFAULT_AUDIO_SINK@",
            Target::Source => "@DEFAULT_AUDIO_SOURCE@",
        }
    }
}

/// Toggle mute on the target's default device.
pub fn toggle_mute(target: Target) -> Result<()> {
    let output = Command::new("wpctl")
        .args(["set-mute", target.id(), "toggle"])
        .output()
        .context("failed to run wpctl (is PipeWire/WirePlumber installed?)")?;
    if !output.status.success() {
        bail!(
            "wpctl set-mute {} toggle failed: {}",
            target.id(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Query whether the target's default device is currently muted.
pub fn is_muted(target: Target) -> Result<bool> {
    let output = Command::new("wpctl")
        .args(["get-volume", target.id()])
        .output()
        .context("failed to run wpctl")?;
    if !output.status.success() {
        bail!(
            "wpctl get-volume {} failed: {}",
            target.id(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_muted(&String::from_utf8_lossy(&output.stdout)))
}

/// `wpctl get-volume` prints e.g. `Volume: 1.00 [MUTED]` when muted.
fn parse_muted(output: &str) -> bool {
    output.contains("[MUTED]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_mute_marker() {
        assert!(parse_muted("Volume: 1.00 [MUTED]"));
        assert!(parse_muted("Volume: 0.50 [MUTED]\n"));
        assert!(!parse_muted("Volume: 1.00"));
        assert!(!parse_muted("Volume: 0.88\n"));
    }
}
