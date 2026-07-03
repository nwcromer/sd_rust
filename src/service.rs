//! `--install-service` / `--remove-service`: manage the systemd **user**
//! service `sd_rust`. The daemon runs as a normal user — never root. The
//! generated unit is hardened, with a guard against path-injection in ExecStart.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::prompt::confirm_overwrite;

const SERVICE_NAME: &str = "sd_rust";

fn service_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join("systemd").join("user"))
        .context("could not determine systemd user config directory")
}

fn service_path() -> Result<PathBuf> {
    Ok(service_dir()?.join(format!("{SERVICE_NAME}.service")))
}

fn binary_path() -> Result<String> {
    let path = std::env::current_exe()
        .context("could not determine binary path")?
        .to_str()
        .map(String::from)
        .context("binary path is not valid UTF-8")?;
    check_path_for_unit_file(&path)?;
    Ok(path)
}

/// Reject binary paths that can't be safely embedded in the double-quoted
/// `ExecStart="{bin_path}"`. We refuse rather than escape — no real-world Linux
/// install puts these characters in a binary path. Pure so it can be tested.
///
///   `"`  closes the quoted string
///   `\`  is the escape character
///   `$`  triggers variable expansion
///   `%`  introduces a systemd specifier (expanded even inside quotes)
///   control chars (esp. newline) terminate the value/line, which could inject
///        arbitrary unit directives after ExecStart.
fn check_path_for_unit_file(path: &str) -> Result<()> {
    if let Some(bad) = path
        .chars()
        .find(|c| matches!(c, '"' | '\\' | '$' | '%') || c.is_control())
    {
        bail!(
            "binary path contains the disallowed character {bad:?} ({path:?}); \
             move or rename the binary to a path without `\"`, `\\`, `$`, `%`, \
             or control characters"
        );
    }
    Ok(())
}

fn generate_service_file(bin_path: &str) -> String {
    // Hardening compatible with the daemon's needs:
    // - HID I/O over /dev/hidraw* and the uinput virtual keyboard over
    //   /dev/uinput (ProtectSystem=strict leaves /dev alone; we don't set
    //   PrivateDevices because that would hide both).
    // - PipeWire's PulseAudio socket (libpulse) over a UNIX socket (AF_UNIX).
    // - OBS over a TCP socket (AF_INET / AF_INET6).
    // - System D-Bus for logind's PrepareForSleep; user D-Bus for systemd-run.
    // The daemon only *reads* the config + icon files from $HOME, so
    // ProtectHome=read-only is safe. Launched applications do NOT inherit this
    // sandbox: they are started via `systemd-run --user --scope`, which hands
    // them to the user manager outside this unit (see launch.rs).
    format!(
        "\
[Unit]
Description=Stream Deck MK.2 Controller (sd_rust)
After=graphical-session.target
Wants=graphical-session.target

[Service]
ExecStart=\"{bin_path}\"
Restart=on-failure
RestartSec=5

# Gives us a writable /run/user/<uid>/sd_rust that KWin can also read — needed
# for the launch-or-raise feature, which hands KWin a small activation script.
RuntimeDirectory=sd_rust

# Hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallFilter=@system-service
SystemCallArchitectures=native

[Install]
WantedBy=default.target
"
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("failed to run systemctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("systemctl --user {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

pub fn install() -> Result<()> {
    let path = service_path()?;
    let bin = binary_path()?;

    if path.exists() {
        if !confirm_overwrite(&path.display().to_string())? {
            return Ok(());
        }
        let _ = systemctl(&["stop", SERVICE_NAME]);
        let _ = systemctl(&["disable", SERVICE_NAME]);
    }

    let dir = service_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let content = generate_service_file(&bin);
    fs::write(&path, &content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    println!("Created {}", path.display());
    println!("Binary: {bin}");

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SERVICE_NAME])?;
    systemctl(&["start", SERVICE_NAME])?;

    println!("Service enabled and started.");
    println!();
    println!("Useful commands:");
    println!("  systemctl --user status {SERVICE_NAME}    # check status");
    println!("  journalctl --user -u {SERVICE_NAME} -f    # follow logs");
    println!("  systemctl --user restart {SERVICE_NAME}   # apply config changes");
    println!("  systemctl --user stop {SERVICE_NAME}      # stop");

    Ok(())
}

pub fn remove() -> Result<()> {
    let path = service_path()?;

    if !path.exists() {
        println!("Service is not installed.");
        return Ok(());
    }

    let _ = systemctl(&["stop", SERVICE_NAME]);
    let _ = systemctl(&["disable", SERVICE_NAME]);

    fs::remove_file(&path)
        .with_context(|| format!("failed to remove {}", path.display()))?;
    systemctl(&["daemon-reload"])?;

    println!("Service stopped, disabled, and removed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_paths() {
        assert!(check_path_for_unit_file("/home/wil/.local/bin/sd_rust").is_ok());
        assert!(check_path_for_unit_file("/usr/bin/sd_rust").is_ok());
        assert!(check_path_for_unit_file("/home/My User/bin/sd_rust").is_ok());
    }

    #[test]
    fn rejects_systemd_quoting_specials() {
        for p in ["/tmp/a\"b", "/tmp/a\\b", "/tmp/a$b", "/tmp/a%b"] {
            assert!(check_path_for_unit_file(p).is_err(), "{p:?} should be rejected");
        }
    }

    #[test]
    fn rejects_control_chars() {
        assert!(check_path_for_unit_file("/tmp/x\nExecStartPre=/evil").is_err());
        assert!(check_path_for_unit_file("/tmp/x\ty").is_err());
        assert!(check_path_for_unit_file("/tmp/x\0y").is_err());
    }

    #[test]
    fn unit_file_has_expected_directives() {
        let unit = generate_service_file("/home/me/.local/bin/sd_rust");
        assert!(unit.contains("ExecStart=\"/home/me/.local/bin/sd_rust\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=5"));
        assert!(unit.contains("WantedBy=default.target"));
        // A few hardening directives that must survive edits.
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(unit.contains("ProtectSystem=strict"));
        assert!(unit.contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6"));
    }
}
