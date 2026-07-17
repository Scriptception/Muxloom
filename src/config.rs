use std::{collections::HashSet, fs, io::Write, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub keymap: KeymapConfig,
    pub scrollback: ScrollbackConfig,
    pub notifications: NotificationConfig,
    pub lifecycle: LifecycleConfig,
    pub launchers: Vec<LaunchTemplate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub leader: String,
    pub theme: String,
    pub context_panel: bool,
    pub workspace_rail: bool,
    pub mouse: bool,
    pub prompt_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeymapConfig {
    pub toggle_mode: String,
    pub leader: String,
    pub detach: String,
    pub zoom: String,
    pub toggle_rail: String,
    pub toggle_context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleConfig {
    pub prune_exited_after_hours: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchTemplate {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
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
            workspace_rail: false,
            mouse: true,
            prompt_history: false,
        }
    }
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            toggle_mode: "ctrl-\\".into(),
            leader: "space".into(),
            detach: "d".into(),
            zoom: "z".into(),
            toggle_rail: "b".into(),
            toggle_context: "o".into(),
        }
    }
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            prune_exited_after_hours: Some(168),
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
        let config: Self = toml::from_str(&contents).context("parse Muxloom config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_atomic(&self, location: &Path) -> Result<()> {
        self.validate()?;
        let parent = location.parent().context("config path has no parent")?;
        fs::create_dir_all(parent).context("create config parent")?;
        if fs::symlink_metadata(location).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!(
                "refusing to replace symlinked config: {}",
                location.display()
            );
        }
        let temporary = parent.join(format!(".muxloom-config-{}.tmp", uuid::Uuid::new_v4()));
        let backup = location.with_extension("toml.bak");
        if location.exists() {
            fs::copy(location, &backup).context("back up existing config")?;
        }
        let rendered = toml::to_string_pretty(self).context("render Muxloom config")?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("create temporary config")?;
        file.write_all(rendered.as_bytes())
            .context("write temporary config")?;
        file.sync_all().context("sync temporary config")?;
        fs::rename(&temporary, location).context("atomically replace config")?;
        set_private(location)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let bindings = [
            ("toggle_mode", self.keymap.toggle_mode.trim()),
            ("leader", self.keymap.leader.trim()),
            ("detach", self.keymap.detach.trim()),
            ("zoom", self.keymap.zoom.trim()),
            ("toggle_rail", self.keymap.toggle_rail.trim()),
            ("toggle_context", self.keymap.toggle_context.trim()),
        ];
        let mut seen = HashSet::new();
        for (action, binding) in bindings {
            if binding.is_empty() {
                bail!("key binding for {action} cannot be empty");
            }
            let normalized = binding.to_ascii_lowercase();
            let valid = normalized == "space"
                || normalized.chars().count() == 1
                || normalized
                    .strip_prefix("ctrl-")
                    .is_some_and(|key| key.chars().count() == 1)
                || normalized
                    .strip_prefix('f')
                    .and_then(|number| number.parse::<u8>().ok())
                    .is_some_and(|number| (1..=12).contains(&number));
            if !valid {
                bail!("unknown key binding for {action}: {binding}");
            }
            if !seen.insert(binding.to_ascii_lowercase()) {
                bail!("conflicting key binding: {binding}");
            }
        }
        for launcher in &self.launchers {
            if launcher.name.trim().is_empty() || launcher.command.is_empty() {
                bail!("launcher templates require a name and argv command");
            }
        }
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
