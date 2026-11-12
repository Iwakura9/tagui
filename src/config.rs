//! Where tagui keeps its files, and the settings it remembers between runs: `key=value`
//! lines in `<config dir>/config`.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/tagui`, else `~/.config/tagui`.
pub fn dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    match base {
        Some(base) => Ok(base.join("tagui")),
        None => bail!("neither XDG_CONFIG_HOME nor HOME is set"),
    }
}

/// The saved settings; none when the file is missing or unreadable.
pub fn load() -> HashMap<String, String> {
    dir()
        .and_then(|d| Ok(std::fs::read_to_string(d.join("config"))?))
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// `key=value` lines; anything else is ignored.
pub fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub fn save(settings: &[(&str, &str)]) -> Result<()> {
    let dir = dir()?;
    let text: String = settings.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join("config"), text))
        .with_context(|| format!("writing {}", dir.join("config").display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_key_value_lines_only() {
        let s = parse("view = grid\njunk\n\nalbums=artist\n");
        assert_eq!(s.len(), 2);
        assert_eq!(
            (s["view"].as_str(), s["albums"].as_str()),
            ("grid", "artist")
        );
    }
}
