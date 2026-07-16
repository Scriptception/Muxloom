use std::{
    collections::{HashMap, VecDeque},
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rusqlite::{Connection, params};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Mutex, broadcast, mpsc},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    model::{
        AgentState, AttentionEvent, EventConfidence, PaneSummary, ScheduleRecord, UsageSnapshot,
        WorkspaceSummary,
    },
    paths::AppPaths,
    protocol::{PROTOCOL_VERSION, Request, Response, read_frame, write_frame},
};

const OUTPUT_BUFFER_BYTES: usize = 2 * 1024 * 1024;

struct PaneRuntime {
    summary: PaneSummary,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<StdMutex<Box<dyn Write + Send>>>,
    output: VecDeque<u8>,
}

struct ServerState {
    workspaces: HashMap<String, WorkspaceSummary>,
    panes: HashMap<String, PaneRuntime>,
    attention: Vec<AttentionEvent>,
    schedules: Vec<ScheduleRecord>,
    usage: Vec<UsageSnapshot>,
    database: PathBuf,
    config: Config,
}

enum RuntimeEvent {
    Output { pane_id: String, data: Vec<u8> },
    Exited { pane_id: String },
}

pub async fn run(paths: AppPaths) -> Result<()> {
    paths.ensure()?;
    let config = Config::load_or_create(&paths)?;
    initialize_database(&paths.database())?;
    let restored = restore_metadata(&paths.database())?;
    let schedules = restore_schedules(&paths.database())?;

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
        attention: Vec::new(),
        schedules,
        usage: Vec::new(),
        database: paths.database(),
        config,
    }));
    let (updates, _) = broadcast::channel::<Response>(512);
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel::<RuntimeEvent>();

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

    let usage_state = Arc::clone(&state);
    tokio::spawn(async move {
        monitor_usage(usage_state).await;
    });

    info!(socket = %paths.socket().display(), "Muxloom daemon ready");
    loop {
        let (stream, _) = listener.accept().await.context("accept client")?;
        let client_state = Arc::clone(&state);
        let client_updates = updates.clone();
        let client_runtime = runtime_tx.clone();
        tokio::spawn(async move {
            if let Err(problem) =
                handle_client(stream, client_state, client_updates, client_runtime).await
            {
                debug!(%problem, "client disconnected");
            }
        });
    }
}

async fn handle_client(
    stream: UnixStream,
    state: Arc<Mutex<ServerState>>,
    updates: broadcast::Sender<Response>,
    runtime_tx: mpsc::UnboundedSender<RuntimeEvent>,
) -> Result<()> {
    verify_peer(&stream)?;
    let (mut reader, mut writer) = stream.into_split();
    let first: Request = read_frame(&mut reader).await?;

    if let Request::Attach {
        workspace_id,
        readonly,
    } = first
    {
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
                .map(|pane| {
                    (
                        pane.summary.id.clone(),
                        pane.output.iter().copied().collect(),
                    )
                })
                .collect::<Vec<(String, Vec<u8>)>>();
            (snapshot_locked(&guard), output)
        };
        write_frame(&mut writer, &snapshot).await?;
        for (pane_id, data) in buffered_output {
            if !data.is_empty() {
                write_frame(&mut writer, &Response::Output { pane_id, data }).await?;
            }
        }

        let mut subscription = updates.subscribe();
        loop {
            tokio::select! {
                incoming = read_frame::<_, Request>(&mut reader) => {
                    let request = incoming?;
                    if readonly && matches!(request, Request::Input { .. } | Request::CreatePane { .. } | Request::ClosePane { .. }) {
                        write_frame(&mut writer, &Response::Error { message: "client is attached read-only".into() }).await?;
                        continue;
                    }
                    match dispatch(request, &state, &updates, &runtime_tx).await {
                        Ok(Some(response)) => write_frame(&mut writer, &response).await?,
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
                            write_frame(&mut writer, &response).await?;
                        }
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
    }

    match dispatch(first, &state, &updates, &runtime_tx).await {
        Ok(Some(response)) => write_frame(&mut writer, &response).await?,
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
        Request::CreateWorkspace { name, cwd, command } => {
            let workspace = create_workspace(state, runtime_tx, name, cwd, command).await?;
            Response::WorkspaceCreated { workspace }
        }
        Request::CreatePane {
            workspace_id,
            cwd,
            command,
            title,
        } => {
            let pane = spawn_pane(&workspace_id, title, cwd, command, runtime_tx.clone())?;
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let workspace = guard
                .workspaces
                .get_mut(&workspace_id)
                .ok_or_else(|| anyhow!("workspace {workspace_id} does not exist"))?;
            workspace.panes.push(pane.summary.clone());
            let updated = workspace.clone();
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
                .map(|pane| tail_lines(&pane.output, lines))
                .unwrap_or_default();
            Response::Capture { pane_id, data }
        }
        Request::ClosePane { pane_id } => {
            let mut guard = state.lock().await;
            let workspace_id = guard
                .panes
                .get(&pane_id)
                .map(|pane| pane.summary.workspace_id.clone())
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
            let pane = guard
                .panes
                .remove(&pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?;
            drop(pane);
            let database = guard.database.clone();
            let workspace = guard.workspaces.get_mut(&workspace_id);
            if let Some(workspace) = workspace {
                workspace.panes.retain(|item| item.id != pane_id);
                let updated = workspace.clone();
                mark_pane_exited(&database, &pane_id)?;
                let response = Response::PaneUpdated { workspace: updated };
                let _ = updates.send(response.clone());
                response
            } else {
                mark_pane_exited(&database, &pane_id)?;
                Response::Ok
            }
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
            let event = guard.attention.last().cloned();
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
            Response::StateSnapshot {
                workspaces: Vec::new(),
                attention: guard.attention.clone(),
                usage: Vec::new(),
            }
        }
        Request::MarkAttentionRead { id } => {
            let mut guard = state.lock().await;
            if let Some(event) = guard.attention.iter_mut().find(|event| event.id == id) {
                event.read_at = Some(Utc::now());
            }
            Response::Ok
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
            Response::Ok
        }
        Request::DeleteSchedule { id } => {
            let mut guard = state.lock().await;
            guard.schedules.retain(|item| item.id != id);
            delete_schedule(&guard.database, &id)?;
            Response::Ok
        }
        Request::RunSchedule { id } => {
            let schedule = {
                let guard = state.lock().await;
                guard
                    .schedules
                    .iter()
                    .find(|item| item.id == id)
                    .cloned()
                    .ok_or_else(|| anyhow!("schedule {id} does not exist"))?
            };
            let name = format!("schedule-{}", schedule.name);
            let workspace =
                create_workspace(state, runtime_tx, name, schedule.cwd, schedule.command).await?;
            Response::WorkspaceCreated { workspace }
        }
        Request::Shutdown => {
            warn!("shutdown requested");
            std::process::exit(0);
        }
        Request::Attach { .. } => bail!("attach must be the first request on a connection"),
    };
    Ok(Some(response))
}

async fn create_workspace(
    state: &Arc<Mutex<ServerState>>,
    runtime_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    name: String,
    cwd: String,
    command: Vec<String>,
) -> Result<WorkspaceSummary> {
    let workspace_id = Uuid::new_v4().to_string();
    let mut workspace = WorkspaceSummary {
        id: workspace_id.clone(),
        name: unique_workspace_name(state, &name).await,
        created_at: Utc::now(),
        panes: Vec::new(),
    };
    let pane = spawn_pane(&workspace_id, None, cwd, command, runtime_tx.clone())?;
    workspace.panes.push(pane.summary.clone());
    let mut guard = state.lock().await;
    persist_workspace(&guard.database, &workspace)?;
    persist_pane(&guard.database, &pane.summary)?;
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
    let child = pair
        .slave
        .spawn_command(builder)
        .context("spawn pane command")?;
    let pid = child.process_id();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
    let writer = Arc::new(StdMutex::new(
        pair.master.take_writer().context("take PTY writer")?,
    ));
    let output_pane_id = pane_id.clone();
    thread::Builder::new()
        .name(format!("muxloom-pane-{}", &pane_id[..8]))
        .spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if runtime_tx
                            .send(RuntimeEvent::Output {
                                pane_id: output_pane_id.clone(),
                                data: buffer[..count].to_vec(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = runtime_tx.send(RuntimeEvent::Exited {
                pane_id: output_pane_id,
            });
            drop(child);
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
        unread: false,
    };
    Ok(PaneRuntime {
        summary,
        master: pair.master,
        writer,
        output: VecDeque::new(),
    })
}

async fn process_runtime_event(
    state: &Arc<Mutex<ServerState>>,
    updates: &broadcast::Sender<Response>,
    event: RuntimeEvent,
) -> Result<()> {
    match event {
        RuntimeEvent::Output { pane_id, data } => {
            let mut guard = state.lock().await;
            let scrollback_limit = guard
                .config
                .scrollback
                .lines
                .saturating_mul(240)
                .max(64 * 1024);
            let inferred = infer_state(&data);
            let mut state_changed = false;
            let mut workspace_id = None;
            if let Some(pane) = guard.panes.get_mut(&pane_id) {
                workspace_id = Some(pane.summary.workspace_id.clone());
                pane.output.extend(&data);
                let limit = scrollback_limit.min(OUTPUT_BUFFER_BYTES.max(scrollback_limit));
                while pane.output.len() > limit {
                    pane.output.pop_front();
                }
                if pane.summary.state == AgentState::Starting {
                    pane.summary.state = AgentState::Working;
                    state_changed = true;
                }
            }
            if let Some((next_state, summary)) = inferred {
                let provider = guard
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane.summary.provider.clone())
                    .unwrap_or_else(|| "generic".into());
                apply_agent_event(
                    &mut guard,
                    &pane_id,
                    &provider,
                    next_state,
                    &summary,
                    EventConfidence::Inferred,
                )?;
                state_changed = true;
            }
            if state_changed && let Some(workspace_id) = workspace_id {
                sync_workspace_panes(&mut guard, &workspace_id);
                if let Some(workspace) = guard.workspaces.get(&workspace_id).cloned() {
                    let _ = updates.send(Response::PaneUpdated { workspace });
                }
            }
            drop(guard);
            let _ = updates.send(Response::Output { pane_id, data });
        }
        RuntimeEvent::Exited { pane_id } => {
            let mut guard = state.lock().await;
            let database = guard.database.clone();
            let mut workspace_id = None;
            let mut attention_details = None;
            if let Some(pane) = guard.panes.get_mut(&pane_id) {
                workspace_id = Some(pane.summary.workspace_id.clone());
                pane.summary.state = AgentState::CompletedUnread;
                pane.summary.exited_at = Some(Utc::now());
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
            mark_pane_exited(&database, &pane_id)?;
            if let Some(workspace_id) = workspace_id {
                sync_workspace_panes(&mut guard, &workspace_id);
                if let Some(workspace) = guard.workspaces.get(&workspace_id).cloned() {
                    let _ = updates.send(Response::PaneUpdated { workspace });
                }
            }
            if let Some(event) = guard.attention.last().cloned() {
                let _ = updates.send(Response::Attention { event });
            }
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
        usage: state.usage.clone(),
    }
}

async fn monitor_usage(state: Arc<Mutex<ServerState>>) {
    let mut system = System::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let panes = {
            let guard = state.lock().await;
            guard
                .panes
                .values()
                .map(|pane| pane.summary.clone())
                .collect::<Vec<_>>()
        };
        let refreshed = tokio::task::spawn_blocking(move || {
            let process_ids = panes
                .iter()
                .filter_map(|pane| pane.pid.map(Pid::from_u32))
                .collect::<Vec<_>>();
            if !process_ids.is_empty() {
                system.refresh_processes(ProcessesToUpdate::Some(&process_ids), true);
            }
            let usage = panes
                .into_iter()
                .map(|pane| {
                    let process = pane.pid.and_then(|pid| system.process(Pid::from_u32(pid)));
                    UsageSnapshot {
                        pane_id: pane.id,
                        cpu_percent: process.map_or(0.0, sysinfo::Process::cpu_usage),
                        memory_bytes: process.map_or(0, sysinfo::Process::memory),
                        elapsed_seconds: (Utc::now() - pane.started_at).num_seconds().max(0) as u64,
                        input_tokens: None,
                        output_tokens: None,
                        cost_usd: None,
                    }
                })
                .collect();
            (system, usage)
        })
        .await;
        let Ok((next_system, usage)) = refreshed else {
            warn!("usage monitor worker failed");
            return;
        };
        system = next_system;
        state.lock().await.usage = usage;
    }
}

async fn response_matches_workspace(
    response: &Response,
    workspace_id: &str,
    state: &Arc<Mutex<ServerState>>,
) -> bool {
    match response {
        Response::Output { pane_id, .. } | Response::Capture { pane_id, .. } => state
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

async fn unique_workspace_name(state: &Arc<Mutex<ServerState>>, requested: &str) -> String {
    let base = if requested.trim().is_empty() {
        "workspace"
    } else {
        requested.trim()
    };
    let guard = state.lock().await;
    if !guard
        .workspaces
        .values()
        .any(|workspace| workspace.name == base)
    {
        return base.into();
    }
    for number in 2..10_000 {
        let candidate = format!("{base}-{number}");
        if !guard
            .workspaces
            .values()
            .any(|workspace| workspace.name == candidate)
        {
            return candidate;
        }
    }
    format!("{base}-{}", Uuid::new_v4().simple())
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

fn tail_lines(output: &VecDeque<u8>, lines: usize) -> Vec<u8> {
    let bytes = output.iter().copied().collect::<Vec<_>>();
    let mut starts = bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'\n').then_some(index + 1))
        .collect::<Vec<_>>();
    starts.push(0);
    starts.sort_unstable();
    let start = starts
        .iter()
        .rev()
        .nth(lines.saturating_sub(1))
        .copied()
        .unwrap_or(0);
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
            let active_names = guard
                .workspaces
                .values()
                .filter(|workspace| {
                    workspace.panes.iter().any(|pane| {
                        !matches!(pane.state, AgentState::Exited | AgentState::CompletedUnread)
                    })
                })
                .map(|workspace| workspace.name.clone())
                .collect::<Vec<_>>();
            let database = guard.database.clone();
            let mut due = Vec::new();
            for schedule in &mut guard.schedules {
                if !schedule.enabled || schedule.next_run_at.is_none_or(|next| next > now) {
                    continue;
                }
                let workspace_name = format!("schedule-{}", schedule.name);
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
                if !active_names.contains(&workspace_name) {
                    due.push(schedule.clone());
                }
            }
            due
        };
        for schedule in due {
            match create_workspace(
                &state,
                &runtime_tx,
                format!("schedule-{}", schedule.name),
                schedule.cwd,
                schedule.command,
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

fn initialize_database(database: &Path) -> Result<()> {
    let connection = Connection::open(database).context("open Muxloom state database")?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS workspaces (
           id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           created_at TEXT NOT NULL
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
           FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
         );
         CREATE TABLE IF NOT EXISTS schedules (
           id TEXT PRIMARY KEY,
           record_json TEXT NOT NULL
         );",
    )?;
    Ok(())
}

fn persist_workspace(database: &Path, workspace: &WorkspaceSummary) -> Result<()> {
    let connection = Connection::open(database)?;
    connection.execute(
        "INSERT OR REPLACE INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![
            workspace.id,
            workspace.name,
            workspace.created_at.to_rfc3339()
        ],
    )?;
    Ok(())
}

fn persist_pane(database: &Path, pane: &PaneSummary) -> Result<()> {
    let connection = Connection::open(database)?;
    connection.execute(
        "INSERT OR REPLACE INTO panes
         (id, workspace_id, title, cwd, command_json, provider, started_at, exited_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            pane.id,
            pane.workspace_id,
            pane.title,
            pane.cwd,
            serde_json::to_string(&pane.command)?,
            pane.provider,
            pane.started_at.to_rfc3339(),
            pane.exited_at.map(|value| value.to_rfc3339()),
        ],
    )?;
    Ok(())
}

fn mark_pane_exited(database: &Path, pane_id: &str) -> Result<()> {
    let connection = Connection::open(database)?;
    connection.execute(
        "UPDATE panes SET exited_at = ?1 WHERE id = ?2",
        params![Utc::now().to_rfc3339(), pane_id],
    )?;
    Ok(())
}

fn restore_metadata(database: &Path) -> Result<Vec<WorkspaceSummary>> {
    let connection = Connection::open(database)?;
    let mut statement = connection.prepare("SELECT id, name, created_at FROM workspaces")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut workspaces = Vec::new();
    for row in rows {
        let (id, name, created_at) = row?;
        workspaces.push(WorkspaceSummary {
            id,
            name,
            created_at: chrono::DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
            panes: Vec::new(),
        });
    }
    let mut pane_statement = connection.prepare(
        "SELECT id, workspace_id, title, cwd, command_json, provider, started_at, exited_at FROM panes",
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
        ))
    })?;
    for pane in panes {
        let (id, workspace_id, title, cwd, command_json, provider, started_at, exited_at) = pane?;
        let summary = PaneSummary {
            id,
            workspace_id: workspace_id.clone(),
            title,
            cwd,
            command: serde_json::from_str(&command_json).unwrap_or_default(),
            provider,
            pid: None,
            state: AgentState::Exited,
            progress: None,
            started_at: chrono::DateTime::parse_from_rfc3339(&started_at)?.with_timezone(&Utc),
            exited_at: exited_at
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| value.with_timezone(&Utc))
                .or(Some(Utc::now())),
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

fn persist_schedule(database: &Path, schedule: &ScheduleRecord) -> Result<()> {
    let connection = Connection::open(database)?;
    connection.execute(
        "INSERT OR REPLACE INTO schedules (id, record_json) VALUES (?1, ?2)",
        params![schedule.id, serde_json::to_string(schedule)?],
    )?;
    Ok(())
}

fn delete_schedule(database: &Path, id: &str) -> Result<()> {
    Connection::open(database)?.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
    Ok(())
}

fn restore_schedules(database: &Path) -> Result<Vec<ScheduleRecord>> {
    let connection = Connection::open(database)?;
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
    let own_uid = unsafe_uid();
    if peer_uid != own_uid {
        bail!("refusing connection from uid {peer_uid}");
    }
    Ok(())
}

#[cfg(unix)]
fn unsafe_uid() -> u32 {
    // std does not yet expose the effective uid. Reading /proc avoids unsafe libc calls.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(u32::MAX)
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
        assert_eq!(tail_lines(&output, 2), b"three\n");
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
}
