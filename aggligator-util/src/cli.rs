//! Utility functions for command line utilities.

use anyhow::Context;
use std::path::PathBuf;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use aggligator::cfg::{Cfg, LinkSteering};

/// Initializes logging for command line utilities.
pub fn init_log() {
    tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
}

/// Prints the default Aggligator configuration.
pub fn print_default_cfg() {
    println!("{}", serde_json::to_string_pretty(&Cfg::default()).unwrap());
}

/// Loads an Aggligator configuration from disk or returns the default
/// configuration if the path is empty.
pub fn load_cfg(path: &Option<PathBuf>, unacked_limit: bool) -> anyhow::Result<Cfg> {
    match path {
        Some(path) => {
            let file = std::fs::File::open(path).context("cannot open configuration file")?;
            serde_json::from_reader(file).context("cannot parse configuration file")
        }
        None => {
            let mut cfg = Cfg::default();
            if unacked_limit {
                cfg.link_steering = LinkSteering::UnackedLimit(Default::default());
            }
            Ok(cfg)
        }
    }
}
