use std::{
    fs::OpenOptions,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{net::UnixStream, time::sleep};

use crate::{
    paths::AppPaths,
    protocol::{
        MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, Request, Response, read_frame, write_frame,
    },
    ui::{Action, App},
};

pub async fn request(paths: &AppPaths, request: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("connect to daemon")?;
    handshake(&mut stream).await?;
    write_frame(&mut stream, &request).await?;
    read_frame(&mut stream).await
}

/// Connect without starting a daemon. Passive hooks must never resurrect a stopped service.
pub async fn request_passive(paths: &AppPaths, passive_request: Request) -> Result<Response> {
    request(paths, passive_request).await
}

pub async fn ensure_daemon(paths: &AppPaths) -> Result<()> {
    if paths.socket().exists() {
        match probe_daemon(paths).await {
            Ok(Response::Pong { .. }) => return Ok(()),
            Ok(Response::Error { message }) => {
                bail!(
                    "running Muxloom daemon is incompatible: {message}. Finish active work, then run `muxloom server stop` and retry"
                );
            }
            Ok(other) => bail!("unexpected daemon health response: {other:?}"),
            Err(_) => {}
        }
    }
    paths.ensure()?;
    let executable = std::env::current_exe().context("locate muxloom executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log())
        .context("open daemon log")?;
    let errors = log.try_clone().context("clone daemon log handle")?;
    let mut daemon = if cfg!(unix) {
        let mut command = Command::new("setsid");
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    daemon
        .arg("daemon")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors))
        .spawn()
        .context("start Muxloom daemon")?;
    for _ in 0..50 {
        sleep(Duration::from_millis(40)).await;
        if matches!(probe_daemon(paths).await, Ok(Response::Pong { .. })) {
            return Ok(());
        }
    }
    bail!(
        "Muxloom daemon did not become ready; inspect {}",
        paths.log().display()
    )
}

async fn probe_daemon(paths: &AppPaths) -> Result<Response> {
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("connect to daemon")?;
    handshake(&mut stream).await?;
    write_frame(
        &mut stream,
        &Request::Ping {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;
    read_frame(&mut stream).await
}

async fn handshake(stream: &mut UnixStream) -> Result<()> {
    write_frame(
        stream,
        &Request::Hello {
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )
    .await?;
    match read_frame::<_, Response>(stream).await? {
        Response::Hello { .. } => Ok(()),
        Response::Error { message } => bail!(message),
        response => bail!("invalid daemon handshake: {response:?}"),
    }
}

pub async fn stop_daemon(paths: &AppPaths) -> Result<usize> {
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("connect to daemon")?;
    handshake(&mut stream).await?;
    write_frame(&mut stream, &Request::Shutdown).await?;
    match read_frame(&mut stream).await? {
        Response::ShutdownComplete { terminated_panes } => Ok(terminated_panes),
        Response::Error { message } => bail!(message),
        response => bail!("unexpected shutdown response: {response:?}"),
    }
}

pub async fn attach(paths: &AppPaths, workspace_id: String, readonly: bool) -> Result<()> {
    ensure_daemon(paths).await?;
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("connect to daemon")?;
    handshake(&mut stream).await?;
    let (mut reader, mut writer) = stream.into_split();
    write_frame(
        &mut writer,
        &Request::Attach {
            workspace_id: workspace_id.clone(),
            readonly,
        },
    )
    .await?;
    let initial: Response = read_frame(&mut reader).await?;
    let Response::StateSnapshot {
        workspaces,
        attention,
        usage,
    } = initial
    else {
        return Err(anyhow!("daemon rejected attach: {initial:?}"));
    };
    let config = crate::config::Config::load_or_create(paths)?;
    let mut app = App::new(
        workspace_id,
        workspaces,
        attention,
        usage,
        Vec::new(),
        Vec::new(),
        Default::default(),
        config,
    );
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )
    .context("enter alternate screen")?;
    let mut guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear()?;
    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;

    loop {
        tokio::select! {
            _ = redraw.tick() => if dirty {
                terminal.draw(|frame| app.render(frame))?;
                for (pane_id, rows, cols) in app.take_resizes() {
                    write_frame(&mut writer, &Request::Resize { pane_id, rows, cols }).await?;
                }
                dirty = false;
            },
            server = read_frame::<_, Response>(&mut reader) => {
                let response = server?;
                let created_workspace = match &response {
                    Response::WorkspaceCreated { workspace } => Some(workspace.id.clone()),
                    _ => None,
                };
                app.apply(response);
                dirty = true;
                if let Some(workspace_id) = created_workspace {
                    write_frame(&mut writer, &Request::SwitchWorkspace { workspace_id }).await?;
                }
            }
            terminal_event = events.next() => {
                let Some(Ok(event)) = terminal_event else { continue; };
                match event {
                    Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                        match app.process_key(key) {
                            Action::None => {}
                            Action::Quit => break,
                            Action::Send(request) => write_frame(&mut writer, &request).await?,
                            Action::SendMany(requests) => for request in requests { write_frame(&mut writer, &request).await?; },
                            Action::Osc52(sequence) => { use std::io::Write as _; std::io::stdout().write_all(sequence.as_bytes())?; std::io::stdout().flush()?; },
                        }
                    }
                    Event::Resize(columns, rows) => {
                        let _ = (columns, rows); // renderer drives every pane's exact inner size
                    }
                    Event::Paste(text) => if let Action::Send(request) = app.process_paste(text) { write_frame(&mut writer, &request).await?; },
                    Event::Mouse(mouse) => if let Action::Send(request) = app.process_mouse(mouse) { write_frame(&mut writer, &request).await?; },
                    _ => {}
                }
                dirty = true;
            }
        }
    }
    terminal.show_cursor()?;
    guard.restore()?;
    std::mem::forget(guard);
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn restore(&mut self) -> Result<()> {
        disable_raw_mode().context("disable terminal raw mode")?;
        execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        )
        .context("leave alternate screen")?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}
