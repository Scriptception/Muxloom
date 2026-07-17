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
            .map(|directory| PathBuf::from(directory).join("muxloom"))
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
        ensure_private_runtime_dir(&self.runtime_dir)?;
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
    nix::unistd::geteuid().as_raw().to_string()
}

#[cfg(unix)]
fn ensure_private_runtime_dir(target: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::symlink_metadata(target) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!("unsafe Muxloom runtime path: {}", target.display());
            }
            if metadata.uid() != nix::unistd::geteuid().as_raw() {
                anyhow::bail!("Muxloom runtime directory is owned by another user");
            }
        }
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(target)
                .context("create private runtime directory")?;
        }
        Err(problem) => return Err(problem).context("inspect runtime directory"),
    }
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700))
        .context("set private runtime directory permissions")
}

#[cfg(not(unix))]
fn ensure_private_runtime_dir(target: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(target).context("create runtime directory")
}
