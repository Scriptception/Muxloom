use std::{
    collections::{HashMap, VecDeque},
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rusqlite::{Connection, params};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, broadcast, mpsc, watch},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    model::{
        AgentState, AttentionEvent, EventConfidence, LayoutNode, PaneSummary, ScheduleRecord,
        SplitAxis, WorkspaceSummary,
    },
    paths::AppPaths,
    protocol::{
        MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, REPLAY_CHUNK_BYTES, Request, Response,
        negotiate_protocol, read_frame, write_frame,
    },
};

struct PaneRuntime {
    summary: PaneSummary,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<StdMutex<Box<dyn Write + Send>>>,
    output: Arc<StdMutex<PaneOutput>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

struct PaneOutput {
    bytes: VecDeque<u8>,
    next_sequence: u64,
    inference_tail: VecDeque<u8>,
}

struct ServerState {
    workspaces: HashMap<String, WorkspaceSummary>,
    panes: HashMap<String, PaneRuntime>,
    attention: Vec<AttentionEvent>,
    schedules: Vec<ScheduleRecord>,
    database: Database,
    config: Config,
}

#[derive(Clone)]
struct Database {
    connection: Arc<StdMutex<Connection>>,
}

impl Database {
    fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).context("open Muxloom state database")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        Ok(Self {
            connection: Arc::new(StdMutex::new(connection)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("Muxloom state database lock was poisoned"))
    }

    fn checkpoint(&self) -> Result<()> {
        self.lock()?
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

enum RuntimeEvent {
    Inference {
        pane_id: String,
        inferred: Option<(AgentState, String)>,
    },
    Exited {
        pane_id: String,
        exit_status: Option<i32>,
    },
}

pub async fn run(paths: AppPaths) -> Result<()> {
    paths.ensure()?;
    let config = Config::load_or_create(&paths)?;
    let database = initialize_database(&paths.database())?;
    if let Some(hours) = config.lifecycle.prune_exited_after_hours {
        prune_exited_before(
            &database,
            Utc::now() - chrono::Duration::hours(hours.min(i64::MAX as u64) as i64),
        )?;
    }
    let restored = restore_metadata(&database)?;
    let schedules = restore_schedules(&database)?;
    let attention = restore_attention(&database)?;

    if paths.socket().exists() {
        if UnixStream::connect(paths.socket()).await.is_ok() {
            bail!("a Muxloom daemon is already running");
        }
        std::fs::remove_file(paths.socket()).context("remove stale Muxloom socket")?;
    }
    let listener = UnixListener::bind(paths.socket()).context("bind Muxloom socket")?;
    set_socket_permissions(&paths.socket())?;

    let state = Arc::new(Mutex::new(ServerState {
        workspaces: restored
            .into_iter()
            .map(|workspace| (workspace.id.clone(), workspace))
            .collect(),
        panes: HashMap::new(),
        attention,
        schedules,
        database,
        config,
    }));
    let (updates, _) = broadcast::channel::<Response>(512);
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel::<RuntimeEvent>();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let respawns = {
        let guard = state.lock().await;
        guard
            .workspaces
            .values()
            .filter(|workspace| workspace.respawn)
            .filter_map(|workspace| {
                workspace.panes.last().map(|pane| {
                    (
                        workspace.id.clone(),
                        pane.title.clone(),
                        pane.cwd.clone(),
                        pane.command.clone(),
                    )
                })
            })
            .collect::<Vec<_>>()
    };
    for (workspace_id, title, cwd, command) in respawns {
        let output_limit = state.lock().await.config.scrollback.max_bytes;
        match spawn_pane(
            &workspace_id,
            Some(title),
            cwd,
            command,
            runtime_tx.clone(),
            updates.clone(),
            output_limit,
        ) {
            Ok(pane) => {
                let mut guard = state.lock().await;
                let database = guard.database.clone();
                if let Some(workspace) = guard.workspaces.get_mut(&workspace_id) {
                    workspace.panes.push(pane.summary.clone());
                    workspace.layout = Some(LayoutNode::Pane {
                        pane_id: pane.summary.id.clone(),
                    });
                    let updated = workspace.clone();
                    persist_workspace(&database, &updated)?;
                    persist_pane(&database, &pane.summary)?;
                    guard.panes.insert(pane.summary.id.clone(), pane);
                }
            }
            Err(problem) => error!(%problem, %workspace_id, "failed to respawn workspace command"),
        }
    }

    let event_state = Arc::clone(&state);
    let event_updates = updates.clone();
    tokio::spawn(async move {
        while let Some(event) = runtime_rx.recv().await {
            if let Err(problem) = process_runtime_event(&event_state, &event_updates, event).await {
                error!(%problem, "failed to process PTY event");
            }
        }
    });

    let scheduler_state = Arc::clone(&state);
    let scheduler_updates = updates.clone();
    let scheduler_runtime = runtime_tx.clone();
    tokio::spawn(async move {
        run_scheduler(scheduler_state, scheduler_updates, scheduler_runtime).await;
    });

    #[cfg(unix)]
    {
        let signal_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Ok(mut signal) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                signal.recv().await;
                let _ = signal_shutdown.send(true);
            }
        });
    }

    info!(socket = %paths.socket().display(), "Muxloom daemon ready");
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => Some(accepted.context("accept client")?),
            changed = shutdown_rx.changed() => { changed.ok(); None },
            signal = tokio::signal::ctrl_c() => { signal.context("listen for shutdown signal")?; None },
        };
        let Some((stream, _)) = accepted else {
            break;
        };
        let client_state = Arc::clone(&state);
        let client_updates = updates.clone();
        let client_runtime = runtime_tx.clone();
        let client_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Err(problem) = handle_client(
                stream,
                client_state,
                client_updates,
                client_runtime,
                client_shutdown,
            )
            .await
            {
                debug!(%problem, "client disconnected");
            }
        });
    }
    let terminated = graceful_shutdown(&state).await?;
    let _ = std::fs::remove_file(paths.socket());
    info!(terminated, "Muxloom daemon stopped cleanly");
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    state: Arc<Mutex<ServerState>>,
    updates: broadcast::Sender<Response>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    verify_peer(&stream)?;
    let (mut reader, mut writer) = stream.into_split();
    let hello: Request = read_frame(&mut reader).await?;
    let Request::Hello {
        min_version,
        max_version,
        ..
    } = hello
    else {
        write_frame(
            &mut writer,
            &Response::Error {
                message: "protocol handshake required before any request".into(),
            },
        )
        .await?;
        return Ok(());
    };
    let negotiated = match negotiate_protocol(min_version, max_version) {
        Ok(version) => version,
        Err(problem) => {
            write_frame(
                &mut writer,
                &Response::Error {
                    message: problem.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    write_frame(
        &mut writer,
        &Response::Hello {
            protocol_version: negotiated,
            min_version: MIN_PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
        },
    )
    .await?;
    let first: Request = read_frame(&mut reader).await?;

    if let Request::Attach {
        mut workspace_id,
        readonly,
    } = first
    {
        let mut subscription = updates.subscribe();
        let (snapshot, buffered_output) = {
            let guard = state.lock().await;
            if !guard.workspaces.contains_key(&workspace_id) {
                write_frame(
                    &mut writer,
                    &Response::Error {
                        message: format!("workspace {workspace_id} does not exist"),
                    },
                )
                .await?;
                return Ok(());
            }
            let output = guard
                .panes
                .values()
                .filter(|pane| pane.summary.workspace_id == workspace_id)
                .filter_map(|pane| {
                    pane.output.lock().ok().map(|output| {
                        (
                            pane.summary.id.clone(),
                            output.bytes.iter().copied().collect(),
                            output.next_sequence,
                        )
                    })
                })
                .collect::<Vec<(String, Vec<u8>, u64)>>();
            (snapshot_locked(&guard), output)
        };
        if let Some(snapshot) = response_for_protocol(snapshot, negotiated, None, &state).await {
            write_frame(&mut writer, &snapshot).await?;
        }
        for (pane_id, data, sequence) in buffered_output {
            write_replay(&mut writer, pane_id, data, sequence).await?;
        }
        loop {
            tokio::select! {
                incoming = read_frame::<_, Request>(&mut reader) => {
                    let request = incoming?;
                    if let Request::SwitchWorkspace { workspace_id: requested } = request {
                        let buffered_output = {
                            let guard = state.lock().await;
                            guard.workspaces.contains_key(&requested).then(|| guard.panes.values()
                                .filter(|pane| pane.summary.workspace_id == requested)
                                .filter_map(|pane| pane.output.lock().ok().map(|output| (pane.summary.id.clone(), output.bytes.iter().copied().collect(), output.next_sequence)))
                                .collect::<Vec<(String, Vec<u8>, u64)>>())
                        };
                        let Some(buffered_output) = buffered_output else {
                            write_frame(&mut writer, &Response::Error {
                                message: format!("workspace {requested} does not exist"),
                            }).await?;
                            continue;
                        };
                        workspace_id = requested;
                        write_frame(&mut writer, &Response::Ok).await?;
                        for (pane_id, data, sequence) in buffered_output {
                            write_replay(&mut writer, pane_id, data, sequence).await?;
                        }
                        continue;
                    }
                    if readonly && !readonly_request_allowed(&request) {
                        write_frame(&mut writer, &Response::Error { message: "client is attached read-only".into() }).await?;
                        continue;
                    }
                    let shutting_down = matches!(request, Request::Shutdown);
                    let compatibility_request = request.clone();
                    match dispatch(request, &state, &updates, &runtime_tx).await {
                        Ok(Some(response)) => {
                            if let Some(response) = response_for_protocol(
                                response,
                                negotiated,
                                Some(&compatibility_request),
                                &state,
                            ).await {
                                write_frame(&mut writer, &response).await?;
                            }
                            if shutting_down { let _ = shutdown.send(true); return Ok(()); }
                        }
                        Ok(None) => {}
                        Err(problem) => {
                            write_frame(&mut writer, &Response::Error {
                                message: problem.to_string(),
                            }).await?;
                        }
                    }
                }
                update = subscription.recv() => {
                    match update {
                        Ok(response) if response_matches_workspace(&response, &workspace_id, &state).await => {
                            if let Some(response) = response_for_protocol(response, negotiated, None, &state).await {
                                write_frame(&mut writer, &response).await?;
                            }
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            warn!(dropped, "client lagged; resynchronizing terminal snapshots");
                            let snapshots = {
                                let guard = state.lock().await;
                                guard.panes.values().filter(|pane| pane.summary.workspace_id == workspace_id)
                                    .filter_map(|pane| pane.output.lock().ok().map(|output| (pane.summary.id.clone(), output.bytes.iter().copied().collect(), output.next_sequence)))
                                    .collect::<Vec<(String, Vec<u8>, u64)>>()
                            };
                            for (pane_id, data, sequence) in snapshots { write_replay(&mut writer, pane_id, data, sequence).await?; }
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
    }

    let shutting_down = matches!(first, Request::Shutdown);
    let compatibility_request = first.clone();
    match dispatch(first, &state, &updates, &runtime_tx).await {
        Ok(Some(response)) => {
            if let Some(response) =
                response_for_protocol(response, negotiated, Some(&compatibility_request), &state)
                    .await
            {
                write_frame(&mut writer, &response).await?;
            }
            if shutting_down {
                let _ = shutdown.send(true);
            }
        }
        Ok(None) => {}
        Err(problem) => {
            write_frame(
                &mut writer,
                &Response::Error {
                    message: problem.to_string(),
                },
            )
            .await?;
        }
    }
    Ok(())
}

async fn response_for_protocol(
    mut response: Response,
    negotiated: u16,
    request: Option<&Request>,
    state: &Arc<Mutex<ServerState>>,
) -> Option<Response> {
    if negotiated >= 4 {
        return Some(response);
    }
    response = match response {
        Response::AttentionSnapshot { .. } if matches!(request, Some(Request::ListAttention)) => {
            let guard = state.lock().await;
            snapshot_locked(&guard)
        }
        Response::AttentionSnapshot { .. } => Response::Ok,
        Response::WorkspaceRemoved { .. } => {
            let guard = state.lock().await;
            snapshot_locked(&guard)
        }
        Response::ServerStopping => return None,
        other => other,
    };
    match &mut response {
        Response::StateSnapshot { attention, .. } => {
            for event in attention {
                downgrade_confidence(event);
            }
        }
        Response::Attention { event } => downgrade_confidence(event),
        _ => {}
    }
    Some(response)
}

fn downgrade_confidence(event: &mut AttentionEvent) {
    if matches!(
        event.confidence,
        EventConfidence::HookDerived | EventConfidence::OutputInferred
    ) {
        event.confidence = EventConfidence::Inferred;
    }
}

fn readonly_request_allowed(request: &Request) -> bool {
    matches!(
        request,
        Request::List
            | Request::Capture { .. }
            | Request::ResyncPane { .. }
            | Request::ListAttention
            | Request::ListSchedules
    )
}

async fn write_replay<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    pane_id: String,
    data: Vec<u8>,
    sequence: u64,
) -> Result<()> {
    if data.is_empty() {
        return write_frame(
            writer,
            &Response::PaneSnapshot {
                pane_id,
                data,
                sequence,
                reset: true,
            },
        )
        .await;
    }
    for (index, chunk) in data.chunks(REPLAY_CHUNK_BYTES).enumerate() {
        write_frame(
            writer,
            &Response::PaneSnapshot {
                pane_id: pane_id.clone(),
                data: chunk.to_vec(),
                sequence,
                reset: index == 0,
            },
        )
        .await?;
    }
    Ok(())
}

async fn graceful_shutdown(state: &Arc<Mutex<ServerState>>) -> Result<usize> {
    let (pids, grace) = {
        let guard = state.lock().await;
        (
            guard
                .panes
                .values()
                .filter(|pane| pane.summary.exited_at.is_none())
                .filter_map(|pane| pane.summary.pid)
                .collect::<Vec<_>>(),
            std::time::Duration::from_secs(guard.config.lifecycle.shutdown_grace_seconds.min(30)),
        )
    };
    for pid in &pids {
        signal_process_group(*pid, nix::sys::signal::Signal::SIGTERM);
    }
    wait_for_pane_exit(state, grace).await;
    {
        let mut guard = state.lock().await;
        for pane in guard.panes.values_mut() {
            if pane.summary.exited_at.is_none() {
                if let Some(pid) = pane.summary.pid {
                    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL);
                }
                let _ = pane.killer.kill();
            }
        }
    }
    wait_for_pane_exit(state, std::time::Duration::from_secs(1)).await;
    state.lock().await.database.checkpoint()?;
    Ok(pids.len())
}

fn signal_process_group(pid: u32, signal: nix::sys::signal::Signal) {
    let group = nix::unistd::Pid::from_raw(-(pid as i32));
    if nix::sys::signal::kill(group, signal).is_err() {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal);
    }
}

async fn wait_for_pane_exit(state: &Arc<Mutex<ServerState>>, timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if state
            .lock()
            .await
            .panes
            .values()
            .all(|pane| pane.summary.exited_at.is_some())
            || tokio::time::Instant::now() >= deadline
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn dispatch(
    request: Request,
    state: &Arc<Mutex<ServerState>>,
    updates: &broadcast::Sender<Response>,
    runtime_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> Result<Option<Response>> {
    let response = match request {
        Request::Ping { protocol_version } => {
            if protocol_version != PROTOCOL_VERSION {
                Response::Error {
                    message: format!(
                        "protocol mismatch: client {protocol_version}, daemon {PROTOCOL_VERSION}"
                    ),
                }
            } else {
                Response::Pong {
                    protocol_version: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").into(),
                }
            }
        }
        Request::CreateWorkspace {
            name,
            cwd,
            command,
            respawn,
        } => {
            let workspace = create_workspace(
                state,
                updates,
                runtime_tx,
                WorkspaceLaunch {
                    name,
                    cwd,
                    command,
                    schedule_id: None,
                    respawn,
                },
            )
            .await?;
            Response::WorkspaceCreated { workspace }
        }
        Request::CreatePane {
            workspace_id,
            cwd,
            command,
            title,
            split_from,
            split_axis,
        } => {
            let output_limit = state.lock().await.config.scrollback.max_bytes;
            let pane = spawn_pane(
                &workspace_id,
                title,
                cwd,
                command,
                runtime_tx.clone(),
                updates.clone(),
                output_limit,
            )?;
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let workspace = guard
                .workspaces
                .get_mut(&workspace_id)
                .ok_or_else(|| anyhow!("workspace {workspace_id} does not exist"))?;
            workspace.panes.push(pane.summary.clone());
            workspace.layout = Some(match workspace.layout.take() {
                Some(layout) => split_layout(
                    layout,
                    split_from.as_deref(),
                    pane.summary.id.clone(),
                    split_axis.unwrap_or(SplitAxis::Vertical),
                ),
                None => LayoutNode::Pane {
                    pane_id: pane.summary.id.clone(),
                },
            });
            let updated = workspace.clone();
            persist_workspace(&database, &updated)?;
            persist_pane(&database, &pane.summary)?;
            guard.panes.insert(pane.summary.id.clone(), pane);
            let response = Response::PaneUpdated { workspace: updated };
            let _ = updates.send(response.clone());
            response
        }
        Request::List => {
            let guard = state.lock().await;
            snapshot_locked(&guard)
        }
        Request::Input { pane_id, data } => {
            let writer = {
                let guard = state.lock().await;
                Arc::clone(
                    &guard
                        .panes
                        .get(&pane_id)
                        .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?
                        .writer,
                )
            };
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut stream = writer.lock().map_err(|_| anyhow!("pane writer poisoned"))?;
                stream.write_all(&data).context("write pane input")?;
                stream.flush().context("flush pane input")?;
                Ok(())
            })
            .await??;
            Response::Ok
        }
        Request::Resize {
            pane_id,
            rows,
            cols,
        } => {
            let guard = state.lock().await;
            let pane = guard
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
            pane.master
                .resize(PtySize {
                    rows: rows.max(2),
                    cols: cols.max(10),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize pane")?;
            Response::Ok
        }
        Request::Capture { pane_id, lines } => {
            let guard = state.lock().await;
            let data = guard
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.output.lock().ok())
                .map(|output| tail_lines(&output.bytes, lines))
                .unwrap_or_default();
            Response::Capture { pane_id, data }
        }
        Request::ResyncPane { pane_id } => {
            let guard = state.lock().await;
            let pane = guard
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
            let output = pane
                .output
                .lock()
                .map_err(|_| anyhow!("pane output lock was poisoned"))?;
            Response::PaneSnapshot {
                pane_id,
                data: output.bytes.iter().copied().collect(),
                sequence: output.next_sequence,
                reset: true,
            }
        }
        Request::ClosePane { pane_id } => {
            let mut guard = state.lock().await;
            let workspace_id = guard
                .panes
                .get(&pane_id)
                .map(|pane| pane.summary.workspace_id.clone())
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
            let mut pane = guard
                .panes
                .remove(&pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
            let _ = pane.killer.kill();
            let database = guard.database.clone();
            let workspace = guard.workspaces.get_mut(&workspace_id);
            if let Some(workspace) = workspace {
                workspace.panes.retain(|item| item.id != pane_id);
                workspace.layout = workspace
                    .layout
                    .take()
                    .and_then(|layout| remove_from_layout(layout, &pane_id));
                let updated = workspace.clone();
                persist_workspace(&database, &updated)?;
                mark_pane_exited(&database, &pane_id, None)?;
                let response = Response::PaneUpdated { workspace: updated };
                let _ = updates.send(response.clone());
                response
            } else {
                mark_pane_exited(&database, &pane_id, None)?;
                Response::Ok
            }
        }
        Request::RenameWorkspace { workspace_id, name } => {
            if name.trim().is_empty() {
                bail!("workspace name cannot be empty");
            }
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let workspace = guard
                .workspaces
                .get_mut(&workspace_id)
                .ok_or_else(|| anyhow!("workspace {workspace_id} does not exist"))?;
            workspace.name = name.trim().into();
            let updated = workspace.clone();
            persist_workspace(&database, &updated)?;
            let response = Response::PaneUpdated { workspace: updated };
            let _ = updates.send(response.clone());
            response
        }
        Request::DeleteWorkspace { workspace_id } => {
            let mut guard = state.lock().await;
            let workspace = guard
                .workspaces
                .remove(&workspace_id)
                .ok_or_else(|| anyhow!("workspace {workspace_id} does not exist"))?;
            for pane in &workspace.panes {
                if let Some(mut runtime) = guard.panes.remove(&pane.id) {
                    let _ = runtime.killer.kill();
                }
            }
            delete_workspace(&guard.database, &workspace_id)?;
            let response = Response::WorkspaceRemoved { workspace_id };
            let _ = updates.send(response.clone());
            response
        }
        Request::PruneExited { workspace_id } => {
            let mut guard = state.lock().await;
            for workspace in guard
                .workspaces
                .values_mut()
                .filter(|workspace| workspace_id.as_ref().is_none_or(|id| &workspace.id == id))
            {
                let removed = workspace
                    .panes
                    .iter()
                    .filter(|pane| pane.exited_at.is_some())
                    .map(|pane| pane.id.clone())
                    .collect::<Vec<_>>();
                workspace.panes.retain(|pane| pane.exited_at.is_none());
                for pane_id in removed {
                    workspace.layout = workspace
                        .layout
                        .take()
                        .and_then(|layout| remove_from_layout(layout, &pane_id));
                }
            }
            prune_exited(&guard.database, workspace_id.as_deref())?;
            Response::Ok
        }
        Request::SetLayout {
            workspace_id,
            layout,
        } => {
            let mut ids = Vec::new();
            layout.pane_ids(&mut ids);
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let workspace = guard
                .workspaces
                .get_mut(&workspace_id)
                .ok_or_else(|| anyhow!("workspace {workspace_id} does not exist"))?;
            if ids
                .iter()
                .any(|id| !workspace.panes.iter().any(|pane| &pane.id == id))
            {
                bail!("layout references an unknown pane");
            }
            workspace.layout = Some(layout);
            let updated = workspace.clone();
            persist_workspace(&database, &updated)?;
            let response = Response::PaneUpdated { workspace: updated };
            let _ = updates.send(response.clone());
            response
        }
        Request::AgentEvent {
            pane_id,
            provider,
            state: next_state,
            summary,
            confidence,
        } => {
            let mut guard = state.lock().await;
            apply_agent_event(
                &mut guard, &pane_id, &provider, next_state, &summary, confidence,
            )?;
            persist_attention(&guard.database, &guard.attention)?;
            let event = guard
                .attention
                .iter()
                .rev()
                .find(|event| event.pane_id == pane_id && event.read_at.is_none())
                .cloned();
            drop(guard);
            if let Some(event) =
                event.filter(|event| event.pane_id == pane_id && event.read_at.is_none())
            {
                let _ = updates.send(Response::Attention { event });
            }
            Response::Ok
        }
        Request::ListAttention => {
            let guard = state.lock().await;
            Response::AttentionSnapshot {
                attention: guard.attention.clone(),
            }
        }
        Request::MarkAttentionRead { id } => {
            let mut guard = state.lock().await;
            if let Some(event) = guard.attention.iter_mut().find(|event| event.id == id) {
                event.read_at = Some(Utc::now());
            }
            persist_attention(&guard.database, &guard.attention)?;
            let response = Response::AttentionSnapshot {
                attention: guard.attention.clone(),
            };
            let _ = updates.send(response.clone());
            response
        }
        Request::MarkAllAttentionRead => {
            let mut guard = state.lock().await;
            let now = Utc::now();
            for event in &mut guard.attention {
                if event.read_at.is_none() {
                    event.read_at = Some(now);
                }
            }
            persist_attention(&guard.database, &guard.attention)?;
            let response = Response::AttentionSnapshot {
                attention: guard.attention.clone(),
            };
            let _ = updates.send(response.clone());
            response
        }
        Request::DismissAttention { id } => {
            let mut guard = state.lock().await;
            guard.attention.retain(|event| event.id != id);
            persist_attention(&guard.database, &guard.attention)?;
            let response = Response::AttentionSnapshot {
                attention: guard.attention.clone(),
            };
            let _ = updates.send(response.clone());
            response
        }
        Request::ListSchedules => {
            let guard = state.lock().await;
            Response::Schedules {
                schedules: guard.schedules.clone(),
            }
        }
        Request::CreateSchedule { mut schedule } => {
            validate_schedule(&schedule)?;
            schedule.next_run_at = calculate_next_run(&schedule, Utc::now())?;
            let mut guard = state.lock().await;
            persist_schedule(&guard.database, &schedule)?;
            guard.schedules.retain(|item| item.id != schedule.id);
            guard.schedules.push(schedule);
            let response = Response::Schedules {
                schedules: guard.schedules.clone(),
            };
            let _ = updates.send(response.clone());
            response
        }
        Request::DeleteSchedule { id } => {
            let mut guard = state.lock().await;
            guard.schedules.retain(|item| item.id != id);
            delete_schedule(&guard.database, &id)?;
            let response = Response::Schedules {
                schedules: guard.schedules.clone(),
            };
            let _ = updates.send(response.clone());
            response
        }
        Request::SetScheduleEnabled { id, enabled } => {
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let schedule = guard
                .schedules
                .iter_mut()
                .find(|schedule| schedule.id == id)
                .ok_or_else(|| anyhow!("schedule {id} does not exist"))?;
            schedule.enabled = enabled;
            schedule.next_run_at = if enabled {
                calculate_next_run(schedule, Utc::now())?
            } else {
                None
            };
            persist_schedule(&database, schedule)?;
            let response = Response::Schedules {
                schedules: guard.schedules.clone(),
            };
            let _ = updates.send(response.clone());
            response
        }
        Request::RunSchedule { id } => {
            let schedule = {
                let guard = state.lock().await;
                if guard.workspaces.values().any(|workspace| {
                    workspace.schedule_id.as_deref() == Some(&id)
                        && workspace.panes.iter().any(|pane| pane.exited_at.is_none())
                }) {
                    bail!("schedule {id} already has a running workspace");
                }
                guard
                    .schedules
                    .iter()
                    .find(|item| item.id == id)
                    .cloned()
                    .ok_or_else(|| anyhow!("schedule {id} does not exist"))?
            };
            cleanup_schedule_workspaces(state, &schedule.id).await?;
            let workspace = create_workspace(
                state,
                updates,
                runtime_tx,
                WorkspaceLaunch {
                    name: format!("schedule-{}", schedule.name),
                    cwd: schedule.cwd,
                    command: schedule.command,
                    schedule_id: Some(schedule.id),
                    respawn: false,
                },
            )
            .await?;
            Response::WorkspaceCreated { workspace }
        }
        Request::Shutdown => {
            warn!("shutdown requested");
            let _ = updates.send(Response::ServerStopping);
            let terminated_panes = state
                .lock()
                .await
                .panes
                .values()
                .filter(|pane| pane.summary.exited_at.is_none())
                .count();
            Response::ShutdownComplete { terminated_panes }
        }
        Request::Hello { .. } => bail!("handshake is already complete"),
        Request::Attach { .. } => bail!("attach must be the first request on a connection"),
        Request::SwitchWorkspace { .. } => {
            bail!("workspace switching requires an attached connection")
        }
    };
    Ok(Some(response))
}

struct WorkspaceLaunch {
    name: String,
    cwd: String,
    command: Vec<String>,
    schedule_id: Option<String>,
    respawn: bool,
}

async fn create_workspace(
    state: &Arc<Mutex<ServerState>>,
    updates: &broadcast::Sender<Response>,
    runtime_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    launch: WorkspaceLaunch,
) -> Result<WorkspaceSummary> {
    let workspace_id = Uuid::new_v4().to_string();
    let mut workspace = WorkspaceSummary {
        id: workspace_id.clone(),
        name: String::new(),
        created_at: Utc::now(),
        panes: Vec::new(),
        layout: None,
        respawn: launch.respawn,
        schedule_id: launch.schedule_id,
    };
    {
        let mut guard = state.lock().await;
        workspace.name = unique_workspace_name_locked(&guard, &launch.name);
        guard
            .workspaces
            .insert(workspace_id.clone(), workspace.clone());
        persist_workspace(&guard.database, &workspace)?;
    }
    let output_limit = state.lock().await.config.scrollback.max_bytes;
    let pane = match spawn_pane(
        &workspace_id,
        None,
        launch.cwd,
        launch.command,
        runtime_tx.clone(),
        updates.clone(),
        output_limit,
    ) {
        Ok(pane) => pane,
        Err(problem) => {
            let mut guard = state.lock().await;
            guard.workspaces.remove(&workspace_id);
            let _ = delete_workspace(&guard.database, &workspace_id);
            return Err(problem);
        }
    };
    workspace.panes.push(pane.summary.clone());
    workspace.layout = Some(LayoutNode::Pane {
        pane_id: pane.summary.id.clone(),
    });
    let mut guard = state.lock().await;
    persist_workspace_with_pane(&guard.database, &workspace, &pane.summary)?;
    guard.panes.insert(pane.summary.id.clone(), pane);
    guard.workspaces.insert(workspace_id, workspace.clone());
    Ok(workspace)
}

fn spawn_pane(
    workspace_id: &str,
    title: Option<String>,
    cwd: String,
    mut command: Vec<String>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
    updates: broadcast::Sender<Response>,
    output_limit: usize,
) -> Result<PaneRuntime> {
    if command.is_empty() {
        command.push(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
        command.push("-l".into());
    }
    if !Path::new(&cwd).is_dir() {
        bail!("working directory does not exist: {cwd}");
    }

    let pane_id = Uuid::new_v4().to_string();
    let provider = detect_provider(&command[0]);
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open PTY")?;
    let mut builder = CommandBuilder::new(&command[0]);
    for argument in command.iter().skip(1) {
        builder.arg(argument);
    }
    builder.cwd(&cwd);
    builder.env("MUXLOOM_PANE_ID", &pane_id);
    builder.env("MUXLOOM_WORKSPACE_ID", workspace_id);
    builder.env("MUXLOOM_PROVIDER", &provider);
    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("spawn pane command")?;
    let pid = child.process_id();
    let killer = child.clone_killer();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
    let writer = Arc::new(StdMutex::new(
        pair.master.take_writer().context("take PTY writer")?,
    ));
    let output = Arc::new(StdMutex::new(PaneOutput {
        bytes: VecDeque::new(),
        next_sequence: 0,
        inference_tail: VecDeque::new(),
    }));
    let output_pane_id = pane_id.clone();
    let output_workspace_id = workspace_id.to_string();
    let reader_output = Arc::clone(&output);
    thread::Builder::new()
        .name(format!("muxloom-pane-{}", &pane_id[..8]))
        .spawn(move || {
            let mut buffer = [0_u8; 8192];
            let mut clean_chunks = 0_u8;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let data = buffer[..count].to_vec();
                        let (sequence, inferred) = match reader_output.lock() {
                            Ok(mut output) => {
                                output.bytes.extend(&data);
                                trim_output_buffer(&mut output.bytes, output_limit);
                                output.inference_tail.extend(&data);
                                while output.inference_tail.len() > 8 * 1024 {
                                    output.inference_tail.pop_front();
                                }
                                let inferred = infer_state(
                                    &output.inference_tail.iter().copied().collect::<Vec<_>>(),
                                );
                                let sequence = output.next_sequence;
                                output.next_sequence = output.next_sequence.saturating_add(1);
                                (sequence, inferred)
                            }
                            Err(_) => return,
                        };
                        let _ = updates.send(Response::Output {
                            workspace_id: output_workspace_id.clone(),
                            pane_id: output_pane_id.clone(),
                            data,
                            sequence,
                        });
                        match inferred {
                            Some(inferred) => {
                                clean_chunks = 0;
                                let _ = runtime_tx.send(RuntimeEvent::Inference {
                                    pane_id: output_pane_id.clone(),
                                    inferred: Some(inferred),
                                });
                            }
                            None => {
                                clean_chunks = clean_chunks.saturating_add(1);
                                if clean_chunks == 2 {
                                    let _ = runtime_tx.send(RuntimeEvent::Inference {
                                        pane_id: output_pane_id.clone(),
                                        inferred: None,
                                    });
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let exit_status = child.wait().ok().map(|status| status.exit_code() as i32);
            let _ = runtime_tx.send(RuntimeEvent::Exited {
                pane_id: output_pane_id,
                exit_status,
            });
        })
        .context("start PTY reader")?;

    let display_title = title.unwrap_or_else(|| {
        Path::new(&command[0])
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shell")
            .to_string()
    });
    let summary = PaneSummary {
        id: pane_id,
        workspace_id: workspace_id.into(),
        title: display_title,
        cwd,
        command,
        provider,
        pid,
        state: AgentState::Starting,
        progress: None,
        started_at: Utc::now(),
        exited_at: None,
        exit_status: None,
        daemon_lost: false,
        unread: false,
    };
    Ok(PaneRuntime {
        summary,
        master: pair.master,
        writer,
        output,
        killer,
    })
}

async fn process_runtime_event(
    state: &Arc<Mutex<ServerState>>,
    updates: &broadcast::Sender<Response>,
    event: RuntimeEvent,
) -> Result<()> {
    match event {
        RuntimeEvent::Inference { pane_id, inferred } => {
            let mut guard = state.lock().await;
            let Some((workspace_id, provider, current_state)) =
                guard.panes.get(&pane_id).map(|pane| {
                    (
                        pane.summary.workspace_id.clone(),
                        pane.summary.provider.clone(),
                        pane.summary.state,
                    )
                })
            else {
                return Ok(());
            };
            if let Some((next_state, summary)) = inferred {
                apply_agent_event(
                    &mut guard,
                    &pane_id,
                    &provider,
                    next_state,
                    &summary,
                    EventConfidence::OutputInferred,
                )?;
            } else if matches!(
                current_state,
                AgentState::Starting
                    | AgentState::Error
                    | AgentState::WaitingInput
                    | AgentState::WaitingApproval
            ) && let Some(pane) = guard.panes.get_mut(&pane_id)
            {
                pane.summary.state = AgentState::Working;
                pane.summary.unread = false;
            }
            sync_workspace_panes(&mut guard, &workspace_id);
            persist_attention(&guard.database, &guard.attention)?;
            if let Some(workspace) = guard.workspaces.get(&workspace_id).cloned() {
                let _ = updates.send(Response::PaneUpdated { workspace });
            }
        }
        RuntimeEvent::Exited {
            pane_id,
            exit_status,
        } => {
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let mut workspace_id = None;
            let mut attention_details = None;
            if let Some(pane) = guard.panes.get_mut(&pane_id) {
                workspace_id = Some(pane.summary.workspace_id.clone());
                pane.summary.state = AgentState::CompletedUnread;
                pane.summary.exited_at = Some(Utc::now());
                pane.summary.exit_status = exit_status;
                pane.summary.unread = true;
                attention_details =
                    Some((pane.summary.provider.clone(), pane.summary.title.clone()));
            }
            if let Some((provider, title)) = attention_details {
                push_attention(
                    &mut guard,
                    &pane_id,
                    &provider,
                    AgentState::CompletedUnread,
                    format!("{title} finished"),
                    EventConfidence::Native,
                );
            }
            mark_pane_exited(&database, &pane_id, exit_status)?;
            if let Some(workspace_id) = workspace_id {
                sync_workspace_panes(&mut guard, &workspace_id);
                let scheduled_done = guard
                    .workspaces
                    .get(&workspace_id)
                    .is_some_and(|workspace| {
                        workspace.schedule_id.is_some()
                            && workspace.panes.iter().all(|pane| pane.exited_at.is_some())
                    });
                if scheduled_done {
                    guard.workspaces.remove(&workspace_id);
                    guard
                        .panes
                        .retain(|_, pane| pane.summary.workspace_id != workspace_id);
                    delete_workspace(&database, &workspace_id)?;
                    let _ = updates.send(Response::WorkspaceRemoved { workspace_id });
                } else if let Some(workspace) = guard.workspaces.get(&workspace_id).cloned() {
                    let _ = updates.send(Response::PaneUpdated { workspace });
                }
            }
            if let Some(event) = guard
                .attention
                .iter()
                .rev()
                .find(|event| {
                    event.pane_id == pane_id
                        && event.kind == AgentState::CompletedUnread
                        && event.read_at.is_none()
                })
                .cloned()
            {
                let _ = updates.send(Response::Attention { event });
            }
            persist_attention(&guard.database, &guard.attention)?;
        }
    }
    Ok(())
}

fn apply_agent_event(
    state: &mut ServerState,
    pane_id: &str,
    provider: &str,
    next_state: AgentState,
    summary: &str,
    confidence: EventConfidence,
) -> Result<()> {
    let workspace_id = {
        let pane = state
            .panes
            .get_mut(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
        pane.summary.state = next_state;
        pane.summary.unread = next_state.needs_attention();
        pane.summary.workspace_id.clone()
    };
    sync_workspace_panes(state, &workspace_id);
    if next_state.needs_attention() {
        push_attention(
            state,
            pane_id,
            provider,
            next_state,
            summary.to_string(),
            confidence,
        );
    }
    Ok(())
}

fn push_attention(
    state: &mut ServerState,
    pane_id: &str,
    provider: &str,
    kind: AgentState,
    summary: String,
    confidence: EventConfidence,
) {
    if let Some(existing) = state
        .attention
        .iter_mut()
        .rev()
        .find(|event| event.pane_id == pane_id && event.kind == kind && event.read_at.is_none())
    {
        existing.summary = summary;
        existing.created_at = Utc::now();
        existing.confidence = confidence;
        return;
    }
    state.attention.push(AttentionEvent {
        id: Uuid::new_v4().to_string(),
        pane_id: pane_id.into(),
        provider: provider.into(),
        kind,
        severity: match kind {
            AgentState::Error => 3,
            AgentState::WaitingApproval => 2,
            _ => 1,
        },
        summary,
        created_at: Utc::now(),
        read_at: None,
        confidence,
    });
}

fn sync_workspace_panes(state: &mut ServerState, workspace_id: &str) {
    if let Some(workspace) = state.workspaces.get_mut(workspace_id) {
        for summary in &mut workspace.panes {
            if let Some(runtime) = state.panes.get(&summary.id) {
                *summary = runtime.summary.clone();
            }
        }
    }
}

fn snapshot_locked(state: &ServerState) -> Response {
    let mut workspaces = state.workspaces.values().cloned().collect::<Vec<_>>();
    workspaces.sort_by_key(|workspace| workspace.created_at);
    Response::StateSnapshot {
        workspaces,
        attention: state.attention.clone(),
        usage: Vec::new(),
    }
}

async fn response_matches_workspace(
    response: &Response,
    workspace_id: &str,
    state: &Arc<Mutex<ServerState>>,
) -> bool {
    match response {
        Response::Output {
            workspace_id: output_workspace,
            ..
        } => output_workspace == workspace_id,
        Response::Capture { pane_id, .. } => state
            .lock()
            .await
            .panes
            .get(pane_id)
            .is_some_and(|pane| pane.summary.workspace_id == workspace_id),
        Response::PaneUpdated { workspace } => workspace.id == workspace_id,
        Response::Attention { event } => state
            .lock()
            .await
            .panes
            .get(&event.pane_id)
            .is_some_and(|pane| pane.summary.workspace_id == workspace_id),
        _ => true,
    }
}

fn unique_workspace_name_locked(state: &ServerState, requested: &str) -> String {
    let base = if requested.trim().is_empty() {
        "workspace"
    } else {
        requested.trim()
    };
    if !state
        .workspaces
        .values()
        .any(|workspace| workspace.name == base)
    {
        return base.into();
    }
    for number in 2..10_000 {
        let candidate = format!("{base}-{number}");
        if !state
            .workspaces
            .values()
            .any(|workspace| workspace.name == candidate)
        {
            return candidate;
        }
    }
    format!("{base}-{}", Uuid::new_v4().simple())
}

fn split_layout(
    layout: LayoutNode,
    target: Option<&str>,
    pane_id: String,
    axis: SplitAxis,
) -> LayoutNode {
    match layout {
        LayoutNode::Pane { pane_id: existing }
            if target.is_none_or(|target| target == existing) =>
        {
            LayoutNode::Split {
                axis,
                ratio: 50,
                first: Box::new(LayoutNode::Pane { pane_id: existing }),
                second: Box::new(LayoutNode::Pane { pane_id }),
            }
        }
        LayoutNode::Split {
            axis: current_axis,
            ratio,
            first,
            second,
        } => {
            let first_contains = target.is_some_and(|target| {
                let mut ids = Vec::new();
                first.pane_ids(&mut ids);
                ids.iter().any(|id| id == target)
            });
            if first_contains {
                LayoutNode::Split {
                    axis: current_axis,
                    ratio,
                    first: Box::new(split_layout(*first, target, pane_id, axis)),
                    second,
                }
            } else {
                LayoutNode::Split {
                    axis: current_axis,
                    ratio,
                    first,
                    second: Box::new(split_layout(*second, target, pane_id, axis)),
                }
            }
        }
        node => LayoutNode::Split {
            axis,
            ratio: 50,
            first: Box::new(node),
            second: Box::new(LayoutNode::Pane { pane_id }),
        },
    }
}

fn remove_from_layout(layout: LayoutNode, pane_id: &str) -> Option<LayoutNode> {
    match layout {
        LayoutNode::Pane { pane_id: current } => {
            (current != pane_id).then_some(LayoutNode::Pane { pane_id: current })
        }
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => match (
            remove_from_layout(*first, pane_id),
            remove_from_layout(*second, pane_id),
        ) {
            (Some(first), Some(second)) => Some(LayoutNode::Split {
                axis,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(node), None) | (None, Some(node)) => Some(node),
            (None, None) => None,
        },
    }
}

fn detect_provider(command: &str) -> String {
    let name = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    if name.contains("codex") {
        "codex".into()
    } else if name.contains("claude") {
        "claude".into()
    } else if name.contains("hermes") {
        "hermes".into()
    } else {
        "generic".into()
    }
}

fn infer_state(data: &[u8]) -> Option<(AgentState, String)> {
    let text = String::from_utf8_lossy(data).to_ascii_lowercase();
    if text.contains("permission") || text.contains("approve this") {
        Some((
            AgentState::WaitingApproval,
            "Agent is requesting approval".into(),
        ))
    } else if text.contains("error:") || text.contains("failed:") {
        Some((AgentState::Error, "Agent output contains an error".into()))
    } else if text.contains("waiting for input") || text.contains("what would you like") {
        Some((
            AgentState::WaitingInput,
            "Agent is waiting for input".into(),
        ))
    } else {
        None
    }
}

fn trim_output_buffer(output: &mut VecDeque<u8>, limit: usize) {
    while output.len() > limit {
        let excess = output.len() - limit;
        if let Some(newline) = output.iter().take(excess).position(|byte| *byte == b'\n') {
            for _ in 0..=newline {
                output.pop_front();
            }
            continue;
        }
        for _ in 0..excess {
            output.pop_front();
        }
        while output
            .front()
            .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
        {
            output.pop_front();
        }
        // A single line exceeded the cap. Reset terminal state before replaying its tail so a
        // snapshot never begins in a stale escape sequence.
        output.push_front(b'c');
        output.push_front(0x1b);
        while output.len() > limit {
            output.pop_back();
        }
    }
}

fn tail_lines(output: &VecDeque<u8>, lines: usize) -> Vec<u8> {
    let bytes = output.iter().copied().collect::<Vec<_>>();
    if lines == 0 {
        return Vec::new();
    }
    let separators = bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'\n' && index + 1 < bytes.len()).then_some(index))
        .collect::<Vec<_>>();
    let start = separators
        .iter()
        .rev()
        .nth(lines.saturating_sub(1))
        .map_or(0, |index| index + 1);
    bytes[start..].to_vec()
}

fn validate_schedule(schedule: &ScheduleRecord) -> Result<()> {
    if schedule.command.is_empty() {
        bail!("schedule command cannot be empty");
    }
    schedule
        .expression
        .parse::<cron::Schedule>()
        .context("invalid cron expression")?;
    if !Path::new(&schedule.cwd).is_dir() {
        bail!("schedule working directory does not exist");
    }
    Ok(())
}

fn calculate_next_run(
    schedule: &ScheduleRecord,
    after: chrono::DateTime<Utc>,
) -> Result<Option<chrono::DateTime<Utc>>> {
    let expression = schedule
        .expression
        .parse::<cron::Schedule>()
        .context("invalid cron expression")?;
    if schedule.timezone.eq_ignore_ascii_case("local") {
        return Ok(expression
            .after(&after.with_timezone(&chrono::Local))
            .next()
            .map(|next| next.with_timezone(&Utc)));
    }
    let timezone = schedule
        .timezone
        .parse::<chrono_tz::Tz>()
        .with_context(|| format!("invalid IANA timezone: {}", schedule.timezone))?;
    Ok(expression
        .after(&after.with_timezone(&timezone))
        .next()
        .map(|next| next.with_timezone(&Utc)))
}

async fn run_scheduler(
    state: Arc<Mutex<ServerState>>,
    updates: broadcast::Sender<Response>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        ticker.tick().await;
        let now = Utc::now();
        let due = {
            let mut guard = state.lock().await;
            let active_schedule_ids = guard
                .workspaces
                .values()
                .filter(|workspace| {
                    workspace.panes.iter().any(|pane| {
                        !matches!(pane.state, AgentState::Exited | AgentState::CompletedUnread)
                    })
                })
                .filter_map(|workspace| workspace.schedule_id.clone())
                .collect::<Vec<_>>();
            let database = guard.database.clone();
            let mut due = Vec::new();
            for schedule in &mut guard.schedules {
                if !schedule.enabled || schedule.next_run_at.is_none_or(|next| next > now) {
                    continue;
                }
                schedule.last_run_at = Some(now);
                match calculate_next_run(schedule, now) {
                    Ok(next) => schedule.next_run_at = next,
                    Err(problem) => {
                        warn!(%problem, schedule = %schedule.name, "disabled invalid schedule");
                        schedule.enabled = false;
                    }
                }
                if let Err(problem) = persist_schedule(&database, schedule) {
                    error!(%problem, schedule = %schedule.name, "failed to persist scheduler tick");
                }
                if !active_schedule_ids.contains(&schedule.id) {
                    due.push(schedule.clone());
                }
            }
            due
        };
        for schedule in due {
            if let Err(problem) = cleanup_schedule_workspaces(&state, &schedule.id).await {
                error!(%problem, schedule = %schedule.name, "failed to clean previous schedule workspace");
                continue;
            }
            let schedule_id = schedule.id.clone();
            match create_workspace(
                &state,
                &updates,
                &runtime_tx,
                WorkspaceLaunch {
                    name: format!("schedule-{}", schedule.name),
                    cwd: schedule.cwd,
                    command: schedule.command,
                    schedule_id: Some(schedule_id),
                    respawn: false,
                },
            )
            .await
            {
                Ok(workspace) => {
                    let _ = updates.send(Response::PaneUpdated { workspace });
                }
                Err(problem) => error!(%problem, "scheduled workspace failed to start"),
            }
        }
    }
}

async fn cleanup_schedule_workspaces(
    state: &Arc<Mutex<ServerState>>,
    schedule_id: &str,
) -> Result<()> {
    let mut guard = state.lock().await;
    let stale = guard
        .workspaces
        .values()
        .filter(|workspace| {
            workspace.schedule_id.as_deref() == Some(schedule_id)
                && workspace.panes.iter().all(|pane| pane.exited_at.is_some())
        })
        .map(|workspace| workspace.id.clone())
        .collect::<Vec<_>>();
    for workspace_id in stale {
        guard.workspaces.remove(&workspace_id);
        delete_workspace(&guard.database, &workspace_id)?;
    }
    Ok(())
}

fn initialize_database(path: &Path) -> Result<Database> {
    let database = Database::open(path)?;
    let connection = database.lock()?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS workspaces (
           id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           created_at TEXT NOT NULL,
           layout_json TEXT,
           respawn INTEGER NOT NULL DEFAULT 0,
           schedule_id TEXT
         );
         CREATE TABLE IF NOT EXISTS panes (
           id TEXT PRIMARY KEY,
           workspace_id TEXT NOT NULL,
           title TEXT NOT NULL,
           cwd TEXT NOT NULL,
           command_json TEXT NOT NULL,
           provider TEXT NOT NULL,
           started_at TEXT NOT NULL,
           exited_at TEXT,
           pid INTEGER,
           exit_status INTEGER,
           daemon_lost INTEGER NOT NULL DEFAULT 0,
           FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
         );
         CREATE TABLE IF NOT EXISTS schedules (
           id TEXT PRIMARY KEY,
           record_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS attention (
           id TEXT PRIMARY KEY,
           record_json TEXT NOT NULL
         );",
    )?;
    for (table, column, definition) in [
        ("workspaces", "layout_json", "TEXT"),
        ("workspaces", "respawn", "INTEGER NOT NULL DEFAULT 0"),
        ("workspaces", "schedule_id", "TEXT"),
        ("panes", "pid", "INTEGER"),
        ("panes", "exit_status", "INTEGER"),
        ("panes", "daemon_lost", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !database_column_exists(&connection, table, column)? {
            connection.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition};"
            ))?;
        }
    }
    connection.pragma_update(None, "user_version", 3)?;
    drop(connection);
    Ok(database)
}

fn database_column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn persist_workspace(database: &Database, workspace: &WorkspaceSummary) -> Result<()> {
    let connection = database.lock()?;
    connection.execute(
        "INSERT OR REPLACE INTO workspaces (id, name, created_at, layout_json, respawn, schedule_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            workspace.id,
            workspace.name,
            workspace.created_at.to_rfc3339(),
            workspace.layout.as_ref().map(serde_json::to_string).transpose()?,
            workspace.respawn,
            workspace.schedule_id,
        ],
    )?;
    Ok(())
}

fn persist_pane(database: &Database, pane: &PaneSummary) -> Result<()> {
    let connection = database.lock()?;
    connection.execute(
        "INSERT OR REPLACE INTO panes
         (id, workspace_id, title, cwd, command_json, provider, started_at, exited_at, pid, exit_status, daemon_lost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            pane.id,
            pane.workspace_id,
            pane.title,
            pane.cwd,
            serde_json::to_string(&pane.command)?,
            pane.provider,
            pane.started_at.to_rfc3339(),
            pane.exited_at.map(|value| value.to_rfc3339()),
            pane.pid,
            pane.exit_status,
            pane.daemon_lost,
        ],
    )?;
    Ok(())
}

fn persist_workspace_with_pane(
    database: &Database,
    workspace: &WorkspaceSummary,
    pane: &PaneSummary,
) -> Result<()> {
    let mut connection = database.lock()?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT OR REPLACE INTO workspaces (id, name, created_at, layout_json, respawn, schedule_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![workspace.id, workspace.name, workspace.created_at.to_rfc3339(), workspace.layout.as_ref().map(serde_json::to_string).transpose()?, workspace.respawn, workspace.schedule_id],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO panes (id, workspace_id, title, cwd, command_json, provider, started_at, exited_at, pid, exit_status, daemon_lost) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![pane.id, pane.workspace_id, pane.title, pane.cwd, serde_json::to_string(&pane.command)?, pane.provider, pane.started_at.to_rfc3339(), pane.exited_at.map(|value| value.to_rfc3339()), pane.pid, pane.exit_status, pane.daemon_lost],
    )?;
    transaction.commit()?;
    Ok(())
}

fn mark_pane_exited(database: &Database, pane_id: &str, exit_status: Option<i32>) -> Result<()> {
    let connection = database.lock()?;
    connection.execute(
        "UPDATE panes SET exited_at = ?1, exit_status = ?2, daemon_lost = 0 WHERE id = ?3",
        params![Utc::now().to_rfc3339(), exit_status, pane_id],
    )?;
    Ok(())
}

fn restore_metadata(database: &Database) -> Result<Vec<WorkspaceSummary>> {
    let connection = database.lock()?;
    let mut statement = connection.prepare(
        "SELECT id, name, created_at, layout_json, respawn, schedule_id FROM workspaces",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, bool>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut workspaces = Vec::new();
    for row in rows {
        let (id, name, created_at, layout_json, respawn, schedule_id) = row?;
        workspaces.push(WorkspaceSummary {
            id,
            name,
            created_at: chrono::DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
            panes: Vec::new(),
            layout: layout_json.and_then(|json| serde_json::from_str(&json).ok()),
            respawn,
            schedule_id,
        });
    }
    let mut pane_statement = connection.prepare(
        "SELECT id, workspace_id, title, cwd, command_json, provider, started_at, exited_at, pid, exit_status, daemon_lost FROM panes",
    )?;
    let panes = pane_statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<u32>>(8)?,
            row.get::<_, Option<i32>>(9)?,
            row.get::<_, bool>(10)?,
        ))
    })?;
    for pane in panes {
        let (
            id,
            workspace_id,
            title,
            cwd,
            command_json,
            provider,
            started_at,
            exited_at,
            pid,
            exit_status,
            persisted_lost,
        ) = pane?;
        let parsed_exit = exited_at
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc));
        let daemon_lost = persisted_lost || parsed_exit.is_none();
        let started_at = chrono::DateTime::parse_from_rfc3339(&started_at)?.with_timezone(&Utc);
        let verified_pid = pid.filter(|pid| process_matches_start(*pid, started_at));
        let summary = PaneSummary {
            id,
            workspace_id: workspace_id.clone(),
            title,
            cwd,
            command: serde_json::from_str(&command_json).unwrap_or_default(),
            provider,
            pid: verified_pid,
            state: if parsed_exit.is_some() {
                AgentState::Exited
            } else {
                AgentState::Unknown
            },
            progress: None,
            started_at,
            exited_at: parsed_exit,
            exit_status,
            daemon_lost,
            unread: false,
        };
        if let Some(workspace) = workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        {
            workspace.panes.push(summary);
        }
    }
    Ok(workspaces)
}

fn process_matches_start(pid: u32, started_at: chrono::DateTime<Utc>) -> bool {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
    system
        .process(Pid::from_u32(pid))
        .is_some_and(|process| (process.start_time() as i64 - started_at.timestamp()).abs() <= 5)
}

fn persist_schedule(database: &Database, schedule: &ScheduleRecord) -> Result<()> {
    let connection = database.lock()?;
    connection.execute(
        "INSERT OR REPLACE INTO schedules (id, record_json) VALUES (?1, ?2)",
        params![schedule.id, serde_json::to_string(schedule)?],
    )?;
    Ok(())
}

fn delete_schedule(database: &Database, id: &str) -> Result<()> {
    database
        .lock()?
        .execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
    Ok(())
}

fn restore_schedules(database: &Database) -> Result<Vec<ScheduleRecord>> {
    let connection = database.lock()?;
    let mut statement = connection.prepare("SELECT record_json FROM schedules")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut schedules = Vec::new();
    for row in rows {
        match serde_json::from_str(&row?) {
            Ok(schedule) => schedules.push(schedule),
            Err(problem) => warn!(%problem, "ignored invalid persisted schedule"),
        }
    }
    Ok(schedules)
}

fn delete_workspace(database: &Database, workspace_id: &str) -> Result<()> {
    let mut connection = database.lock()?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM panes WHERE workspace_id = ?1",
        params![workspace_id],
    )?;
    transaction.execute(
        "DELETE FROM workspaces WHERE id = ?1",
        params![workspace_id],
    )?;
    transaction.commit()?;
    Ok(())
}

fn prune_exited(database: &Database, workspace_id: Option<&str>) -> Result<()> {
    let connection = database.lock()?;
    if let Some(workspace_id) = workspace_id {
        connection.execute(
            "DELETE FROM panes WHERE exited_at IS NOT NULL AND workspace_id = ?1",
            params![workspace_id],
        )?;
    } else {
        connection.execute("DELETE FROM panes WHERE exited_at IS NOT NULL", [])?;
    }
    Ok(())
}

fn prune_exited_before(database: &Database, cutoff: chrono::DateTime<Utc>) -> Result<()> {
    database.lock()?.execute(
        "DELETE FROM panes WHERE exited_at IS NOT NULL AND exited_at < ?1",
        params![cutoff.to_rfc3339()],
    )?;
    Ok(())
}

fn persist_attention(database: &Database, attention: &[AttentionEvent]) -> Result<()> {
    let mut connection = database.lock()?;
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM attention", [])?;
    let cutoff = Utc::now() - chrono::Duration::days(30);
    for event in attention
        .iter()
        .rev()
        .filter(|event| event.read_at.is_none_or(|read_at| read_at >= cutoff))
        .take(2_000)
    {
        transaction.execute(
            "INSERT INTO attention (id, record_json) VALUES (?1, ?2)",
            params![event.id, serde_json::to_string(event)?],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn restore_attention(database: &Database) -> Result<Vec<AttentionEvent>> {
    let connection = database.lock()?;
    let mut statement = connection.prepare("SELECT record_json FROM attention")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut events = Vec::new();
    for row in rows {
        if let Ok(event) = serde_json::from_str(&row?) {
            events.push(event);
        }
    }
    events.sort_by_key(|event: &AttentionEvent| event.created_at);
    Ok(events)
}

#[cfg(unix)]
fn set_socket_permissions(socket: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .context("set private socket permissions")
}

#[cfg(unix)]
fn verify_peer(stream: &UnixStream) -> Result<()> {
    let credentials = stream.peer_cred().context("read peer credentials")?;
    let peer_uid = credentials.uid();
    let own_uid = nix::unistd::geteuid().as_raw();
    if peer_uid != own_uid {
        bail!("refusing connection from uid {peer_uid}");
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_socket_permissions(_socket: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn verify_peer(_stream: &UnixStream) -> Result<()> {
    bail!("Muxloom currently supports Linux and Unix hosts only")
}

pub fn configure_logging(paths: &AppPaths, verbose: bool) -> Result<()> {
    paths.ensure()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())
        .context("open daemon log")?;
    let level = if verbose {
        "muxloom=debug"
    } else {
        "muxloom=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_writer(StdMutex::new(log))
        .with_ansi(false)
        .try_init()
        .ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_detection_is_conservative() {
        assert_eq!(detect_provider("/usr/bin/codex"), "codex");
        assert_eq!(detect_provider("claude"), "claude");
        assert_eq!(detect_provider("bash"), "generic");
    }

    #[test]
    fn output_inference_reports_attention_without_percentages() {
        let inferred = infer_state(b"Permission required for Bash").unwrap();
        assert_eq!(inferred.0, AgentState::WaitingApproval);
    }

    #[test]
    fn tail_capture_is_bounded_by_lines() {
        let output = VecDeque::from(b"one\ntwo\nthree\n".to_vec());
        assert_eq!(tail_lines(&output, 2), b"two\nthree\n");
    }

    #[test]
    fn cron_and_timezone_are_validated() {
        let schedule = ScheduleRecord {
            id: "id".into(),
            name: "daily".into(),
            expression: "0 0 9 * * *".into(),
            timezone: "Australia/Sydney".into(),
            cwd: "/tmp".into(),
            command: vec!["true".into()],
            enabled: true,
            last_run_at: None,
            next_run_at: None,
        };
        assert!(validate_schedule(&schedule).is_ok());
        assert!(calculate_next_run(&schedule, Utc::now()).unwrap().is_some());
    }

    #[test]
    fn readonly_policy_is_deny_by_default() {
        assert!(readonly_request_allowed(&Request::List));
        assert!(!readonly_request_allowed(&Request::Shutdown));
        assert!(!readonly_request_allowed(&Request::MarkAllAttentionRead));
        assert!(!readonly_request_allowed(&Request::CreateWorkspace {
            name: "x".into(),
            cwd: "/tmp".into(),
            command: vec!["true".into()],
            respawn: false
        }));
    }

    #[test]
    fn protocol_three_confidence_is_downgraded() {
        let mut event = AttentionEvent {
            id: "event".into(),
            pane_id: "pane".into(),
            provider: "codex".into(),
            kind: AgentState::WaitingApproval,
            severity: 2,
            summary: "approval required".into(),
            created_at: Utc::now(),
            read_at: None,
            confidence: EventConfidence::HookDerived,
        };
        downgrade_confidence(&mut event);
        assert_eq!(event.confidence, EventConfidence::Inferred);
    }

    #[test]
    fn legacy_database_is_migrated_in_place() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.db");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL); CREATE TABLE panes (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, title TEXT NOT NULL, cwd TEXT NOT NULL, command_json TEXT NOT NULL, provider TEXT NOT NULL, started_at TEXT NOT NULL, exited_at TEXT); CREATE TABLE schedules (id TEXT PRIMARY KEY, record_json TEXT NOT NULL);").unwrap();
        drop(connection);
        let database = initialize_database(&database).unwrap();
        let connection = database.lock().unwrap();
        assert!(database_column_exists(&connection, "workspaces", "layout_json").unwrap());
        assert!(database_column_exists(&connection, "panes", "exit_status").unwrap());
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            3
        );
    }
}
