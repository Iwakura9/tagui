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
    Ok(())
}

fn default_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    match base {
        Some(base) => Ok(base.join("tagui/library")),
        None => bail!("neither XDG_CONFIG_HOME nor HOME is set"),
    }
}
