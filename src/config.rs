use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub scrollback: ScrollbackConfig,
    pub notifications: NotificationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub leader: String,
    pub theme: String,
    pub context_panel: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrollbackConfig {
    pub lines: usize,
    pub persist: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub bell: bool,
    pub osc: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            leader: "ctrl-\\".into(),
            theme: "loom".into(),
            context_panel: true,
        }
    }
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            lines: 5_000,
            persist: false,
        }
    }
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            bell: true,
            osc: false,
        }
    }
}

impl Config {
    pub fn load_or_create(paths: &AppPaths) -> Result<Self> {
        let location = paths.config();
        if !location.exists() {
            let config = Self::default();
            config.save_atomic(&location)?;
            return Ok(config);
        }
        let contents = fs::read_to_string(&location).context("read Muxloom config")?;
        toml::from_str(&contents).context("parse Muxloom config")
    }

    pub fn save_atomic(&self, location: &Path) -> Result<()> {
        let parent = location.parent().context("config path has no parent")?;
        fs::create_dir_all(parent).context("create config parent")?;
        let temporary = location.with_extension("toml.tmp");
        let backup = location.with_extension("toml.bak");
        if location.exists() {
            fs::copy(location, &backup).context("back up existing config")?;
        }
        let rendered = toml::to_string_pretty(self).context("render Muxloom config")?;
        fs::write(&temporary, rendered).context("write temporary config")?;
        fs::rename(&temporary, location).context("atomically replace config")?;
        set_private(location)?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_private(target: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(target, fs::Permissions::from_mode(0o600))
        .context("set private config permissions")
}

#[cfg(not(unix))]
fn set_private(_target: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let location = directory.path().join("config.toml");
        let config = Config::default();
        config.save_atomic(&location).unwrap();
        let decoded: Config = toml::from_str(&fs::read_to_string(location).unwrap()).unwrap();
        assert_eq!(decoded.scrollback.lines, 5_000);
        assert_eq!(decoded.ui.leader, "ctrl-\\");
    }
}
