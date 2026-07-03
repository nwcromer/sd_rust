//! sd_rust — a config-driven daemon that turns an Elgato Stream Deck MK.2 into
//! a programmable control surface on Linux (Wayland/KDE). See README.md.
//!
//! v1 is intentionally lean: no hot-reload (restart to apply config), single
//! device, Wayland-only, AMD-only GPU metrics. Deferred items are tracked in
//! TODO.md.

mod audio;
mod config;
mod device;
mod keys;
mod launch;
mod obs;
mod osd;
mod prompt;
mod render;
mod runtime;
mod screensaver;
mod service;
mod state;
mod suspend;
mod udev;
mod widgets;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::info;

/// Control an Elgato Stream Deck MK.2 as a programmable control surface.
#[derive(Debug, Parser)]
#[command(name = "sd_rust", version, about)]
struct Cli {
    /// Path to the config file [default: ~/.config/sd_rust/config.toml].
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Increase log verbosity (debug-level logging).
    #[arg(long)]
    verbose: bool,

    /// Write the udev rules (Stream Deck HID + /dev/uinput) then exit. Requires
    /// root; prints the `udevadm` reload instructions.
    #[arg(long)]
    create_udev_rules: bool,

    /// Install and start the systemd **user** service `sd_rust`, then exit.
    #[arg(long)]
    install_service: bool,

    /// Stop, disable, and remove the systemd user service, then exit.
    #[arg(long)]
    remove_service: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // `--verbose` raises the default filter to debug; RUST_LOG still overrides.
    let default_level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();

    // One-shot ops subcommands: do the thing and exit. These never enter the
    // daemon loop. (Mutually exclusive in practice; first match wins.)
    if cli.create_udev_rules {
        return udev::create_udev_rules();
    }
    if cli.install_service {
        return service::install();
    }
    if cli.remove_service {
        return service::remove();
    }

    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let config_path = cli
        .config
        .clone()
        .or_else(config::default_config_path)
        .context("could not determine config path")?;

    if !config_path.exists() {
        bail!(
            "Config file not found: {}\n\
             Create one or specify a path with --config",
            config_path.display()
        );
    }

    let config = config::load_config(&config_path)?;
    info!("loaded config from {}", config_path.display());
    info!("{} key(s) mapped", config.key.len());

    runtime::run(config)
}
