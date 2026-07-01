//! `--create-udev-rules`: install the scoped udev rules (Stream Deck HID +
//! /dev/uinput) so the daemon can run as a normal user. Root is required only
//! for this one-time write; the daemon itself never needs it.

use std::fs;
use std::path::Path;

use anyhow::{bail, Result};

use crate::prompt::confirm_overwrite;

// Numbered 70 (not 99) on purpose: a `TAG+="uaccess"` rule must sort before
// systemd's 71-seat.rules / 73-seat-late.rules, which consume the tag and apply
// the ACL. A 99-* rule would run too late to take effect. See the rules file.
const RULES_PATH: &str = "/etc/udev/rules.d/70-sd_rust.rules";

// Single source of truth: the same file shipped at `udev/70-sd_rust.rules` is
// embedded at compile time so the installed rule and the repo copy can't drift.
const RULES_CONTENT: &str = include_str!("../udev/70-sd_rust.rules");

pub fn create_udev_rules() -> Result<()> {
    if !running_as_root() {
        bail!(
            "Creating udev rules requires root privileges.\n\
             Run again with: sudo {} --create-udev-rules",
            std::env::args().next().unwrap_or_else(|| "sd_rust".into())
        );
    }

    let path = Path::new(RULES_PATH);
    if path.exists() && !confirm_overwrite(RULES_PATH)? {
        return Ok(());
    }

    fs::write(path, RULES_CONTENT)?;
    println!("Created {RULES_PATH}");
    println!();
    println!("Reload rules and re-trigger so they apply to the already-plugged deck:");
    println!("  sudo udevadm control --reload-rules");
    println!("  sudo udevadm trigger");
    println!();
    println!("The /dev/uinput grant requires an active local (seat) login. If macros");
    println!("still can't open /dev/uinput, replug the deck / re-log-in so uaccess applies.");

    Ok(())
}

fn running_as_root() -> bool {
    // SAFETY: geteuid() reads the effective UID — no preconditions, no side
    // effects. The libc binding is `unsafe` only because all FFI is.
    unsafe { libc::geteuid() == 0 }
}
