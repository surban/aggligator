//! Aggligator command line utilities.

use anyhow::{bail, Context};
use std::path::PathBuf;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use aggligator_transport_tcp::TcpLinkFilter;
use aggligator::cfg::Cfg;

/// Initializes logging for command line utilities.
pub fn init_log() {
    tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
    tracing_log::LogTracer::init().unwrap();
}

/// Prints the default Aggligator configuration.
pub fn print_default_cfg() {
    println!("{}", serde_json::to_string_pretty(&Cfg::default()).unwrap());
}

/// Loads an Aggligator configuration from disk or returns the default
/// configuration if the path is empty.
pub fn load_cfg(path: &Option<PathBuf>) -> anyhow::Result<Cfg> {
    match path {
        Some(path) => {
            let file = std::fs::File::open(path).context("cannot open configuration file")?;
            serde_json::from_reader(file).context("cannot parse configuration file")
        }
        None => Ok(Cfg::default()),
    }
}

/// Parse [TcpLinkFilter] option.
pub fn parse_tcp_link_filter(s: &str) -> anyhow::Result<TcpLinkFilter> {
    match s {
        "none" => Ok(TcpLinkFilter::None),
        "interface-interface" => Ok(TcpLinkFilter::InterfaceInterface),
        "interface-ip" => Ok(TcpLinkFilter::InterfaceIp),
        other => bail!("unknown TCP link filter: {other}"),
    }
}
