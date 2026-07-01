//! Tiny interactive helpers for the one-shot ops subcommands (udev/service).
//! These run only from a terminal invocation, never in the daemon loop.

use std::io::{self, Write};

use anyhow::Result;

/// Ask the user whether to overwrite an existing file. Returns `Ok(true)` to
/// proceed. A non-interactive stdin (EOF) is treated as "no" — fail safe.
pub fn confirm_overwrite(path: &str) -> Result<bool> {
    print!("{path} already exists. Overwrite? [y/N] ");
    io::stdout().flush()?;

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        // EOF / non-interactive: don't clobber anything.
        println!();
        return Ok(false);
    }
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}
