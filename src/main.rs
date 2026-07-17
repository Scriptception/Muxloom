mod client;
mod config;
mod daemon;
mod integrations;
mod model;
mod paths;
mod protocol;
mod ui;

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

use crate::{
    model::{AgentState, EventConfidence, ScheduleRecord},
    paths::AppPaths,
    protocol::{Request, Response},
};

#[derive(Parser)]
#[command(name = "muxloom", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a persistent workspace and its first pane.
    New(NewArgs),
    /// Attach to a workspace by name or ID.
    Attach(AttachArgs),
    /// List workspaces and panes.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Rename, kill, prune, or inspect a workspace.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Inspect and update the persisted attention queue.
    Attention {
        #[command(subcommand)]
        command: AttentionCommand,
    },
    /// Generate shell completions on stdout.
    Completions { shell: clap_complete::Shell },
    /// Generate the muxloom(1) manual page on stdout.
    Man,
    /// Send text or stdin to a pane without shell evaluation.
    Send(SendArgs),
    /// Capture recent output from a pane.
    Capture {
        #[arg(long)]
        pane: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// Inspect runtime, integrations and security posture.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Inspect or initialize Muxloom configuration.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// List skills discovered across supported agent installations.
    Skills {
        #[arg(long)]
        json: bool,
    },
    /// Manage native agent schedules.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// Inspect and manage Hermes Agent through its local CLI.
    Hermes {
        #[command(subcommand)]
        command: HermesCommand,
    },
    /// Ingest a non-blocking agent lifecycle hook from stdin.
    Hook(HookArgs),
    /// Print safe hook snippets for Codex and Claude.
    Setup {
        #[command(subcommand)]
        command: SetupCommand,
    },
    /// Manage the local daemon.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        verbose: bool,
    },
}

#[derive(Args)]
struct NewArgs {
    #[arg(short, long, default_value = "workspace")]
    name: String,
    #[arg(short, long)]
    cwd: Option<PathBuf>,
    /// Create the workspace without opening the interactive TUI.
    #[arg(short, long)]
    detach: bool,
    /// Relaunch the command after a daemon restart.
    #[arg(long)]
    respawn: bool,
    #[arg(last = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct AttachArgs {
    workspace: Option<String>,
    #[arg(long)]
    readonly: bool,
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    Rename {
        workspace: String,
        name: String,
    },
    Kill {
        workspace: String,
        #[arg(long)]
        force: bool,
    },
    Prune {
        workspace: Option<String>,
    },
    Layout {
        workspace: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AttentionCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Read {
        id: String,
    },
    ReadAll,
    Dismiss {
        id: String,
    },
}

#[derive(Args)]
struct SendArgs {
    #[arg(long)]
    pane: String,
    #[arg(long)]
    enter: bool,
    #[arg(long)]
    stdin: bool,
    text: Option<String>,
}

#[derive(Args)]
struct HookArgs {
    #[arg(long)]
    provider: String,
    #[arg(long)]
    pane: Option<String>,
    #[arg(long, value_enum)]
    state: Option<HookState>,
    #[arg(long)]
    summary: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum HookState {
    Starting,
    Working,
    Approval,
    Input,
    Completed,
    Idle,
    Error,
}

#[derive(Subcommand)]
enum ConfigCommand {
    Show,
    Path,
    Reset,
    Sources,
    Edit {
        #[arg(value_enum)]
        provider: ConfigProvider,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ConfigProvider {
    Muxloom,
    Codex,
    Claude,
    Hermes,
}

#[derive(Subcommand)]
enum ScheduleCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long, default_value = "local")]
        timezone: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    Remove {
        id: String,
    },
    Run {
        id: String,
    },
}

#[derive(Subcommand)]
enum HermesCommand {
    Status,
    Sessions,
    Insights,
    Cron {
        #[command(subcommand)]
        action: HermesCronCommand,
    },
}

#[derive(Subcommand)]
enum HermesCronCommand {
    List,
    Add {
        schedule: String,
        prompt: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Run {
        id: String,
    },
    Remove {
        id: String,
    },
}

#[derive(Subcommand)]
enum SetupCommand {
    Hooks {
        #[arg(value_enum, default_value = "all")]
        provider: SetupProvider,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SetupProvider {
    Codex,
    Claude,
    All,
}

#[derive(Subcommand)]
enum ServerCommand {
    Start,
    Status,
    Stop {
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    match cli.command {
        Some(Command::Daemon { verbose, .. }) => {
            daemon::configure_logging(&paths, verbose)?;
            daemon::run(paths).await
        }
        Some(Command::New(arguments)) => new_workspace(&paths, arguments).await,
        Some(Command::Attach(arguments)) => attach_workspace(&paths, arguments).await,
        Some(Command::List { json }) => list_workspaces(&paths, json).await,
        Some(Command::Workspace { command }) => workspace_command(&paths, command).await,
        Some(Command::Attention { command }) => attention_command(&paths, command).await,
        Some(Command::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "muxloom",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Some(Command::Man) => {
            clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
            Ok(())
        }
        Some(Command::Send(arguments)) => send_input(&paths, arguments).await,
        Some(Command::Capture { pane, lines }) => capture(&paths, pane, lines).await,
        Some(Command::Doctor { json }) => doctor(&paths, json).await,
        Some(Command::Config { command }) => config_command(&paths, command),
        Some(Command::Skills { json }) => skills_command(json),
        Some(Command::Schedule { command }) => schedule_command(&paths, command).await,
        Some(Command::Hermes { command }) => hermes_command(command),
        Some(Command::Hook(arguments)) => hook(&paths, arguments).await,
        Some(Command::Setup { command }) => setup(command),
        Some(Command::Server { command }) => server_command(&paths, command).await,
        None => {
            attach_workspace(
                &paths,
                AttachArgs {
                    workspace: None,
                    readonly: false,
                },
            )
            .await
        }
    }
}

async fn new_workspace(paths: &AppPaths, arguments: NewArgs) -> Result<()> {
    client::ensure_daemon(paths).await?;
    let cwd = arguments
        .cwd
        .unwrap_or(std::env::current_dir().context("read current directory")?);
    let response = client::request(
        paths,
        Request::CreateWorkspace {
            name: arguments.name,
            cwd: cwd.display().to_string(),
            command: arguments.command,
            respawn: arguments.respawn,
        },
    )
    .await?;
    match response {
        Response::WorkspaceCreated { workspace } => {
            println!("Created {} ({})", workspace.name, workspace.id);
            if arguments.detach {
                Ok(())
            } else {
                client::attach(paths, workspace.id, false).await
            }
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn attach_workspace(paths: &AppPaths, arguments: AttachArgs) -> Result<()> {
    client::ensure_daemon(paths).await?;
    let response = client::request(paths, Request::List).await?;
    let Response::StateSnapshot { workspaces, .. } = response else {
        bail!("unable to list workspaces");
    };
    if workspaces.is_empty() {
        return new_workspace(
            paths,
            NewArgs {
                name: "workspace".into(),
                cwd: None,
                detach: false,
                respawn: false,
                command: Vec::new(),
            },
        )
        .await;
    }
    let requested = arguments.workspace.as_deref();
    let workspace = requested
        .map_or_else(
            || workspaces.last(),
            |requested| {
                workspaces.iter().find(|workspace| {
                    workspace.id.starts_with(requested) || workspace.name == requested
                })
            },
        )
        .ok_or_else(|| anyhow!("workspace was not found"))?;
    client::attach(paths, workspace.id.clone(), arguments.readonly).await
}

async fn list_workspaces(paths: &AppPaths, json: bool) -> Result<()> {
    match client::request(paths, Request::List).await? {
        Response::StateSnapshot { workspaces, .. } if json => {
            println!("{}", serde_json::to_string_pretty(&workspaces)?);
        }
        Response::StateSnapshot { workspaces, .. } => {
            if workspaces.is_empty() {
                println!("No workspaces. Run `muxloom new`. ");
            }
            for workspace in workspaces {
                println!(
                    "{}  {}  {} pane(s)",
                    &workspace.id[..8],
                    workspace.name,
                    workspace.panes.len()
                );
                for pane in workspace.panes {
                    println!(
                        "  {}  {:<10} {:<12} {}",
                        &pane.id[..8],
                        pane.state.label(),
                        pane.provider,
                        pane.title
                    );
                }
            }
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
    Ok(())
}

async fn resolve_workspace(paths: &AppPaths, requested: &str) -> Result<model::WorkspaceSummary> {
    let Response::StateSnapshot { workspaces, .. } = client::request(paths, Request::List).await?
    else {
        bail!("unable to list workspaces");
    };
    workspaces
        .into_iter()
        .find(|workspace| workspace.id.starts_with(requested) || workspace.name == requested)
        .ok_or_else(|| anyhow!("workspace was not found: {requested}"))
}

async fn workspace_command(paths: &AppPaths, command: WorkspaceCommand) -> Result<()> {
    let request = match command {
        WorkspaceCommand::Rename { workspace, name } => Request::RenameWorkspace {
            workspace_id: resolve_workspace(paths, &workspace).await?.id,
            name,
        },
        WorkspaceCommand::Kill { workspace, force } => {
            let workspace = resolve_workspace(paths, &workspace).await?;
            let live = workspace
                .panes
                .iter()
                .filter(|pane| pane.exited_at.is_none() && !pane.daemon_lost)
                .count();
            if live > 0 && !force {
                bail!(
                    "workspace {} owns {live} live pane(s); rerun with --force to terminate them",
                    workspace.name
                );
            }
            Request::DeleteWorkspace {
                workspace_id: workspace.id,
            }
        }
        WorkspaceCommand::Prune { workspace } => Request::PruneExited {
            workspace_id: match workspace {
                Some(workspace) => Some(resolve_workspace(paths, &workspace).await?.id),
                None => None,
            },
        },
        WorkspaceCommand::Layout { workspace, json } => {
            let workspace = resolve_workspace(paths, &workspace).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&workspace.layout)?);
            } else {
                println!("{:#?}", workspace.layout);
            }
            return Ok(());
        }
    };
    match client::request(paths, request).await? {
        Response::Ok | Response::PaneUpdated { .. } | Response::StateSnapshot { .. } => Ok(()),
        Response::Error { message } => bail!(message),
        response => bail!("unexpected daemon response: {response:?}"),
    }
}

async fn attention_command(paths: &AppPaths, command: AttentionCommand) -> Result<()> {
    let request = match command {
        AttentionCommand::List { json } => {
            let Response::StateSnapshot { attention, .. } =
                client::request(paths, Request::ListAttention).await?
            else {
                bail!("unable to list attention");
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&attention)?);
            } else {
                for event in attention.iter().filter(|event| event.read_at.is_none()) {
                    println!(
                        "{}  {:<10} {}",
                        &event.id[..8],
                        event.kind.label(),
                        event.summary
                    );
                }
            }
            return Ok(());
        }
        AttentionCommand::Read { id } => Request::MarkAttentionRead { id },
        AttentionCommand::ReadAll => Request::MarkAllAttentionRead,
        AttentionCommand::Dismiss { id } => Request::DismissAttention { id },
    };
    match client::request(paths, request).await? {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!(message),
        response => bail!("unexpected daemon response: {response:?}"),
    }
}

async fn send_input(paths: &AppPaths, arguments: SendArgs) -> Result<()> {
    let mut data = if arguments.stdin {
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        input
    } else {
        arguments.text.unwrap_or_default().into_bytes()
    };
    if arguments.enter {
        data.push(b'\r');
    }
    match client::request(
        paths,
        Request::Input {
            pane_id: arguments.pane,
            data,
        },
    )
    .await?
    {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn capture(paths: &AppPaths, pane: String, lines: usize) -> Result<()> {
    match client::request(
        paths,
        Request::Capture {
            pane_id: pane,
            lines,
        },
    )
    .await?
    {
        Response::Capture { data, .. } => {
            print!("{}", String::from_utf8_lossy(&data));
            Ok(())
        }
        Response::Error { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn doctor(paths: &AppPaths, json: bool) -> Result<()> {
    let config = config::Config::load_or_create(paths)?;
    let home = directories::BaseDirs::new().context("cannot determine home directory")?;
    let codex_hooks = hook_installation_valid(
        &home.home_dir().join(".codex/hooks.json"),
        "--provider codex",
    );
    let claude_hooks = hook_installation_valid(
        &home.home_dir().join(".claude/settings.json"),
        "--provider claude",
    );
    let daemon = matches!(
        client::request(
            paths,
            Request::Ping {
                protocol_version: protocol::PROTOCOL_VERSION
            }
        )
        .await,
        Ok(Response::Pong { .. })
    );
    let checks = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "daemon": daemon,
        "socket": paths.socket(),
        "socket_private": paths.socket().metadata().map(|metadata| {
            #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; metadata.permissions().mode() & 0o077 == 0 }
            #[cfg(not(unix))] { false }
        }).unwrap_or(false),
        "config": paths.config(),
        "state_database": paths.database(),
        "codex": which("codex"),
        "claude": which("claude"),
        "hermes": which("hermes"),
        "codex_hooks": codex_hooks,
        "claude_hooks": claude_hooks,
        "effective_keymap": &config.keymap,
        "web_listener": false,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        println!("Muxloom doctor\n");
        println!(
            "  daemon          {}",
            if daemon { "healthy" } else { "unavailable" }
        );
        println!("  private socket  {}", checks["socket_private"]);
        println!("  Codex           {}", checks["codex"]);
        println!("  Claude          {}", checks["claude"]);
        println!("  Hermes          {}", checks["hermes"]);
        println!(
            "  Codex hooks     {}",
            if codex_hooks {
                "configured"
            } else {
                "missing or drifted"
            }
        );
        println!(
            "  Claude hooks    {}",
            if claude_hooks {
                "configured"
            } else {
                "missing or drifted"
            }
        );
        println!("  mode toggle     {}", config.keymap.toggle_mode);
        println!("  network         local Unix socket only");
    }
    Ok(())
}

fn hook_installation_valid(location: &Path, marker: &str) -> bool {
    fs::read_to_string(location)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .is_some_and(|value| value.to_string().contains(marker))
}

fn config_command(paths: &AppPaths, command: Option<ConfigCommand>) -> Result<()> {
    paths.ensure()?;
    let config = match command {
        Some(ConfigCommand::Reset) => {
            let config = config::Config::default();
            config.save_atomic(&paths.config())?;
            config
        }
        Some(ConfigCommand::Edit { provider }) => {
            edit_config(paths, provider)?;
            config::Config::load_or_create(paths)?
        }
        _ => config::Config::load_or_create(paths)?,
    };
    match command {
        Some(ConfigCommand::Path) => println!("{}", paths.config().display()),
        Some(ConfigCommand::Sources) => {
            for source in integrations::config_sources(paths.config()) {
                println!(
                    "{:<10} {:<8} {}",
                    source.provider,
                    if source.present { "present" } else { "missing" },
                    source.location.display()
                );
            }
        }
        Some(ConfigCommand::Edit { .. }) => println!("Configuration updated and validated."),
        _ => print!("{}", toml::to_string_pretty(&config)?),
    }
    Ok(())
}

fn skills_command(json: bool) -> Result<()> {
    let skills = integrations::discover_skills();
    if json {
        println!("{}", serde_json::to_string_pretty(&skills)?);
    } else {
        for skill in skills {
            println!(
                "{} {:<10} {:<28} {}",
                if skill.valid { "✓" } else { "!" },
                skill.provider,
                skill.name,
                skill.location
            );
        }
    }
    Ok(())
}

fn hermes_command(command: HermesCommand) -> Result<()> {
    let arguments = match command {
        HermesCommand::Status => vec!["status".into(), "--all".into()],
        HermesCommand::Sessions => vec!["sessions".into(), "list".into()],
        HermesCommand::Insights => vec!["insights".into(), "--days".into(), "7".into()],
        HermesCommand::Cron { action } => match action {
            HermesCronCommand::List => vec!["cron".into(), "list".into(), "--all".into()],
            HermesCronCommand::Add {
                schedule,
                prompt,
                name,
                workdir,
            } => {
                let mut arguments = vec!["cron".into(), "create".into()];
                if let Some(name) = name {
                    arguments.extend(["--name".into(), name]);
                }
                if let Some(workdir) = workdir {
                    arguments.extend(["--workdir".into(), workdir.display().to_string()]);
                }
                arguments.extend([schedule, prompt]);
                arguments
            }
            HermesCronCommand::Pause { id } => vec!["cron".into(), "pause".into(), id],
            HermesCronCommand::Resume { id } => vec!["cron".into(), "resume".into(), id],
            HermesCronCommand::Run { id } => vec!["cron".into(), "run".into(), id],
            HermesCronCommand::Remove { id } => vec!["cron".into(), "remove".into(), id],
        },
    };
    let result = ProcessCommand::new("hermes")
        .args(&arguments)
        .status()
        .context("run Hermes CLI")?;
    if !result.success() {
        bail!("Hermes command failed with {result}");
    }
    Ok(())
}

async fn schedule_command(paths: &AppPaths, command: ScheduleCommand) -> Result<()> {
    let request = match command {
        ScheduleCommand::List { json } => {
            let response = client::request(paths, Request::ListSchedules).await?;
            let Response::Schedules { schedules } = response else {
                bail!("unable to list schedules")
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&schedules)?);
            } else {
                for schedule in schedules {
                    println!(
                        "{}  {:<20} {}  {}",
                        &schedule.id[..8],
                        schedule.name,
                        schedule.expression,
                        schedule.command.join(" ")
                    );
                }
            }
            return Ok(());
        }
        ScheduleCommand::Add {
            name,
            cron,
            timezone,
            cwd,
            command,
        } => Request::CreateSchedule {
            schedule: ScheduleRecord {
                id: Uuid::new_v4().to_string(),
                name,
                expression: cron,
                timezone,
                cwd: cwd
                    .unwrap_or(std::env::current_dir()?)
                    .display()
                    .to_string(),
                command,
                enabled: true,
                last_run_at: None,
                next_run_at: None,
            },
        },
        ScheduleCommand::Remove { id } => Request::DeleteSchedule { id },
        ScheduleCommand::Run { id } => Request::RunSchedule { id },
    };
    match client::request(paths, request).await? {
        Response::Ok | Response::WorkspaceCreated { .. } => Ok(()),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

async fn hook(paths: &AppPaths, arguments: HookArgs) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let payload: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let environment_pane = std::env::var("MUXLOOM_PANE_ID").ok();
    let payload_pane = payload.get("pane_id").and_then(|value| value.as_str());
    let pane_id = arguments.pane.or_else(|| {
        environment_pane.as_ref().and_then(|trusted| {
            payload_pane
                .is_none_or(|candidate| candidate == trusted)
                .then(|| trusted.clone())
        })
    });
    let Some(pane_id) = pane_id else {
        return Ok(());
    };
    let event_name = payload
        .get("hook_event_name")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let explicit_state = arguments.state;
    let state = explicit_state
        .map(hook_state)
        .unwrap_or_else(|| infer_hook_state(event_name, &payload));
    let summary = arguments.summary.unwrap_or_else(|| {
        payload
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or(event_name)
            .to_string()
    });
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        client::request_passive(
            paths,
            Request::AgentEvent {
                pane_id,
                provider: arguments.provider,
                state,
                summary,
                confidence: if explicit_state.is_some() {
                    EventConfidence::Native
                } else {
                    EventConfidence::Inferred
                },
            },
        ),
    )
    .await;
    Ok(())
}

fn setup(command: SetupCommand) -> Result<()> {
    match command {
        SetupCommand::Hooks { provider, dry_run } => {
            let executable = std::env::current_exe()?;
            let codex = codex_hook(executable.to_string_lossy().as_ref());
            let claude = claude_hook(executable.to_string_lossy().as_ref());
            if dry_run {
                if matches!(provider, SetupProvider::Codex | SetupProvider::All) {
                    println!(
                        "\nCodex (~/.codex/hooks.json):\n{}",
                        serde_json::to_string_pretty(&codex)?
                    );
                }
                if matches!(provider, SetupProvider::Claude | SetupProvider::All) {
                    println!(
                        "\nClaude (~/.claude/settings.json):\n{}",
                        serde_json::to_string_pretty(&claude)?
                    );
                }
                return Ok(());
            }
            let home = directories::BaseDirs::new().context("cannot determine home directory")?;
            if matches!(provider, SetupProvider::Codex | SetupProvider::All) {
                merge_hook_config(&home.home_dir().join(".codex/hooks.json"), &codex)?;
                println!("Installed non-blocking Codex hooks.");
            }
            if matches!(provider, SetupProvider::Claude | SetupProvider::All) {
                merge_hook_config(&home.home_dir().join(".claude/settings.json"), &claude)?;
                println!("Installed non-blocking Claude hooks.");
            }
        }
    }
    Ok(())
}

async fn server_command(paths: &AppPaths, command: ServerCommand) -> Result<()> {
    match command {
        ServerCommand::Start => client::ensure_daemon(paths).await,
        ServerCommand::Status => {
            let response = client::request(
                paths,
                Request::Ping {
                    protocol_version: protocol::PROTOCOL_VERSION,
                },
            )
            .await?;
            println!("{response:?}");
            Ok(())
        }
        ServerCommand::Stop { force } => {
            let live = match client::request(paths, Request::List).await? {
                Response::StateSnapshot { workspaces, .. } => workspaces
                    .into_iter()
                    .flat_map(|workspace| workspace.panes)
                    .filter(|pane| pane.exited_at.is_none() && !pane.daemon_lost)
                    .count(),
                _ => 0,
            };
            if live > 0 && !force {
                bail!(
                    "daemon owns {live} live pane(s); rerun with `muxloom server stop --force` to terminate them"
                );
            }
            let terminated = client::stop_daemon(paths).await?;
            println!("Stopped Muxloom daemon; terminated {terminated} pane(s).");
            Ok(())
        }
    }
}

fn hook_state(state: HookState) -> AgentState {
    match state {
        HookState::Starting => AgentState::Starting,
        HookState::Working => AgentState::Working,
        HookState::Approval => AgentState::WaitingApproval,
        HookState::Input => AgentState::WaitingInput,
        HookState::Completed => AgentState::CompletedUnread,
        HookState::Idle => AgentState::Idle,
        HookState::Error => AgentState::Error,
    }
}

fn infer_hook_state(event: &str, payload: &serde_json::Value) -> AgentState {
    match event {
        "PermissionRequest" => AgentState::WaitingApproval,
        "Notification"
            if payload
                .get("notification_type")
                .and_then(|value| value.as_str())
                == Some("permission_prompt") =>
        {
            AgentState::WaitingApproval
        }
        "Notification" => AgentState::WaitingInput,
        "Stop" | "SubagentStop" | "SessionEnd" => AgentState::CompletedUnread,
        "PostToolUseFailure" | "StopFailure" => AgentState::Error,
        "SessionStart" | "SubagentStart" | "PreToolUse" | "PostToolUse" => AgentState::Working,
        _ => AgentState::Unknown,
    }
}

fn codex_hook(executable: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": format!("{executable} hook --provider codex"), "timeout": 5}]}],
            "PermissionRequest": [{"hooks": [{"type": "command", "command": format!("{executable} hook --provider codex"), "timeout": 5}]}],
            "PostToolUse": [{"hooks": [{"type": "command", "command": format!("{executable} hook --provider codex"), "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": format!("{executable} hook --provider codex"), "timeout": 5}]}]
        }
    })
}

fn claude_hook(executable: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "Notification": [
                {"matcher": "permission_prompt", "hooks": [{"type": "command", "command": format!("{executable} hook --provider claude")} ]},
                {"matcher": "idle_prompt", "hooks": [{"type": "command", "command": format!("{executable} hook --provider claude")} ]}
            ],
            "PermissionRequest": [{"matcher": "*", "hooks": [{"type": "command", "command": format!("{executable} hook --provider claude")}]}],
            "Stop": [{"hooks": [{"type": "command", "command": format!("{executable} hook --provider claude")}]}]
        }
    })
}

fn merge_hook_config(location: &Path, addition: &serde_json::Value) -> Result<()> {
    if fs::symlink_metadata(location).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "refusing to update symlinked hook configuration: {}",
            location.display()
        );
    }
    let mut existing = if location.exists() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(location)?)
            .with_context(|| format!("parse {}", location.display()))?
    } else {
        serde_json::json!({})
    };
    let object = existing
        .as_object_mut()
        .context("hook config root must be an object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks = hooks.as_object_mut().context("hooks must be an object")?;
    if let Some(additions) = addition.get("hooks").and_then(serde_json::Value::as_object) {
        for (event, handlers) in additions {
            let target = hooks.entry(event).or_insert_with(|| serde_json::json!([]));
            let target = target
                .as_array_mut()
                .context("hook event must be an array")?;
            for handler in handlers.as_array().into_iter().flatten() {
                if !target.iter().any(|current| current == handler) {
                    target.push(handler.clone());
                }
            }
        }
    }
    let parent = location.parent().context("hook config has no parent")?;
    fs::create_dir_all(parent)?;
    if location.exists() {
        let backup = location.with_extension(format!(
            "json.muxloom-backup-{}",
            chrono::Utc::now().format("%Y%m%d%H%M%S")
        ));
        fs::copy(location, backup)?;
    }
    let temporary = parent.join(format!(".muxloom-hooks-{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(format!("{}\n", serde_json::to_string_pretty(&existing)?).as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, location)?;
    Ok(())
}

fn edit_config(paths: &AppPaths, provider: ConfigProvider) -> Result<()> {
    if matches!(provider, ConfigProvider::Hermes) {
        let result = ProcessCommand::new("hermes")
            .args(["config", "edit"])
            .status()?;
        if !result.success() {
            bail!("Hermes config editor failed");
        }
        let validation = ProcessCommand::new("hermes")
            .args(["config", "check"])
            .status()?;
        if !validation.success() {
            bail!("Hermes config validation failed");
        }
        return Ok(());
    }
    let home = directories::BaseDirs::new().context("cannot determine home directory")?;
    let (location, format) = match provider {
        ConfigProvider::Muxloom => (paths.config(), "toml"),
        ConfigProvider::Codex => (home.home_dir().join(".codex/config.toml"), "toml"),
        ConfigProvider::Claude => (home.home_dir().join(".claude/settings.json"), "json"),
        ConfigProvider::Hermes => unreachable!(),
    };
    if !location.exists() {
        bail!("configuration does not exist: {}", location.display());
    }
    if fs::symlink_metadata(&location)?.file_type().is_symlink() {
        bail!(
            "refusing to edit symlinked configuration: {}",
            location.display()
        );
    }
    let temporary = location
        .parent()
        .context("config has no parent")?
        .join(format!(".muxloom-edit-{}.{}", Uuid::new_v4(), format));
    fs::copy(&location, &temporary)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut editor_parts = shell_words::split(&editor).context("parse EDITOR")?;
    let program = editor_parts.first().cloned().context("EDITOR is empty")?;
    let result = ProcessCommand::new(program)
        .args(editor_parts.drain(1..))
        .arg(&temporary)
        .status()?;
    if !result.success() {
        let _ = fs::remove_file(&temporary);
        bail!("editor exited without saving");
    }
    let contents = fs::read_to_string(&temporary)?;
    match format {
        "toml" => {
            toml::from_str::<toml::Value>(&contents).context("edited TOML is invalid")?;
        }
        "json" => {
            serde_json::from_str::<serde_json::Value>(&contents)
                .context("edited JSON is invalid")?;
        }
        _ => unreachable!(),
    }
    let backup = location.with_extension(format!(
        "{format}.muxloom-backup-{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    ));
    fs::copy(&location, &backup)?;
    fs::rename(&temporary, &location)?;
    println!("Backup: {}", backup.display());
    Ok(())
}

fn which(program: &str) -> bool {
    std::process::Command::new("sh")
        .args([
            "-c",
            "command -v \"$1\" >/dev/null 2>&1",
            "muxloom-doctor",
            program,
        ])
        .status()
        .is_ok_and(|status| status.success())
}
