use std::{path::PathBuf, process::Command};

use crate::model::SkillRecord;

#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub provider: String,
    pub location: PathBuf,
    pub present: bool,
    pub writable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HermesSnapshot {
    pub available: bool,
    pub version: String,
    pub status: String,
    pub sessions: String,
    pub insights: String,
}

pub fn discover_skills() -> Vec<SkillRecord> {
    let Some(home) = directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf()) else {
        return Vec::new();
    };
    let roots = [
        ("codex", "user", home.join(".codex/skills")),
        ("claude", "user", home.join(".claude/skills")),
        ("hermes", "user", home.join(".hermes/skills")),
    ];
    let mut skills = Vec::new();
    for (provider, scope, root) in roots {
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() == "SKILL.md")
        {
            let location = entry.path().to_path_buf();
            let contents = std::fs::read_to_string(&location).unwrap_or_default();
            let name = frontmatter_value(&contents, "name").unwrap_or_else(|| {
                location
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("unnamed")
                    .to_string()
            });
            let description = frontmatter_value(&contents, "description").unwrap_or_default();
            skills.push(SkillRecord {
                name,
                provider: provider.into(),
                scope: scope.into(),
                location: location.display().to_string(),
                description,
                enabled: true,
                valid: contents.starts_with("---") && contents.contains("\n---"),
            });
        }
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

pub fn config_sources(muxloom: PathBuf) -> Vec<ConfigSource> {
    let Some(home) = directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf()) else {
        return Vec::new();
    };
    [
        ("muxloom", muxloom),
        ("codex", home.join(".codex/config.toml")),
        ("claude", home.join(".claude/settings.json")),
        ("hermes", home.join(".hermes/config.yaml")),
    ]
    .into_iter()
    .map(|(provider, location)| ConfigSource {
        provider: provider.into(),
        present: location.exists(),
        writable: location
            .metadata()
            .map(|metadata| !metadata.permissions().readonly())
            .unwrap_or_else(|_| location.parent().is_some_and(|parent| parent.exists())),
        location,
    })
    .collect()
}

#[allow(dead_code)]
pub fn hermes_snapshot() -> HermesSnapshot {
    let available = std::env::var_os("PATH").is_some_and(|directories| {
        std::env::split_paths(&directories).any(|directory| directory.join("hermes").is_file())
    });
    if !available {
        return HermesSnapshot::default();
    }
    HermesSnapshot {
        available: true,
        version: "installed".into(),
        status: "Use `muxloom hermes status` for a live health snapshot.".into(),
        sessions: "Use `muxloom hermes sessions` to inspect recent sessions.".into(),
        insights: "Use `muxloom hermes insights` for the seven-day report.".into(),
    }
}

pub fn git_summary(cwd: &str) -> String {
    let branch = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if branch.is_empty() {
        return "not a Git worktree".into();
    }
    let changes = Command::new("git")
        .args(["-C", cwd, "status", "--short"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout.iter().filter(|byte| **byte == b'\n').count())
        .unwrap_or(0);
    format!(
        "{branch} · {changes} change{}",
        if changes == 1 { "" } else { "s" }
    )
}

fn frontmatter_value(contents: &str, key: &str) -> Option<String> {
    contents.lines().take(30).find_map(|line| {
        line.strip_prefix(&format!("{key}:"))
            .map(|value| value.trim().trim_matches(['\'', '"']).to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_parsed() {
        let skill = "---\nname: useful-skill\ndescription: Does useful work\n---\n";
        assert_eq!(
            frontmatter_value(skill, "name").as_deref(),
            Some("useful-skill")
        );
    }
}
