//! `tagui [DIR]`: open the TUI on a music library directory.

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tagui", about = "Edit music tags, browsed by album or folder")]
struct Cli {
    /// Music library directory [default: $XDG_CONFIG_HOME/tagui/library]
    dir: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let dir = match Cli::parse().dir {
        Some(dir) => dir,
        None => default_dir()?,
    };
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    crate::tui::run(dir)
}

fn default_dir() -> Result<PathBuf> {
    Ok(crate::config::dir()?.join("library"))
}
