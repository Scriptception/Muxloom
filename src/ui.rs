use std::collections::HashMap;

use base64::Engine as _;
use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    config::{Config, KeymapConfig, LaunchTemplate},
    integrations::{ConfigSource, HermesSnapshot, git_summary},
    model::{
        AgentState, AttentionEvent, LayoutNode, ScheduleRecord, SkillRecord, SplitAxis,
        UsageSnapshot, WorkspaceSummary,
    },
    protocol::{Request, Response},
};

const INK: Color = Color::Rgb(7, 12, 24);
const SURFACE: Color = Color::Rgb(14, 22, 39);
const RAISED: Color = Color::Rgb(22, 33, 55);
const BORDER: Color = Color::Rgb(54, 70, 98);
const ACCENT: Color = Color::Rgb(79, 209, 255);
const VIOLET: Color = Color::Rgb(160, 132, 255);
const TEXT: Color = Color::Rgb(229, 235, 247);
const MUTED: Color = Color::Rgb(136, 151, 177);
const SUCCESS: Color = Color::Rgb(81, 207, 151);
const WARNING: Color = Color::Rgb(255, 190, 92);
const DANGER: Color = Color::Rgb(255, 107, 134);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum View {
    Workspace,
    Attention,
    Usage,
    Skills,
    Config,
    Schedules,
    Hermes,
    Help,
}

#[derive(Debug, Clone)]
pub enum Action {
    None,
    Send(Request),
    SendMany(Vec<Request>),
    Osc52(String),
    Quit,
}

pub struct App {
    pub workspace_id: String,
    pub workspaces: Vec<WorkspaceSummary>,
    pub attention: Vec<AttentionEvent>,
    pub usage: Vec<UsageSnapshot>,
    pub schedules: Vec<ScheduleRecord>,
    pub skills: Vec<SkillRecord>,
    pub configs: Vec<ConfigSource>,
    pub hermes: HermesSnapshot,
    pub view: View,
    pub selected_pane: usize,
    pub navigation: bool,
    pub prompt_mode: bool,
    pub prompt: String,
    pub toast: String,
    creating_workspace: bool,
    confirm_close: bool,
    parsers: HashMap<String, vt100::Parser>,
    sequences: HashMap<String, u64>,
    last_sizes: HashMap<String, (u16, u16)>,
    pending_resizes: Vec<(String, u16, u16)>,
    scroll_offsets: HashMap<String, usize>,
    zoomed: bool,
    show_rail: bool,
    show_context: bool,
    mouse_enabled: bool,
    keymap: KeymapConfig,
    attention_index: usize,
    leader_pending: bool,
    send_toggle_pending: bool,
    launcher_axis: Option<SplitAxis>,
    pending_workspace_name: Option<String>,
    launch_templates: Vec<LaunchTemplate>,
    prompt_history: Vec<String>,
    prompt_history_index: Option<usize>,
    persist_prompt_history: bool,
    prompt_broadcast: bool,
    prompt_target: Option<String>,
    copy_search: String,
    copy_search_input: bool,
    visual_select: bool,
    last_pane_rects: Vec<(String, Rect)>,
    last_workspace_rows: Vec<(String, Rect)>,
    pending_chord: Option<char>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_id: String,
        workspaces: Vec<WorkspaceSummary>,
        attention: Vec<AttentionEvent>,
        usage: Vec<UsageSnapshot>,
        skills: Vec<SkillRecord>,
        configs: Vec<ConfigSource>,
        hermes: HermesSnapshot,
        config: Config,
    ) -> Self {
        let mut parsers = HashMap::new();
        for pane in workspaces.iter().flat_map(|workspace| &workspace.panes) {
            parsers.insert(pane.id.clone(), vt100::Parser::new(30, 120, 5_000));
        }
        let persist_prompt_history = config.ui.prompt_history;
        let prompt_history = if persist_prompt_history {
            load_prompt_history().unwrap_or_default()
        } else {
            Vec::new()
        };
        Self {
            workspace_id,
            workspaces,
            attention,
            usage,
            schedules: Vec::new(),
            skills,
            configs,
            hermes,
            view: View::Workspace,
            selected_pane: 0,
            navigation: true,
            prompt_mode: false,
            prompt: String::new(),
            toast: "Navigation mode · press Enter to focus the agent".into(),
            creating_workspace: false,
            confirm_close: false,
            parsers,
            sequences: HashMap::new(),
            last_sizes: HashMap::new(),
            pending_resizes: Vec::new(),
            scroll_offsets: HashMap::new(),
            zoomed: false,
            show_rail: config.ui.workspace_rail,
            show_context: config.ui.context_panel,
            mouse_enabled: config.ui.mouse,
            keymap: config.keymap,
            attention_index: 0,
            leader_pending: false,
            send_toggle_pending: false,
            launcher_axis: None,
            pending_workspace_name: None,
            launch_templates: config.launchers,
            prompt_history,
            prompt_history_index: None,
            persist_prompt_history,
            prompt_broadcast: false,
            prompt_target: None,
            copy_search: String::new(),
            copy_search_input: false,
            visual_select: false,
            last_pane_rects: Vec::new(),
            last_workspace_rows: Vec::new(),
            pending_chord: None,
        }
    }

    pub fn apply(&mut self, response: Response) {
        match response {
            Response::StateSnapshot {
                workspaces,
                attention,
                usage,
            } => {
                if !workspaces.is_empty() {
                    self.workspaces = workspaces;
                    self.ensure_parsers();
                }
                self.attention = attention;
                self.usage = usage;
            }
            Response::Output {
                pane_id,
                data,
                sequence,
                ..
            } => {
                let expected = self.sequences.get(&pane_id).copied().unwrap_or(sequence);
                if sequence != expected {
                    self.toast = format!(
                        "Output resync required for {}",
                        &pane_id[..8.min(pane_id.len())]
                    );
                }
                self.parsers
                    .entry(pane_id.clone())
                    .or_insert_with(|| vt100::Parser::new(30, 120, 5_000))
                    .process(&data);
                self.sequences.insert(pane_id, sequence.saturating_add(1));
            }
            Response::PaneSnapshot {
                pane_id,
                data,
                sequence,
                reset,
            } => {
                if reset {
                    let (rows, cols) = self.last_sizes.get(&pane_id).copied().unwrap_or((30, 120));
                    self.parsers
                        .insert(pane_id.clone(), vt100::Parser::new(rows, cols, 5_000));
                }
                self.parsers
                    .entry(pane_id.clone())
                    .or_insert_with(|| vt100::Parser::new(30, 120, 5_000))
                    .process(&data);
                self.sequences.insert(pane_id, sequence);
            }
            Response::PaneUpdated { workspace } => {
                if let Some(existing) = self
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == workspace.id)
                {
                    *existing = workspace;
                } else {
                    self.workspaces.push(workspace);
                }
                self.ensure_parsers();
                self.clamp_selection();
            }
            Response::Attention { event } => {
                if let Some(existing) = self.attention.iter_mut().find(|item| item.id == event.id) {
                    *existing = event;
                } else {
                    self.toast = format!("Attention · {}", event.summary);
                    self.attention.push(event);
                }
            }
            Response::Schedules { schedules } => self.schedules = schedules,
            Response::Error { message } => self.toast = format!("Error · {message}"),
            Response::WorkspaceCreated { workspace } => {
                self.workspace_id = workspace.id.clone();
                self.workspaces.push(workspace);
                self.selected_pane = 0;
                self.view = View::Workspace;
                self.ensure_parsers();
                self.toast = "New workspace ready".into();
            }
            Response::Ok
            | Response::Hello { .. }
            | Response::Pong { .. }
            | Response::Capture { .. }
            | Response::ShutdownComplete { .. } => {}
        }
    }

    pub fn take_resizes(&mut self) -> Vec<(String, u16, u16)> {
        std::mem::take(&mut self.pending_resizes)
    }

    pub fn process_paste(&self, text: String) -> Action {
        self.selected_pane_id()
            .map(|pane_id| {
                let mut data = b"\x1b[200~".to_vec();
                data.extend(text.as_bytes());
                data.extend(b"\x1b[201~");
                Action::Send(Request::Input { pane_id, data })
            })
            .unwrap_or(Action::None)
    }

    pub fn process_mouse(&mut self, event: MouseEvent) -> Action {
        if !self.mouse_enabled {
            return Action::None;
        }
        if let Some((workspace_id, _)) = self
            .last_workspace_rows
            .iter()
            .find(|(_, area)| area.contains((event.column, event.row).into()))
            .cloned()
            && matches!(
                event.kind,
                MouseEventKind::Down(crossterm::event::MouseButton::Left)
            )
        {
            self.workspace_id = workspace_id.clone();
            self.selected_pane = 0;
            return Action::Send(Request::SwitchWorkspace { workspace_id });
        }
        let hit = self
            .last_pane_rects
            .iter()
            .find(|(_, area)| area.contains((event.column, event.row).into()))
            .cloned();
        let Some((pane_id, area)) = hit else {
            return Action::None;
        };
        if let Some(index) = self
            .current_workspace()
            .and_then(|workspace| workspace.panes.iter().position(|pane| pane.id == pane_id))
        {
            self.selected_pane = index;
        }
        let mouse_reporting = self.parsers.get(&pane_id).is_some_and(|parser| {
            parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
        });
        if mouse_reporting && !self.navigation {
            let button = match event.kind {
                MouseEventKind::Down(crossterm::event::MouseButton::Left) => 0,
                MouseEventKind::Down(crossterm::event::MouseButton::Middle) => 1,
                MouseEventKind::Down(crossterm::event::MouseButton::Right) => 2,
                MouseEventKind::Up(_) => 3,
                MouseEventKind::ScrollUp => 64,
                MouseEventKind::ScrollDown => 65,
                _ => return Action::None,
            };
            let suffix = if matches!(event.kind, MouseEventKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            let x = event.column.saturating_sub(area.x).max(1);
            let y = event.row.saturating_sub(area.y).max(1);
            return Action::Send(Request::Input {
                pane_id,
                data: format!("\x1b[<{button};{x};{y}{suffix}").into_bytes(),
            });
        }
        match event.kind {
            MouseEventKind::ScrollUp => *self.scroll_offsets.entry(pane_id).or_default() += 3,
            MouseEventKind::ScrollDown => {
                let offset = self.scroll_offsets.entry(pane_id).or_default();
                *offset = offset.saturating_sub(3);
            }
            _ => {}
        }
        Action::None
    }

    pub fn process_key(&mut self, key: KeyEvent) -> Action {
        if self.copy_search_input {
            match key.code {
                KeyCode::Enter => {
                    self.copy_search_input = false;
                    self.toast = format!("Copy search · {}", self.copy_search);
                }
                KeyCode::Esc => {
                    self.copy_search_input = false;
                    self.copy_search.clear();
                }
                KeyCode::Backspace => {
                    self.copy_search.pop();
                }
                KeyCode::Char(character) => self.copy_search.push(character),
                _ => {}
            }
            return Action::None;
        }
        if self.send_toggle_pending {
            self.send_toggle_pending = false;
            return self
                .selected_pane_id()
                .map(|pane_id| {
                    Action::Send(Request::Input {
                        pane_id,
                        data: key_to_bytes(key),
                    })
                })
                .unwrap_or(Action::None);
        }
        if is_navigation_toggle(key, &self.keymap.toggle_mode) {
            self.navigation = !self.navigation;
            self.prompt_mode = false;
            self.creating_workspace = false;
            self.toast = if self.navigation {
                "Navigation mode".into()
            } else {
                "Agent focus · Ctrl-\\ or F12 to return".into()
            };
            return Action::None;
        }
        if let Some(axis) = self.launcher_axis {
            let command = match key.code {
                KeyCode::Char('1') => Some(vec![
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                    "-l".into(),
                ]),
                KeyCode::Char('2') if executable_on_path("claude") => Some(vec!["claude".into()]),
                KeyCode::Char('3') if executable_on_path("codex") => Some(vec!["codex".into()]),
                KeyCode::Char('4') if executable_on_path("hermes") => Some(vec!["hermes".into()]),
                KeyCode::Char(character @ 'a'..='z') => self
                    .launch_templates
                    .get(usize::from(character as u8 - b'a'))
                    .map(|template| template.command.clone()),
                KeyCode::Esc => {
                    self.launcher_axis = None;
                    return Action::None;
                }
                _ => None,
            };
            if let Some(command) = command {
                self.launcher_axis = None;
                return self.create_pane(axis, command);
            }
            self.toast = "Launcher · 1 shell · 2 claude · 3 codex · 4 hermes · Esc cancel".into();
            return Action::None;
        }
        if self.pending_workspace_name.is_some() {
            let command = match key.code {
                KeyCode::Char('1') => Some(vec![
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
                    "-l".into(),
                ]),
                KeyCode::Char('2') if executable_on_path("claude") => Some(vec!["claude".into()]),
                KeyCode::Char('3') if executable_on_path("codex") => Some(vec!["codex".into()]),
                KeyCode::Char('4') if executable_on_path("hermes") => Some(vec!["hermes".into()]),
                KeyCode::Char(character @ 'a'..='z') => self
                    .launch_templates
                    .get(usize::from(character as u8 - b'a'))
                    .map(|template| template.command.clone()),
                KeyCode::Esc => {
                    self.pending_workspace_name = None;
                    return Action::None;
                }
                _ => None,
            };
            if let Some(command) = command {
                let name = self
                    .pending_workspace_name
                    .take()
                    .unwrap_or_else(|| "workspace".into());
                let cwd = self
                    .selected_pane()
                    .map(|pane| pane.cwd.clone())
                    .unwrap_or_else(|| ".".into());
                return Action::Send(Request::CreateWorkspace {
                    name,
                    cwd,
                    command,
                    respawn: false,
                });
            }
            self.toast = "Workspace launcher · 1 shell · 2 claude · 3 codex · 4 hermes".into();
            return Action::None;
        }
        if self.confirm_close {
            self.confirm_close = false;
            return match key.code {
                KeyCode::Char('y' | 'Y') => self
                    .selected_pane_id()
                    .map(|pane_id| {
                        self.toast = "Closing pane…".into();
                        Action::Send(Request::ClosePane { pane_id })
                    })
                    .unwrap_or(Action::None),
                _ => {
                    self.toast = "Pane close cancelled".into();
                    Action::None
                }
            };
        }
        if self.prompt_mode {
            return self.process_prompt_key(key);
        }
        if !self.navigation {
            if self.send_toggle_pending {
                self.send_toggle_pending = false;
            }
            return self
                .selected_pane_id()
                .map(|pane_id| {
                    let application_cursor = self
                        .parsers
                        .get(&pane_id)
                        .is_some_and(|parser| parser.screen().application_cursor());
                    Action::Send(Request::Input {
                        pane_id,
                        data: key_to_bytes_mode(key, application_cursor),
                    })
                })
                .unwrap_or(Action::None);
        }

        if self.leader_pending {
            self.leader_pending = false;
            match key.code {
                KeyCode::Char('a') => self.set_view(View::Attention),
                KeyCode::Char('s') => {
                    self.toast = "Schedules are managed with `muxloom schedule`".into()
                }
                KeyCode::Char('?') => self.set_view(View::Help),
                KeyCode::Char('t') => {
                    self.send_toggle_pending = true;
                    self.navigation = false;
                    self.toast = "Send-prefix: next key passes to the agent".into();
                }
                _ => self.toast = "Unknown leader command".into(),
            }
            return Action::None;
        }
        if binding_matches(key, &self.keymap.detach) {
            return Action::Quit;
        }
        if binding_matches(key, &self.keymap.leader) {
            self.leader_pending = true;
            self.toast = "Leader · a attention · s schedules · ? help · t send-prefix".into();
            return Action::None;
        }
        if self.view == View::Workspace && binding_matches(key, &self.keymap.zoom) {
            self.zoomed = !self.zoomed;
            return Action::None;
        }
        if self.view == View::Workspace && binding_matches(key, &self.keymap.toggle_rail) {
            self.show_rail = !self.show_rail;
            return Action::None;
        }
        if self.view == View::Workspace && binding_matches(key, &self.keymap.toggle_context) {
            self.show_context = !self.show_context;
            return Action::None;
        }

        if let Some(chord) = self.pending_chord.take()
            && key.code == KeyCode::Char('a')
        {
            return self.jump_attention(chord == ']');
        }
        if self.view == View::Attention {
            let active = self
                .attention
                .iter()
                .enumerate()
                .filter(|(_, event)| event.read_at.is_none())
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if !active.is_empty() {
                        self.attention_index = (self.attention_index + 1) % active.len();
                    }
                    return Action::None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !active.is_empty() {
                        self.attention_index = self
                            .attention_index
                            .checked_sub(1)
                            .unwrap_or(active.len().saturating_sub(1));
                    }
                    return Action::None;
                }
                KeyCode::Enter => {
                    if let Some(&index) = active.get(self.attention_index) {
                        let pane_id = self.attention[index].pane_id.clone();
                        if let Some((workspace_id, pane_index)) =
                            self.workspaces.iter().find_map(|workspace| {
                                workspace
                                    .panes
                                    .iter()
                                    .position(|pane| pane.id == pane_id)
                                    .map(|index| (workspace.id.clone(), index))
                            })
                        {
                            self.workspace_id = workspace_id.clone();
                            self.selected_pane = pane_index;
                            self.view = View::Workspace;
                            return Action::Send(Request::SwitchWorkspace { workspace_id });
                        }
                    }
                    return Action::None;
                }
                KeyCode::Char('r') => {
                    return active
                        .get(self.attention_index)
                        .map(|&index| {
                            Action::Send(Request::MarkAttentionRead {
                                id: self.attention[index].id.clone(),
                            })
                        })
                        .unwrap_or(Action::None);
                }
                KeyCode::Char('d') => {
                    return active
                        .get(self.attention_index)
                        .map(|&index| {
                            Action::Send(Request::DismissAttention {
                                id: self.attention[index].id.clone(),
                            })
                        })
                        .unwrap_or(Action::None);
                }
                KeyCode::Char('R') => return Action::Send(Request::MarkAllAttentionRead),
                _ => {}
            }
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('/'), _)
                if self
                    .selected_pane_id()
                    .as_ref()
                    .and_then(|id| self.scroll_offsets.get(id))
                    .copied()
                    .unwrap_or(0)
                    > 0 =>
            {
                self.copy_search_input = true;
                self.copy_search.clear();
                self.toast = "Copy search · type query and press Enter".into();
            }
            (KeyCode::Char('v'), _)
                if self
                    .selected_pane_id()
                    .as_ref()
                    .and_then(|id| self.scroll_offsets.get(id))
                    .copied()
                    .unwrap_or(0)
                    > 0 =>
            {
                self.visual_select = true;
                self.toast = "Visual copy selection · y yank visible selection".into();
            }
            (KeyCode::Char('y'), _) if self.visual_select => {
                self.visual_select = false;
                if let Some(pane_id) = self.selected_pane_id()
                    && let Some(parser) = self.parsers.get(&pane_id)
                {
                    let encoded = base64::engine::general_purpose::STANDARD
                        .encode(parser.screen().contents());
                    return Action::Osc52(format!("\x1b]52;c;{encoded}\x07"));
                }
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                if let Some(pane_id) = self.selected_pane_id() {
                    let offset = self.scroll_offsets.entry(pane_id).or_default();
                    *offset = offset.saturating_add(10);
                    self.toast =
                        "Copy mode · PgUp/PgDn g/G · y copies visible text · Esc live".into();
                }
            }
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                if let Some(pane_id) = self.selected_pane_id() {
                    let offset = self.scroll_offsets.entry(pane_id).or_default();
                    *offset = offset.saturating_sub(10);
                }
            }
            (KeyCode::Char(number @ '1'..='9'), _) => {
                let index = number.to_digit(10).unwrap_or(1) as usize - 1;
                if let Some(workspace) = self.workspaces.get(index) {
                    self.workspace_id = workspace.id.clone();
                    self.selected_pane = 0;
                    return Action::Send(Request::SwitchWorkspace {
                        workspace_id: self.workspace_id.clone(),
                    });
                }
            }
            (KeyCode::Char('A'), _) => self.set_view(View::Attention),
            (KeyCode::Char('?'), _) => self.set_view(View::Help),
            (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => self.select_spatial(0, 1),
            (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => self.select_spatial(0, -1),
            (KeyCode::BackTab, _) => {
                return self.switch_workspace(false);
            }
            (KeyCode::Tab, _) => {
                return self.switch_workspace(true);
            }
            (KeyCode::Char('h'), _) => self.select_spatial(-1, 0),
            (KeyCode::Char('l'), _) => self.select_spatial(1, 0),
            (KeyCode::Char('g'), _)
                if self
                    .selected_pane_id()
                    .as_ref()
                    .and_then(|id| self.scroll_offsets.get(id))
                    .copied()
                    .unwrap_or(0)
                    > 0 =>
            {
                if let Some(pane_id) = self.selected_pane_id() {
                    self.scroll_offsets.insert(pane_id, usize::MAX / 2);
                }
            }
            (KeyCode::Char('G'), _)
                if self
                    .selected_pane_id()
                    .as_ref()
                    .and_then(|id| self.scroll_offsets.get(id))
                    .copied()
                    .unwrap_or(0)
                    > 0 =>
            {
                if let Some(pane_id) = self.selected_pane_id() {
                    self.scroll_offsets.insert(pane_id, 0);
                }
            }
            (KeyCode::Char('g'), _) => self.selected_pane = 0,
            (KeyCode::Char('G'), _) => {
                self.selected_pane = self
                    .current_workspace()
                    .map_or(0, |workspace| workspace.panes.len().saturating_sub(1));
            }
            (KeyCode::Enter | KeyCode::Char('i'), _) if self.view == View::Workspace => {
                self.navigation = false;
                self.toast = "Agent focus · Ctrl-\\ or F12 to return".into();
            }
            (KeyCode::Char('p'), _) if self.view == View::Workspace => {
                self.prompt_mode = true;
                self.creating_workspace = false;
                self.prompt.clear();
            }
            (KeyCode::Char('c'), _) if self.view == View::Workspace => {
                self.prompt_mode = true;
                self.creating_workspace = true;
                self.prompt.clear();
                self.toast = "Name the new workspace".into();
            }
            (KeyCode::Char('v'), _) if self.view == View::Workspace => {
                self.launcher_axis = Some(SplitAxis::Vertical);
                self.toast = "Vertical split · 1 shell · 2 claude · 3 codex · 4 hermes".into();
            }
            (KeyCode::Char('s'), _) if self.view == View::Workspace => {
                self.launcher_axis = Some(SplitAxis::Horizontal);
                self.toast = "Horizontal split · 1 shell · 2 claude · 3 codex · 4 hermes".into();
            }
            (KeyCode::Char('<') | KeyCode::Left, KeyModifiers::CONTROL)
                if self.view == View::Workspace =>
            {
                return self.resize_selected(-5);
            }
            (KeyCode::Char('>') | KeyCode::Right, KeyModifiers::CONTROL)
                if self.view == View::Workspace =>
            {
                return self.resize_selected(5);
            }
            (KeyCode::Char('-') | KeyCode::Up, KeyModifiers::CONTROL)
                if self.view == View::Workspace =>
            {
                return self.resize_selected(-5);
            }
            (KeyCode::Char('+') | KeyCode::Down, KeyModifiers::CONTROL)
                if self.view == View::Workspace =>
            {
                return self.resize_selected(5);
            }
            (KeyCode::Char('x'), _) if self.view == View::Workspace => {
                if self.selected_pane_id().is_some() {
                    self.confirm_close = true;
                    self.toast = "Close selected pane? y confirms · any key cancels".into();
                }
            }
            (KeyCode::Char(character @ ('[' | ']')), _) => self.pending_chord = Some(character),
            (KeyCode::Esc, _) => {
                if let Some(pane_id) = self.selected_pane_id() {
                    self.scroll_offsets.insert(pane_id, 0);
                }
                self.set_view(View::Workspace);
            }
            _ => {}
        }
        Action::None
    }

    fn process_prompt_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.prompt_mode = false;
                self.creating_workspace = false;
                self.prompt.clear();
                Action::None
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prompt.push('\n');
                    return Action::None;
                }
                self.prompt_mode = false;
                if self.creating_workspace {
                    self.creating_workspace = false;
                    let name = if self.prompt.trim().is_empty() {
                        "workspace".to_string()
                    } else {
                        self.prompt.trim().to_string()
                    };
                    self.prompt.clear();
                    self.pending_workspace_name = Some(name);
                    self.toast =
                        "Workspace launcher · 1 shell · 2 claude · 3 codex · 4 hermes".into();
                    return Action::None;
                }
                let submitted = self.prompt.clone();
                let mut data = b"\x1b[200~".to_vec();
                data.extend(submitted.as_bytes());
                data.extend(b"\x1b[201~\r");
                if !submitted.is_empty() {
                    self.prompt_history.push(submitted);
                    if self.prompt_history.len() > 100 {
                        self.prompt_history.remove(0);
                    }
                    if self.persist_prompt_history {
                        let _ = persist_prompt_history(&self.prompt_history);
                    }
                }
                self.prompt.clear();
                if self.prompt_broadcast {
                    self.prompt_broadcast = false;
                    let requests = self.current_workspace().map_or_else(Vec::new, |workspace| {
                        workspace
                            .panes
                            .iter()
                            .filter(|pane| pane.exited_at.is_none())
                            .map(|pane| Request::Input {
                                pane_id: pane.id.clone(),
                                data: data.clone(),
                            })
                            .collect()
                    });
                    Action::SendMany(requests)
                } else {
                    self.prompt_target
                        .take()
                        .or_else(|| self.selected_pane_id())
                        .map(|pane_id| Action::Send(Request::Input { pane_id, data }))
                        .unwrap_or(Action::None)
                }
            }
            KeyCode::Up => {
                if !self.prompt_history.is_empty() {
                    let index = self
                        .prompt_history_index
                        .unwrap_or(self.prompt_history.len())
                        .saturating_sub(1);
                    self.prompt_history_index = Some(index);
                    self.prompt = self.prompt_history[index].clone();
                }
                Action::None
            }
            KeyCode::Down => {
                if let Some(index) = self.prompt_history_index {
                    let next = (index + 1).min(self.prompt_history.len());
                    self.prompt_history_index = (next < self.prompt_history.len()).then_some(next);
                    self.prompt = self
                        .prompt_history_index
                        .map_or_else(String::new, |index| self.prompt_history[index].clone());
                }
                Action::None
            }
            KeyCode::Backspace => {
                self.prompt.pop();
                Action::None
            }
            KeyCode::Char(character) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && character == 'b' {
                    self.prompt_broadcast = !self.prompt_broadcast;
                    self.toast = if self.prompt_broadcast {
                        "Prompt target · all panes"
                    } else {
                        "Prompt target · selected pane"
                    }
                    .into();
                    return Action::None;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && character == 't' {
                    let panes = self.current_workspace().map_or_else(Vec::new, |workspace| {
                        workspace
                            .panes
                            .iter()
                            .filter(|pane| pane.exited_at.is_none())
                            .map(|pane| (pane.id.clone(), pane.title.clone()))
                            .collect::<Vec<_>>()
                    });
                    if !panes.is_empty() {
                        let current = self
                            .prompt_target
                            .as_ref()
                            .and_then(|id| panes.iter().position(|(pane_id, _)| pane_id == id))
                            .unwrap_or(0);
                        let next = (current + 1) % panes.len();
                        self.prompt_target = Some(panes[next].0.clone());
                        self.toast = format!("Prompt target · {}", panes[next].1);
                    }
                    return Action::None;
                }
                self.prompt.push(character);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.toast = view_label(view).to_string();
    }

    fn create_pane(&self, axis: SplitAxis, command: Vec<String>) -> Action {
        let cwd = self
            .selected_pane()
            .map(|pane| pane.cwd.clone())
            .unwrap_or_else(|| ".".into());
        Action::Send(Request::CreatePane {
            workspace_id: self.workspace_id.clone(),
            cwd,
            title: command.first().cloned(),
            command,
            split_from: self.selected_pane_id(),
            split_axis: Some(axis),
        })
    }

    fn select_spatial(&mut self, dx: i32, dy: i32) {
        let Some(selected_id) = self.selected_pane_id() else {
            return;
        };
        let Some((_, selected)) = self
            .last_pane_rects
            .iter()
            .find(|(id, _)| id == &selected_id)
        else {
            if dy > 0 || dx > 0 {
                self.select_next();
            } else {
                self.select_previous();
            }
            return;
        };
        let sx = i32::from(selected.x) + i32::from(selected.width) / 2;
        let sy = i32::from(selected.y) + i32::from(selected.height) / 2;
        let candidate = self
            .last_pane_rects
            .iter()
            .filter_map(|(id, rect)| {
                if id == &selected_id {
                    return None;
                }
                let x = i32::from(rect.x) + i32::from(rect.width) / 2;
                let y = i32::from(rect.y) + i32::from(rect.height) / 2;
                let forward = (dx < 0 && x < sx)
                    || (dx > 0 && x > sx)
                    || (dy < 0 && y < sy)
                    || (dy > 0 && y > sy);
                forward.then_some((id, (x - sx).abs() + (y - sy).abs()))
            })
            .min_by_key(|(_, distance)| *distance);
        if let Some((id, _)) = candidate
            && let Some(index) = self
                .current_workspace()
                .and_then(|workspace| workspace.panes.iter().position(|pane| &pane.id == id))
        {
            self.selected_pane = index;
        }
    }

    fn resize_selected(&mut self, delta: i8) -> Action {
        let Some(pane_id) = self.selected_pane_id() else {
            return Action::None;
        };
        let workspace_id = self.workspace_id.clone();
        let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return Action::None;
        };
        let Some(mut layout) = workspace.layout.clone() else {
            return Action::None;
        };
        if adjust_layout_ratio(&mut layout, &pane_id, delta) {
            workspace.layout = Some(layout.clone());
            Action::Send(Request::SetLayout {
                workspace_id,
                layout,
            })
        } else {
            Action::None
        }
    }

    fn switch_workspace(&mut self, forward: bool) -> Action {
        if self.workspaces.len() < 2 {
            self.toast = "Only one workspace · press c to create another".into();
            return Action::None;
        }
        let current = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == self.workspace_id)
            .unwrap_or(0);
        let index = if forward {
            (current + 1) % self.workspaces.len()
        } else if current == 0 {
            self.workspaces.len() - 1
        } else {
            current - 1
        };
        self.workspace_id = self.workspaces[index].id.clone();
        self.selected_pane = 0;
        self.view = View::Workspace;
        self.toast = format!("Workspace · {}", self.workspaces[index].name);
        Action::Send(Request::SwitchWorkspace {
            workspace_id: self.workspace_id.clone(),
        })
    }

    fn jump_attention(&mut self, forward: bool) -> Action {
        let active = self
            .attention
            .iter()
            .filter(|event| event.read_at.is_none())
            .collect::<Vec<_>>();
        if active.is_empty() {
            self.toast = "Attention queue is clear".into();
            return Action::None;
        }
        let current_id = self.selected_pane_id();
        let current = active
            .iter()
            .position(|event| Some(event.pane_id.clone()) == current_id)
            .unwrap_or(0);
        let index = if forward {
            (current + 1) % active.len()
        } else if current == 0 {
            active.len() - 1
        } else {
            current - 1
        };
        let event = active[index];
        if let Some((workspace_id, pane_index)) = self.workspaces.iter().find_map(|workspace| {
            workspace
                .panes
                .iter()
                .position(|pane| pane.id == event.pane_id)
                .map(|index| (workspace.id.clone(), index))
        }) {
            self.workspace_id = workspace_id.clone();
            self.selected_pane = pane_index;
            self.view = View::Workspace;
            return Action::Send(Request::SwitchWorkspace { workspace_id });
        }
        Action::None
    }

    fn select_next(&mut self) {
        let count = self
            .current_workspace()
            .map_or(0, |workspace| workspace.panes.len());
        if count > 0 {
            self.selected_pane = (self.selected_pane + 1) % count;
        }
    }

    fn select_previous(&mut self) {
        let count = self
            .current_workspace()
            .map_or(0, |workspace| workspace.panes.len());
        if count > 0 {
            self.selected_pane = if self.selected_pane == 0 {
                count - 1
            } else {
                self.selected_pane - 1
            };
        }
    }

    fn clamp_selection(&mut self) {
        let maximum = self
            .current_workspace()
            .map_or(0, |workspace| workspace.panes.len().saturating_sub(1));
        self.selected_pane = self.selected_pane.min(maximum);
    }

    fn ensure_parsers(&mut self) {
        for pane in self
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.panes)
        {
            self.parsers
                .entry(pane.id.clone())
                .or_insert_with(|| vt100::Parser::new(30, 120, 5_000));
        }
    }

    pub fn current_workspace(&self) -> Option<&WorkspaceSummary> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == self.workspace_id)
    }

    fn selected_pane(&self) -> Option<&crate::model::PaneSummary> {
        self.current_workspace()?.panes.get(self.selected_pane)
    }

    pub fn selected_pane_id(&self) -> Option<String> {
        self.selected_pane().map(|pane| pane.id.clone())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn selected_running_pane_id(&self) -> Option<String> {
        self.selected_pane()
            .filter(|pane| pane.exited_at.is_none() && pane.state != AgentState::Exited)
            .map(|pane| pane.id.clone())
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(INK)), area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(if self.prompt_mode { 4 } else { 2 }),
            ])
            .split(area);
        self.render_header(frame, rows[0]);
        self.render_body(frame, rows[1]);
        self.render_footer(frame, rows[2]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let attention = self
            .attention
            .iter()
            .filter(|event| event.read_at.is_none())
            .count();
        let working = self.current_workspace().map_or(0, |workspace| {
            workspace
                .panes
                .iter()
                .filter(|pane| matches!(pane.state, AgentState::Starting | AgentState::Working))
                .count()
        });
        let workspace = self
            .current_workspace()
            .map_or("No workspace", |workspace| workspace.name.as_str());
        let line = Line::from(vec![
            Span::styled(
                "  MUX",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "LOOM",
                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {workspace}"),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("    ● {working} working"),
                Style::default().fg(SUCCESS),
            ),
            Span::styled(
                format!("    ◆ {attention} attention"),
                Style::default().fg(if attention > 0 { WARNING } else { MUTED }),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line)
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM)
                        .border_style(Style::default().fg(BORDER)),
                )
                .style(Style::default().bg(SURFACE)),
            area,
        );
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect) {
        let show_rail = self.show_rail && area.width >= 76 && !self.zoomed;
        let show_context =
            self.show_context && area.width >= 132 && self.view == View::Workspace && !self.zoomed;
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(match (show_rail, show_context) {
                (true, true) => vec![
                    Constraint::Length(28),
                    Constraint::Min(40),
                    Constraint::Length(34),
                ],
                (true, false) => vec![Constraint::Length(26), Constraint::Min(40)],
                (false, true) => vec![Constraint::Min(30), Constraint::Length(34)],
                (false, false) => vec![Constraint::Min(30)],
            })
            .split(area);
        let mut center_index = 0;
        if show_rail {
            self.render_rail(frame, columns[0]);
            center_index = 1;
        }
        match self.view {
            View::Workspace => self.render_terminals(frame, columns[center_index]),
            View::Attention => self.render_attention(frame, columns[center_index]),
            View::Usage => self.render_usage(frame, columns[center_index]),
            View::Skills => self.render_skills(frame, columns[center_index]),
            View::Config => self.render_config(frame, columns[center_index]),
            View::Schedules => self.render_schedules(frame, columns[center_index]),
            View::Hermes => self.render_hermes(frame, columns[center_index]),
            View::Help => self.render_help(frame, columns[center_index]),
        }
        if show_context {
            self.render_context(frame, columns[center_index + 1]);
        }
    }

    fn render_rail(&mut self, frame: &mut Frame, area: Rect) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(area);
        let workspace_items = self
            .workspaces
            .iter()
            .map(|workspace| {
                let selected = workspace.id == self.workspace_id;
                let attention = workspace.panes.iter().filter(|pane| pane.unread).count();
                let running = workspace
                    .panes
                    .iter()
                    .filter(|pane| pane.exited_at.is_none() && pane.state != AgentState::Exited)
                    .count();
                let style = if selected {
                    Style::default()
                        .fg(INK)
                        .bg(VIOLET)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT)
                };
                let marker = if selected { "›" } else { " " };
                let badge = if attention > 0 {
                    format!(" ◆{attention}")
                } else if running > 0 {
                    format!(" ●{running}")
                } else {
                    " ·".into()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {marker} "), style),
                    Span::styled(workspace.name.clone(), style),
                    Span::styled(badge, style),
                ]))
                .style(style)
            })
            .collect::<Vec<_>>();
        let selected_workspace = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == self.workspace_id);
        self.last_workspace_rows = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                (
                    workspace.id.clone(),
                    Rect::new(
                        sections[0].x,
                        sections[0].y.saturating_add(1 + index as u16),
                        sections[0].width,
                        1,
                    ),
                )
            })
            .collect();
        let mut workspace_state = ListState::default().with_selected(selected_workspace);
        frame.render_stateful_widget(
            List::new(workspace_items)
                .block(
                    Block::default()
                        .title(" WORKSPACES · c new ")
                        .title_style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD))
                        .borders(Borders::RIGHT)
                        .border_style(Style::default().fg(BORDER)),
                )
                .style(Style::default().bg(SURFACE)),
            sections[0],
            &mut workspace_state,
        );

        let pane_items = self.current_workspace().map_or_else(Vec::new, |workspace| {
            workspace
                .panes
                .iter()
                .enumerate()
                .map(|(index, pane)| {
                    let selected = index == self.selected_pane;
                    let symbol = state_symbol(pane.state);
                    let style = if selected {
                        Style::default()
                            .fg(INK)
                            .bg(ACCENT)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(state_color(pane.state))
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" {symbol} "), style),
                        Span::styled(pane.title.clone(), style),
                    ]))
                    .style(style)
                })
                .collect()
        });
        frame.render_widget(
            List::new(pane_items)
                .block(
                    Block::default()
                        .title(" PANES · v/s split ")
                        .title_style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD))
                        .borders(Borders::TOP | Borders::RIGHT)
                        .border_style(Style::default().fg(BORDER)),
                )
                .style(Style::default().bg(SURFACE)),
            sections[1],
        );
    }

    fn render_terminals(&mut self, frame: &mut Frame, area: Rect) {
        let Some(workspace) = self.current_workspace() else {
            return;
        };
        if workspace.panes.is_empty() {
            frame.render_widget(
                empty_state("No panes", "Press v or s to open a shell"),
                area,
            );
            return;
        }
        let pane_meta = workspace
            .panes
            .iter()
            .map(|pane| (pane.id.clone(), pane.title.clone(), pane.state))
            .collect::<Vec<_>>();
        let layout = workspace
            .layout
            .clone()
            .unwrap_or_else(|| flat_layout(&pane_meta));
        let mut sections = Vec::new();
        layout_rects(&layout, area, &mut sections);
        if self.zoomed
            && let Some(selected) = pane_meta.get(self.selected_pane)
        {
            sections.clear();
            sections.push((selected.0.clone(), area));
        }
        self.last_pane_rects = sections.clone();
        let mut cursor = None;
        for (pane_id, section) in sections {
            let Some((index, (_, title_text, pane_state))) = pane_meta
                .iter()
                .enumerate()
                .find(|(_, (id, _, _))| id == &pane_id)
            else {
                continue;
            };
            let selected = index == self.selected_pane;
            let inner = section.inner(Margin {
                horizontal: 1,
                vertical: 1,
            });
            let parser = self.parsers.entry(pane_id.clone()).or_insert_with(|| {
                vt100::Parser::new(inner.height.max(2), inner.width.max(10), 5_000)
            });
            let size = (inner.height.max(2), inner.width.max(10));
            if self.last_sizes.get(&pane_id).copied() != Some(size) {
                parser.screen_mut().set_size(size.0, size.1);
                self.last_sizes.insert(pane_id.clone(), size);
                self.pending_resizes.push((pane_id.clone(), size.0, size.1));
            }
            let offset = self.scroll_offsets.get(&pane_id).copied().unwrap_or(0);
            parser.screen_mut().set_scrollback(offset);
            let title = format!(
                " {}  {}{} ",
                state_symbol(*pane_state),
                title_text,
                if offset > 0 { " · COPY" } else { "" }
            );
            let border = if selected { ACCENT } else { BORDER };
            frame.render_widget(
                Block::default()
                    .title(title)
                    .title_style(
                        Style::default()
                            .fg(state_color(*pane_state))
                            .add_modifier(Modifier::BOLD),
                    )
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border)),
                section,
            );
            render_vt100(frame, inner, parser.screen(), &self.copy_search);
            if selected && !self.navigation && !parser.screen().hide_cursor() {
                let (row, col) = parser.screen().cursor_position();
                if row < inner.height && col < inner.width {
                    cursor = Some((inner.x + col, inner.y + row));
                }
            }
        }
        if let Some(position) = cursor {
            frame.set_cursor_position(position);
        }
    }

    fn render_context(&self, frame: &mut Frame, area: Rect) {
        let Some(pane) = self.selected_pane() else {
            return;
        };
        let attention = self
            .attention
            .iter()
            .rev()
            .find(|event| event.pane_id == pane.id && event.read_at.is_none());
        let git = git_summary(&pane.cwd);
        let pid = pane.pid.map_or_else(|| "—".into(), |pid| pid.to_string());
        let mut lines = vec![
            labelled("STATE", pane.state.label(), state_color(pane.state)),
            labelled("PROVIDER", &pane.provider, VIOLET),
            labelled("PID", &pid, TEXT),
            Line::default(),
            Line::styled(
                "PROJECT",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Line::styled(pane.cwd.clone(), Style::default().fg(TEXT)),
            Line::styled(git, Style::default().fg(SUCCESS)),
            Line::default(),
            Line::styled(
                "ATTENTION",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ];
        lines.push(Line::styled(
            attention.map_or("Nothing pending", |event| event.summary.as_str()),
            Style::default().fg(if attention.is_some() { WARNING } else { MUTED }),
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(" CONTEXT ")
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(BORDER)),
                )
                .style(Style::default().bg(SURFACE)),
            area,
        );
    }

    fn render_attention(&self, frame: &mut Frame, area: Rect) {
        let events = self
            .attention
            .iter()
            .filter(|event| event.read_at.is_none())
            .map(|event| {
                let age = Utc::now() - event.created_at;
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!("{}  ", state_symbol(event.kind)),
                            Style::default().fg(state_color(event.kind)),
                        ),
                        Span::styled(
                            event.summary.clone(),
                            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::styled(
                        format!(
                            "   {} · {} · {}s ago · {:?}",
                            event.provider,
                            &event.pane_id[..8.min(event.pane_id.len())],
                            age.num_seconds().max(0),
                            event.confidence
                        ),
                        Style::default().fg(MUTED),
                    ),
                    Line::default(),
                ])
            })
            .collect::<Vec<_>>();
        if events.is_empty() {
            frame.render_widget(
                empty_state("All clear", "No agent needs your attention"),
                area,
            );
        } else {
            let mut state = ListState::default().with_selected(Some(
                self.attention_index.min(events.len().saturating_sub(1)),
            ));
            frame.render_stateful_widget(
                List::new(events)
                    .highlight_style(Style::default().bg(RAISED).fg(ACCENT))
                    .block(section_block(
                        "ATTENTION · Enter jump · r read · d dismiss · R all read",
                    )),
                area,
                &mut state,
            );
        }
    }

    fn render_usage(&self, frame: &mut Frame, area: Rect) {
        let rows = self.current_workspace().map_or_else(Vec::new, |workspace| {
            workspace
                .panes
                .iter()
                .map(|pane| {
                    let usage = self.usage.iter().find(|usage| usage.pane_id == pane.id);
                    let elapsed = usage.map_or(0, |usage| usage.elapsed_seconds);
                    let tokens = usage
                        .and_then(|usage| usage.input_tokens.zip(usage.output_tokens))
                        .map_or_else(
                            || "tokens unavailable".into(),
                            |(input, output)| format!("{input} in · {output} out"),
                        );
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(
                                pane.title.clone(),
                                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("  {}", pane.provider),
                                Style::default().fg(VIOLET),
                            ),
                        ]),
                        Line::styled(
                            format!("{}m {:02}s · {tokens}", elapsed / 60, elapsed % 60),
                            Style::default().fg(MUTED),
                        ),
                        Line::default(),
                    ])
                })
                .collect()
        });
        frame.render_widget(
            List::new(rows).block(section_block("USAGE · PROVIDER-REPORTED ONLY")),
            area,
        );
    }

    fn render_skills(&self, frame: &mut Frame, area: Rect) {
        let rows = self
            .skills
            .iter()
            .map(|skill| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            if skill.valid { "✓ " } else { "! " },
                            Style::default().fg(if skill.valid { SUCCESS } else { DANGER }),
                        ),
                        Span::styled(
                            skill.name.clone(),
                            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  {}", skill.provider), Style::default().fg(VIOLET)),
                    ]),
                    Line::styled(skill.description.clone(), Style::default().fg(MUTED)),
                    Line::styled(skill.location.clone(), Style::default().fg(BORDER)),
                    Line::default(),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(rows).block(section_block(&format!(
                "SKILLS · {} DISCOVERED",
                self.skills.len()
            ))),
            area,
        );
    }

    fn render_config(&self, frame: &mut Frame, area: Rect) {
        let rows = self
            .configs
            .iter()
            .map(|source| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            if source.present { "● " } else { "○ " },
                            Style::default().fg(if source.present { SUCCESS } else { MUTED }),
                        ),
                        Span::styled(
                            source.provider.to_uppercase(),
                            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            if source.writable {
                                "  writable"
                            } else {
                                "  read-only"
                            },
                            Style::default().fg(if source.writable { ACCENT } else { WARNING }),
                        ),
                    ]),
                    Line::styled(
                        source.location.display().to_string(),
                        Style::default().fg(MUTED),
                    ),
                    Line::default(),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(rows).block(section_block("CONFIGURATION · SECRETS REDACTED")),
            area,
        );
    }

    fn render_schedules(&self, frame: &mut Frame, area: Rect) {
        if self.schedules.is_empty() {
            frame.render_widget(
                empty_state("No schedules", "Create one with muxloom schedule add"),
                area,
            );
            return;
        }
        let rows = self
            .schedules
            .iter()
            .map(|schedule| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            if schedule.enabled { "● " } else { "○ " },
                            Style::default().fg(if schedule.enabled { SUCCESS } else { MUTED }),
                        ),
                        Span::styled(
                            schedule.name.clone(),
                            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::styled(
                        format!(
                            "{} · {} · {}",
                            schedule.expression,
                            schedule.timezone,
                            schedule.command.join(" ")
                        ),
                        Style::default().fg(MUTED),
                    ),
                    Line::default(),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(rows).block(section_block("SCHEDULES · NO OVERLAP")),
            area,
        );
    }

    fn render_hermes(&self, frame: &mut Frame, area: Rect) {
        if !self.hermes.available {
            frame.render_widget(
                empty_state(
                    "Hermes not found",
                    "Install Hermes Agent to enable this control surface",
                ),
                area,
            );
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Percentage(42),
                Constraint::Percentage(58),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    self.hermes.version.clone(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "Terminal-native status · no web services started",
                    Style::default().fg(MUTED),
                ),
            ])
            .block(section_block("HERMES")),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(self.hermes.status.clone())
                .wrap(Wrap { trim: false })
                .block(section_block("COMPONENTS")),
            rows[1],
        );
        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        frame.render_widget(
            Paragraph::new(self.hermes.sessions.clone())
                .wrap(Wrap { trim: false })
                .block(section_block("SESSIONS")),
            bottom[0],
        );
        frame.render_widget(
            Paragraph::new(self.hermes.insights.clone())
                .wrap(Wrap { trim: false })
                .block(section_block("7-DAY INSIGHTS")),
            bottom[1],
        );
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let help = vec![
            Line::styled(
                "NAVIGATION",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::raw("j/k         select pane"),
            Line::raw("h/l · Tab   switch workspace"),
            Line::raw("Enter / i   focus selected agent"),
            Line::raw("Ctrl-\\ / F12 toggle focus / navigation"),
            Line::raw("c           create named workspace"),
            Line::raw("v / s       split with a new shell"),
            Line::raw("p           quick prompt composer"),
            Line::raw("[a / ]a     previous / next attention"),
            Line::raw("1…7         workspace and control views"),
            Line::raw("A           attention queue"),
            Line::raw("q           detach safely"),
            Line::default(),
            Line::styled(
                "PASSTHROUGH",
                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
            ),
            Line::raw(
                "All keys are sent directly to the PTY. Pane processes keep running after detach.",
            ),
        ];
        frame.render_widget(
            Paragraph::new(help)
                .wrap(Wrap { trim: false })
                .block(section_block("KEYBOARD GUIDE")),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        if self.prompt_mode {
            let title = if self.creating_workspace {
                " NEW WORKSPACE · Enter to create · Esc to cancel "
            } else {
                " QUICK PROMPT · Enter to send · Esc to cancel "
            };
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(self.prompt.as_str())
                    .block(
                        Block::default()
                            .title(title)
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .style(Style::default().fg(TEXT).bg(RAISED)),
                area,
            );
            return;
        }
        let mode = if self.navigation { " NAV " } else { " AGENT " };
        let hints = if self.navigation && area.width < 100 {
            " h/l ws   j/k pane   c new   v/s split   Enter focus   q detach "
        } else if self.navigation {
            " h/l workspace   j/k pane   c new   v/s split   Enter focus   p prompt   q detach "
        } else {
            " Input is passing directly to the agent · Ctrl-\\ or F12 returns to navigation "
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    mode,
                    Style::default()
                        .fg(INK)
                        .bg(if self.navigation { ACCENT } else { VIOLET })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(hints, Style::default().fg(MUTED)),
                Span::styled(format!(" · {}", self.toast), Style::default().fg(TEXT)),
            ]))
            .style(Style::default().bg(SURFACE)),
            area,
        );
    }
}

fn section_block(title: &str) -> Block<'_> {
    Block::default()
        .title(format!(" {title} "))
        .title_style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(INK))
}

fn flat_layout(panes: &[(String, String, AgentState)]) -> LayoutNode {
    let mut nodes = panes.iter().map(|(pane_id, _, _)| LayoutNode::Pane {
        pane_id: pane_id.clone(),
    });
    let first = nodes.next().unwrap_or_else(|| LayoutNode::Pane {
        pane_id: String::new(),
    });
    nodes.fold(first, |first, second| LayoutNode::Split {
        axis: SplitAxis::Vertical,
        ratio: 50,
        first: Box::new(first),
        second: Box::new(second),
    })
}

fn layout_rects(node: &LayoutNode, area: Rect, target: &mut Vec<(String, Rect)>) {
    match node {
        LayoutNode::Pane { pane_id } => target.push((pane_id.clone(), area)),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(10, 90);
            let sections = Layout::default()
                .direction(match axis {
                    SplitAxis::Horizontal => Direction::Vertical,
                    SplitAxis::Vertical => Direction::Horizontal,
                })
                .constraints([
                    Constraint::Percentage(u16::from(ratio)),
                    Constraint::Percentage(u16::from(100 - ratio)),
                ])
                .split(area);
            layout_rects(first, sections[0], target);
            layout_rects(second, sections[1], target);
        }
    }
}

fn adjust_layout_ratio(node: &mut LayoutNode, pane_id: &str, delta: i8) -> bool {
    let LayoutNode::Split {
        ratio,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };
    if adjust_layout_ratio(first, pane_id, delta) || adjust_layout_ratio(second, pane_id, delta) {
        return true;
    }
    let mut first_ids = Vec::new();
    first.pane_ids(&mut first_ids);
    let mut second_ids = Vec::new();
    second.pane_ids(&mut second_ids);
    if first_ids.iter().any(|id| id == pane_id) {
        *ratio = i16::from(*ratio)
            .saturating_add(i16::from(delta))
            .clamp(10, 90) as u8;
        true
    } else if second_ids.iter().any(|id| id == pane_id) {
        *ratio = i16::from(*ratio)
            .saturating_sub(i16::from(delta))
            .clamp(10, 90) as u8;
        true
    } else {
        false
    }
}

fn terminal_color(color: vt100::Color, fallback: Color) -> Color {
    match color {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn render_vt100(frame: &mut Frame, area: Rect, screen: &vt100::Screen, search: &str) {
    for row in 0..area.height {
        let row_matches = !search.is_empty()
            && (0..area.width)
                .filter_map(|col| screen.cell(row, col))
                .map(vt100::Cell::contents)
                .collect::<String>()
                .to_ascii_lowercase()
                .contains(&search.to_ascii_lowercase());
        for col in 0..area.width {
            let Some(source) = screen.cell(row, col) else {
                continue;
            };
            if source.is_wide_continuation() {
                continue;
            }
            let mut foreground = terminal_color(source.fgcolor(), TEXT);
            let mut background = terminal_color(source.bgcolor(), INK);
            if source.inverse() {
                std::mem::swap(&mut foreground, &mut background);
            }
            if row_matches {
                foreground = INK;
                background = WARNING;
            }
            let mut modifiers = Modifier::empty();
            if source.bold() {
                modifiers |= Modifier::BOLD;
            }
            if source.dim() {
                modifiers |= Modifier::DIM;
            }
            if source.italic() {
                modifiers |= Modifier::ITALIC;
            }
            if source.underline() {
                modifiers |= Modifier::UNDERLINED;
            }
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x + col, area.y + row)) {
                cell.set_symbol(if source.has_contents() {
                    source.contents()
                } else {
                    " "
                });
                cell.set_style(
                    Style::default()
                        .fg(foreground)
                        .bg(background)
                        .add_modifier(modifiers),
                );
            }
        }
    }
}

fn empty_state<'a>(title: &'a str, detail: &'a str) -> Paragraph<'a> {
    Paragraph::new(Text::from(vec![
        Line::default(),
        Line::styled(
            "◇",
            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            title,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(detail, Style::default().fg(MUTED)),
    ]))
    .alignment(Alignment::Center)
    .block(section_block("MUXLOOM"))
}

fn labelled<'a>(label: &'a str, value: &'a str, color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{label:<10}"),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value, Style::default().fg(color)),
    ])
}

const fn state_symbol(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting | AgentState::Working => "●",
        AgentState::WaitingApproval | AgentState::WaitingInput => "◆",
        AgentState::CompletedUnread => "✓",
        AgentState::Idle => "○",
        AgentState::Error => "!",
        AgentState::Exited | AgentState::Unknown => "·",
    }
}

const fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Starting | AgentState::Working => SUCCESS,
        AgentState::WaitingApproval | AgentState::WaitingInput => WARNING,
        AgentState::CompletedUnread => ACCENT,
        AgentState::Error => DANGER,
        AgentState::Idle | AgentState::Exited | AgentState::Unknown => MUTED,
    }
}

const fn view_label(view: View) -> &'static str {
    match view {
        View::Workspace => "Workspace",
        View::Attention => "Attention queue",
        View::Usage => "Usage",
        View::Skills => "Skills",
        View::Config => "Configuration",
        View::Schedules => "Schedules",
        View::Hermes => "Hermes",
        View::Help => "Help",
    }
}

fn is_navigation_toggle(key: KeyEvent, configured: &str) -> bool {
    if configured.eq_ignore_ascii_case("f12") {
        return key.code == KeyCode::F(12);
    }
    key.code == KeyCode::F(12)
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('\\' | '4')))
}

fn binding_matches(key: KeyEvent, configured: &str) -> bool {
    let configured = configured.to_ascii_lowercase();
    if configured == "space" {
        return key.code == KeyCode::Char(' ') && key.modifiers.is_empty();
    }
    if let Some(character) = configured
        .strip_prefix("ctrl-")
        .and_then(|value| value.chars().next())
    {
        return key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(actual) if actual.to_ascii_lowercase() == character);
    }
    if let Some(number) = configured
        .strip_prefix('f')
        .and_then(|value| value.parse::<u8>().ok())
    {
        return key.code == KeyCode::F(number);
    }
    configured.chars().next().is_some_and(|character| {
        configured.chars().count() == 1
            && key.code == KeyCode::Char(character)
            && key.modifiers.is_empty()
    })
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    key_to_bytes_mode(key, false)
}

fn key_to_bytes_mode(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    let mut encoded = match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = character.to_ascii_lowercase() as u8;
            vec![lower & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => if application_cursor {
            b"\x1bOA"
        } else {
            b"\x1b[A"
        }
        .to_vec(),
        KeyCode::Down => if application_cursor {
            b"\x1bOB"
        } else {
            b"\x1b[B"
        }
        .to_vec(),
        KeyCode::Right => if application_cursor {
            b"\x1bOC"
        } else {
            b"\x1b[C"
        }
        .to_vec(),
        KeyCode::Left => if application_cursor {
            b"\x1bOD"
        } else {
            b"\x1b[D"
        }
        .to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(number @ 1..=4) => vec![0x1b, b'O', b'P' + number - 1],
        KeyCode::F(number @ 5..=12) => format!(
            "\x1b[{}~",
            [15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)]
        )
        .into_bytes(),
        _ => Vec::new(),
    };
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        encoded = match key.code {
            KeyCode::Up => b"\x1b[1;2A".to_vec(),
            KeyCode::Down => b"\x1b[1;2B".to_vec(),
            KeyCode::Right => b"\x1b[1;2C".to_vec(),
            KeyCode::Left => b"\x1b[1;2D".to_vec(),
            KeyCode::Home => b"\x1b[1;2H".to_vec(),
            KeyCode::End => b"\x1b[1;2F".to_vec(),
            _ => encoded,
        };
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && !matches!(key.code, KeyCode::Char(_)) {
        encoded = match key.code {
            KeyCode::Up => b"\x1b[1;5A".to_vec(),
            KeyCode::Down => b"\x1b[1;5B".to_vec(),
            KeyCode::Right => b"\x1b[1;5C".to_vec(),
            KeyCode::Left => b"\x1b[1;5D".to_vec(),
            KeyCode::Home => b"\x1b[1;5H".to_vec(),
            KeyCode::End => b"\x1b[1;5F".to_vec(),
            _ => encoded,
        };
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        encoded.insert(0, 0x1b);
    }
    encoded
}

fn executable_on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

fn persist_prompt_history(history: &[String]) -> std::io::Result<()> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|base| base.home_dir().join(".local/state")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let directory = state_home.join("muxloom");
    std::fs::create_dir_all(&directory)?;
    let target = directory.join("prompt-history.json");
    let temporary = directory.join("prompt-history.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(history)?)?;
    std::fs::rename(temporary, target)
}

fn load_prompt_history() -> std::io::Result<Vec<String>> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|base| base.home_dir().join(".local/state")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let target = state_home.join("muxloom/prompt-history.json");
    if !target.exists() {
        return Ok(Vec::new());
    }
    let history = serde_json::from_slice::<Vec<String>>(&std::fs::read(target)?)?;
    Ok(history
        .into_iter()
        .rev()
        .take(100)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PaneSummary;
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn control_key_is_encoded() {
        assert_eq!(
            key_to_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![3]
        );
    }

    #[test]
    fn navigation_toggle_accepts_unix_control_backslash_encoding() {
        assert!(is_navigation_toggle(
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL),
            "ctrl-\\"
        ));
        assert!(is_navigation_toggle(
            KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
            "ctrl-\\"
        ));
        assert!(is_navigation_toggle(
            KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
            "ctrl-\\"
        ));
    }

    #[test]
    fn closing_a_pane_requires_confirmation() {
        let workspace_id = "workspace-id".to_string();
        let pane = PaneSummary {
            id: "pane-id".into(),
            workspace_id: workspace_id.clone(),
            title: "shell".into(),
            cwd: "/tmp".into(),
            command: vec!["sh".into()],
            provider: "generic".into(),
            pid: Some(4242),
            state: AgentState::Working,
            progress: None,
            started_at: Utc::now(),
            exited_at: None,
            exit_status: None,
            daemon_lost: false,
            unread: false,
        };
        let mut app = App::new(
            workspace_id.clone(),
            vec![WorkspaceSummary {
                id: workspace_id,
                name: "Test".into(),
                created_at: Utc::now(),
                panes: vec![pane],
                layout: None,
                respawn: false,
                schedule_id: None,
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HermesSnapshot::default(),
            Config::default(),
        );

        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            Action::Send(Request::ClosePane { pane_id }) if pane_id == "pane-id"
        ));
    }

    #[test]
    fn workspaces_can_be_created_and_switched_from_navigation() {
        let first = WorkspaceSummary {
            id: "first-id".into(),
            name: "First".into(),
            created_at: Utc::now(),
            panes: Vec::new(),
            layout: None,
            respawn: false,
            schedule_id: None,
        };
        let second = WorkspaceSummary {
            id: "second-id".into(),
            name: "Second".into(),
            created_at: Utc::now(),
            panes: Vec::new(),
            layout: None,
            respawn: false,
            schedule_id: None,
        };
        let mut app = App::new(
            first.id.clone(),
            vec![first, second],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HermesSnapshot::default(),
            Config::default(),
        );

        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Action::Send(Request::SwitchWorkspace { workspace_id }) if workspace_id == "second-id"
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Action::Send(Request::SwitchWorkspace { workspace_id }) if workspace_id == "first-id"
        ));

        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Action::None
        ));
        for character in "Review".chars() {
            assert!(matches!(
                app.process_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                Action::None
            ));
        }
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
            Action::Send(Request::CreateWorkspace { name, command, .. })
                if name == "Review" && !command.is_empty()
        ));
    }

    #[test]
    fn compact_and_wide_layouts_render() {
        for (width, height) in [(80, 24), (120, 40), (200, 60)] {
            let workspace_id = "workspace-id".to_string();
            let pane = PaneSummary {
                id: "pane-id".into(),
                workspace_id: workspace_id.clone(),
                title: "codex-api".into(),
                cwd: "/tmp/project".into(),
                command: vec!["codex".into()],
                provider: "codex".into(),
                pid: Some(4242),
                state: AgentState::WaitingInput,
                progress: None,
                started_at: Utc::now(),
                exited_at: None,
                exit_status: None,
                daemon_lost: false,
                unread: true,
            };
            let workspace = WorkspaceSummary {
                id: workspace_id.clone(),
                name: "API Platform".into(),
                created_at: Utc::now(),
                panes: vec![pane],
                layout: None,
                respawn: false,
                schedule_id: None,
            };
            let mut app = App::new(
                workspace_id,
                vec![workspace],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                HermesSnapshot::default(),
                Config::default(),
            );
            app.apply(Response::Output {
                workspace_id: "workspace-id".into(),
                pane_id: "pane-id".into(),
                data: b"\x1b[1;31mR\x1b[0m".to_vec(),
                sequence: 0,
            });
            assert_eq!(app.selected_running_pane_id().as_deref(), Some("pane-id"));
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
            let resizes = app.take_resizes();
            assert_eq!(resizes.len(), 1);
            let parser_size = app.parsers["pane-id"].screen().size();
            assert_eq!(parser_size, (resizes[0].1, resizes[0].2));
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("MUXLOOM"));
            assert!(rendered.contains("codex-api"));
            assert!(rendered.contains("NAV"));
            let colored = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .find(|cell| cell.symbol() == "R")
                .unwrap();
            assert_eq!(colored.fg, Color::Indexed(1));
            assert!(colored.modifier.contains(Modifier::BOLD));

            app.workspaces[0].panes[0].state = AgentState::Exited;
            app.workspaces[0].panes[0].exited_at = Some(Utc::now());
            assert_eq!(app.selected_running_pane_id(), None);
        }
    }
}
