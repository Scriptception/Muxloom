use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Working,
    WaitingApproval,
    WaitingInput,
    CompletedUnread,
    Idle,
    Error,
    Exited,
    #[default]
    Unknown,
}

impl AgentState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::WaitingApproval => "approval",
            Self::WaitingInput => "input",
            Self::CompletedUnread => "unread",
            Self::Idle => "idle",
            Self::Error => "error",
            Self::Exited => "exited",
            Self::Unknown => "unknown",
        }
    }

    pub const fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::WaitingApproval | Self::WaitingInput | Self::CompletedUnread | Self::Error
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Progress {
    Indeterminate {
        activity: String,
        since: DateTime<Utc>,
    },
    Determinate {
        completed: u32,
        total: u32,
        label: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: String,
    pub workspace_id: String,
    pub title: String,
    pub cwd: String,
    pub command: Vec<String>,
    pub provider: String,
    pub pid: Option<u32>,
    pub state: AgentState,
    pub progress: Option<Progress>,
    pub started_at: DateTime<Utc>,
    pub exited_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub exit_status: Option<i32>,
    #[serde(default)]
    pub daemon_lost: bool,
    pub unread: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum LayoutNode {
    Pane {
        pane_id: String,
    },
    Split {
        axis: SplitAxis,
        /// Percentage assigned to the first child, clamped to 10..=90.
        ratio: u8,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn pane_ids(&self, target: &mut Vec<String>) {
        match self {
            Self::Pane { pane_id } => target.push(pane_id.clone()),
            Self::Split { first, second, .. } => {
                first.pane_ids(target);
                second.pane_ids(target);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub panes: Vec<PaneSummary>,
    #[serde(default)]
    pub layout: Option<LayoutNode>,
    #[serde(default)]
    pub respawn: bool,
    #[serde(default)]
    pub schedule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionEvent {
    pub id: String,
    pub pane_id: String,
    pub provider: String,
    pub kind: AgentState,
    pub severity: u8,
    pub summary: String,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
    pub confidence: EventConfidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventConfidence {
    Native,
    Inferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub pane_id: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub elapsed_seconds: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRecord {
    pub id: String,
    pub name: String,
    pub expression: String,
    pub timezone: String,
    pub cwd: String,
    pub command: Vec<String>,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRecord {
    pub name: String,
    pub provider: String,
    pub scope: String,
    pub location: String,
    pub description: String,
    pub enabled: bool,
    pub valid: bool,
}
