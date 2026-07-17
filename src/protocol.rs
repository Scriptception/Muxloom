use std::io::Cursor;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::model::{
    AgentState, AttentionEvent, EventConfidence, LayoutNode, ScheduleRecord, SplitAxis,
    UsageSnapshot, WorkspaceSummary,
};

pub const PROTOCOL_VERSION: u16 = 3;
pub const MIN_PROTOCOL_VERSION: u16 = 3;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const REPLAY_CHUNK_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Hello {
        min_version: u16,
        max_version: u16,
        client_version: String,
    },
    Ping {
        protocol_version: u16,
    },
    CreateWorkspace {
        name: String,
        cwd: String,
        command: Vec<String>,
        respawn: bool,
    },
    CreatePane {
        workspace_id: String,
        cwd: String,
        command: Vec<String>,
        title: Option<String>,
        split_from: Option<String>,
        split_axis: Option<SplitAxis>,
    },
    List,
    Attach {
        workspace_id: String,
        readonly: bool,
    },
    SwitchWorkspace {
        workspace_id: String,
    },
    Input {
        pane_id: String,
        data: Vec<u8>,
    },
    Resize {
        pane_id: String,
        rows: u16,
        cols: u16,
    },
    Capture {
        pane_id: String,
        lines: usize,
    },
    ClosePane {
        pane_id: String,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    DeleteWorkspace {
        workspace_id: String,
    },
    PruneExited {
        workspace_id: Option<String>,
    },
    SetLayout {
        workspace_id: String,
        layout: LayoutNode,
    },
    AgentEvent {
        pane_id: String,
        provider: String,
        state: AgentState,
        summary: String,
        confidence: EventConfidence,
    },
    ListAttention,
    MarkAttentionRead {
        id: String,
    },
    MarkAllAttentionRead,
    DismissAttention {
        id: String,
    },
    ListSchedules,
    CreateSchedule {
        schedule: ScheduleRecord,
    },
    DeleteSchedule {
        id: String,
    },
    RunSchedule {
        id: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol_version: u16,
        min_version: u16,
        server_version: String,
    },
    Pong {
        protocol_version: u16,
        version: String,
    },
    Ok,
    Error {
        message: String,
    },
    WorkspaceCreated {
        workspace: WorkspaceSummary,
    },
    StateSnapshot {
        workspaces: Vec<WorkspaceSummary>,
        attention: Vec<AttentionEvent>,
        usage: Vec<UsageSnapshot>,
    },
    Output {
        workspace_id: String,
        pane_id: String,
        data: Vec<u8>,
        sequence: u64,
    },
    PaneSnapshot {
        pane_id: String,
        data: Vec<u8>,
        sequence: u64,
        reset: bool,
    },
    PaneUpdated {
        workspace: WorkspaceSummary,
    },
    Attention {
        event: AttentionEvent,
    },
    Capture {
        pane_id: String,
        data: Vec<u8>,
    },
    Schedules {
        schedules: Vec<ScheduleRecord>,
    },
    ShutdownComplete {
        terminated_panes: usize,
    },
}

pub fn negotiate_protocol(client_min: u16, client_max: u16) -> Result<u16> {
    let negotiated = client_max.min(PROTOCOL_VERSION);
    if negotiated < client_min || negotiated < MIN_PROTOCOL_VERSION {
        bail!(
            "protocol mismatch: client supports {client_min}..={client_max}, daemon supports {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}; finish active work, stop the old daemon, and retry"
        );
    }
    Ok(negotiated)
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut encoded = Vec::new();
    ciborium::into_writer(value, &mut encoded).context("encode protocol frame")?;
    if encoded.len() > MAX_FRAME_BYTES {
        bail!("protocol frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    writer
        .write_u32(encoded.len() as u32)
        .await
        .context("write frame length")?;
    writer.write_all(&encoded).await.context("write frame")?;
    writer.flush().await.context("flush frame")?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = reader.read_u32().await.context("read frame length")? as usize;
    if length > MAX_FRAME_BYTES {
        bail!("protocol frame of {length} bytes is too large");
    }
    let mut encoded = vec![0; length];
    reader
        .read_exact(&mut encoded)
        .await
        .context("read frame")?;
    ciborium::from_reader(Cursor::new(encoded)).context("decode protocol frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move {
            write_frame(
                &mut client,
                &Request::Ping {
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .await
            .unwrap();
        });
        let request: Request = read_frame(&mut server).await.unwrap();
        write.await.unwrap();
        assert!(matches!(
            request,
            Request::Ping {
                protocol_version: PROTOCOL_VERSION
            }
        ));
    }

    #[test]
    fn incompatible_protocol_has_actionable_error() {
        let problem = negotiate_protocol(1, 2).unwrap_err().to_string();
        assert!(problem.contains("client supports 1..=2"));
        assert!(problem.contains("stop the old daemon"));
    }
}
