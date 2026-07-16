use std::{
    fs::OpenOptions,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    event::{Event, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{net::UnixStream, time::sleep};

use crate::{
    integrations::{config_sources, discover_skills, hermes_snapshot},
    paths::AppPaths,
    protocol::{PROTOCOL_VERSION, Request, Response, read_frame, write_frame},
    ui::{Action, App},
};

pub async fn request(paths: &AppPaths, request: Request) -> Result<Response> {
    ensure_daemon(paths).await?;
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("connect to daemon")?;
    write_frame(&mut stream, &request).await?;
    read_frame(&mut stream).await
}

pub async fn ensure_daemon(paths: &AppPaths) -> Result<()> {
    if let Ok(mut stream) = UnixStream::connect(paths.socket()).await {
        write_frame(
            &mut stream,
            &Request::Ping {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;
        if matches!(
            read_frame::<_, Response>(&mut stream).await?,
            Response::Pong { .. }
        ) {
            return Ok(());
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
        if UnixStream::connect(paths.socket()).await.is_ok() {
            return Ok(());
        }
    }
    bail!(
        "Muxloom daemon did not become ready; inspect {}",
        paths.log().display()
    )
}

pub async fn attach(paths: &AppPaths, workspace_id: String, readonly: bool) -> Result<()> {
    ensure_daemon(paths).await?;
    let stream = UnixStream::connect(paths.socket())
        .await
        .context("connect to daemon")?;
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
    let mut app = App::new(
        workspace_id,
        workspaces,
        attention,
        usage,
        discover_skills(),
        config_sources(paths.config()),
        hermes_snapshot(),
    );
    write_frame(&mut writer, &Request::ListSchedules).await?;

    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let mut guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear()?;
    let mut events = EventStream::new();

    loop {
        terminal.draw(|frame| app.render(frame))?;
        tokio::select! {
            server = read_frame::<_, Response>(&mut reader) => {
                app.apply(server?);
            }
            terminal_event = events.next() => {
                let Some(Ok(event)) = terminal_event else { continue; };
                match event {
                    Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                        match app.process_key(key) {
                            Action::None => {}
                            Action::Quit => break,
                            Action::Send(request) => write_frame(&mut writer, &request).await?,
                        }
                    }
                    Event::Resize(columns, rows) => {
                        if let Some(pane_id) = app.selected_running_pane_id() {
                            write_frame(&mut writer, &Request::Resize {
                                pane_id,
                                rows: rows.saturating_sub(7),
                                cols: columns.saturating_sub(30),
                            }).await?;
                        }
                    }
                    _ => {}
                }
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
        execute!(std::io::stdout(), LeaveAlternateScreen).context("leave alternate screen")?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}
