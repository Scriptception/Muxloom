use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::BaseDirs;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let base = BaseDirs::new().context("cannot determine home directory")?;
        let config_dir = base.config_dir().join("muxloom");
        let state_dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| base.home_dir().join(".local/state"))
            .join("muxloom");
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/muxloom-{}", current_uid())));
        Ok(Self {
            config_dir,
            state_dir,
            runtime_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir).context("create config directory")?;
        std::fs::create_dir_all(&self.state_dir).context("create state directory")?;
        std::fs::create_dir_all(&self.runtime_dir).context("create runtime directory")?;
        set_private(&self.runtime_dir)?;
        Ok(())
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("muxloom.sock")
    }

    pub fn database(&self) -> PathBuf {
        self.state_dir.join("state.db")
    }

    pub fn log(&self) -> PathBuf {
        self.state_dir.join("muxloom.log")
    }

    pub fn config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

fn current_uid() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(unix)]
fn set_private(target: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700))
        .context("set private runtime directory permissions")
}

#[cfg(not(unix))]
fn set_private(_target: &std::path::Path) -> Result<()> {
    Ok(())
}
