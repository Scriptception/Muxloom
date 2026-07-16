use std::collections::HashMap;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    integrations::{ConfigSource, HermesSnapshot, git_summary},
    model::{
        AgentState, AttentionEvent, ScheduleRecord, SkillRecord, UsageSnapshot, WorkspaceSummary,
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

#[derive(Debug, Clone, Copy)]
pub enum SplitAxis {
    Columns,
    Rows,
}

pub enum Action {
    None,
    Send(Request),
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
    pub split_axis: SplitAxis,
    pub prompt_mode: bool,
    pub prompt: String,
    pub toast: String,
    creating_workspace: bool,
    confirm_close: bool,
    parsers: HashMap<String, vt100::Parser>,
    pending_chord: Option<char>,
}

impl App {
    pub fn new(
        workspace_id: String,
        workspaces: Vec<WorkspaceSummary>,
        attention: Vec<AttentionEvent>,
        usage: Vec<UsageSnapshot>,
        skills: Vec<SkillRecord>,
        configs: Vec<ConfigSource>,
        hermes: HermesSnapshot,
    ) -> Self {
        let mut parsers = HashMap::new();
        for pane in workspaces.iter().flat_map(|workspace| &workspace.panes) {
            parsers.insert(pane.id.clone(), vt100::Parser::new(30, 120, 5_000));
        }
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
            split_axis: SplitAxis::Columns,
            prompt_mode: false,
            prompt: String::new(),
            toast: "Navigation mode · press Enter to focus the agent".into(),
            creating_workspace: false,
            confirm_close: false,
            parsers,
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
            Response::Output { pane_id, data } => {
                self.parsers
                    .entry(pane_id)
                    .or_insert_with(|| vt100::Parser::new(30, 120, 5_000))
                    .process(&data);
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
            Response::Ok | Response::Pong { .. } | Response::Capture { .. } => {}
        }
    }

    pub fn process_key(&mut self, key: KeyEvent) -> Action {
        if is_navigation_toggle(key) {
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

        if let Some(chord) = self.pending_chord.take()
            && key.code == KeyCode::Char('a')
        {
            return self.jump_attention(chord == ']');
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => return Action::Quit,
            (KeyCode::Char('1'), _) => self.set_view(View::Workspace),
            (KeyCode::Char('2' | 'A'), _) => self.set_view(View::Attention),
            (KeyCode::Char('3'), _) => self.set_view(View::Usage),
            (KeyCode::Char('4'), _) => self.set_view(View::Skills),
            (KeyCode::Char('5'), _) => self.set_view(View::Config),
            (KeyCode::Char('6'), _) => self.set_view(View::Schedules),
            (KeyCode::Char('7'), _) => self.set_view(View::Hermes),
            (KeyCode::Char('?'), _) => self.set_view(View::Help),
            (KeyCode::Char('j') | KeyCode::Down, _) => self.select_next(),
            (KeyCode::Char('k') | KeyCode::Up, _) => self.select_previous(),
            (KeyCode::Char('h') | KeyCode::BackTab, _) => {
                return self.switch_workspace(false);
            }
            (KeyCode::Char('l') | KeyCode::Tab, _) => {
                return self.switch_workspace(true);
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
                self.split_axis = SplitAxis::Columns;
                return self.create_shell_pane();
            }
            (KeyCode::Char('s'), _) if self.view == View::Workspace => {
                self.split_axis = SplitAxis::Rows;
                return self.create_shell_pane();
            }
            (KeyCode::Char('x'), _) if self.view == View::Workspace => {
                if self.selected_pane_id().is_some() {
                    self.confirm_close = true;
                    self.toast = "Close selected pane? y confirms · any key cancels".into();
                }
            }
            (KeyCode::Char(character @ ('[' | ']')), _) => self.pending_chord = Some(character),
            (KeyCode::Esc, _) => self.set_view(View::Workspace),
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
                self.prompt_mode = false;
                if self.creating_workspace {
                    self.creating_workspace = false;
                    let name = if self.prompt.trim().is_empty() {
                        "workspace".to_string()
                    } else {
                        self.prompt.trim().to_string()
                    };
                    self.prompt.clear();
                    let cwd = self
                        .selected_pane()
                        .map(|pane| pane.cwd.clone())
                        .unwrap_or_else(|| ".".into());
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                    return Action::Send(Request::CreateWorkspace {
                        name,
                        cwd,
                        command: vec![shell, "-l".into()],
                    });
                }
                let mut data = self.prompt.as_bytes().to_vec();
                data.push(b'\n');
                self.prompt.clear();
                self.selected_pane_id()
                    .map(|pane_id| Action::Send(Request::Input { pane_id, data }))
                    .unwrap_or(Action::None)
            }
            KeyCode::Backspace => {
                self.prompt.pop();
                Action::None
            }
            KeyCode::Char(character) => {
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

    fn create_shell_pane(&self) -> Action {
        let cwd = self
            .selected_pane()
            .map(|pane| pane.cwd.clone())
            .unwrap_or_else(|| ".".into());
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        Action::Send(Request::CreatePane {
            workspace_id: self.workspace_id.clone(),
            cwd,
            command: vec![shell, "-l".into()],
            title: Some("shell".into()),
        })
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
        if let Some(workspace) = self.current_workspace()
            && let Some(pane_index) = workspace
                .panes
                .iter()
                .position(|pane| pane.id == event.pane_id)
        {
            self.selected_pane = pane_index;
            self.view = View::Workspace;
        }
        Action::Send(Request::MarkAttentionRead {
            id: event.id.clone(),
        })
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
        let show_rail = area.width >= 76;
        let show_context = area.width >= 132 && self.view == View::Workspace;
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(match (show_rail, show_context) {
                (true, true) => vec![
                    Constraint::Length(28),
                    Constraint::Min(40),
                    Constraint::Length(34),
                ],
                (true, false) => vec![Constraint::Length(26), Constraint::Min(40)],
                (false, _) => vec![Constraint::Min(30)],
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

    fn render_rail(&self, frame: &mut Frame, area: Rect) {
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
        let panes = self
            .current_workspace()
            .map_or_else(Vec::new, |workspace| workspace.panes.clone());
        if panes.is_empty() {
            frame.render_widget(
                empty_state("No panes", "Press v or s to open a shell"),
                area,
            );
            return;
        }
        let constraints = vec![Constraint::Ratio(1, panes.len() as u32); panes.len()];
        let direction = match self.split_axis {
            SplitAxis::Columns => Direction::Horizontal,
            SplitAxis::Rows => Direction::Vertical,
        };
        let sections = Layout::default()
            .direction(direction)
            .constraints(constraints)
            .split(area);
        for (index, pane) in panes.iter().enumerate() {
            let selected = index == self.selected_pane;
            let parser = self
                .parsers
                .entry(pane.id.clone())
                .or_insert_with(|| vt100::Parser::new(30, 120, 5_000));
            let content = parser.screen().contents();
            let title = format!(" {}  {} ", state_symbol(pane.state), pane.title);
            let border = if selected { ACCENT } else { BORDER };
            frame.render_widget(
                Paragraph::new(content)
                    .style(Style::default().fg(TEXT).bg(INK))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(title)
                            .title_style(
                                Style::default()
                                    .fg(state_color(pane.state))
                                    .add_modifier(Modifier::BOLD),
                            )
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border)),
                    ),
                sections[index],
            );
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
            frame.render_widget(
                List::new(events).block(section_block("ATTENTION QUEUE")),
                area,
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

fn is_navigation_toggle(key: KeyEvent) -> bool {
    key.code == KeyCode::F(12)
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('\\' | '4')))
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
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
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => Vec::new(),
    }
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
        assert!(is_navigation_toggle(KeyEvent::new(
            KeyCode::Char('4'),
            KeyModifiers::CONTROL
        )));
        assert!(is_navigation_toggle(KeyEvent::new(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL
        )));
        assert!(is_navigation_toggle(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::NONE
        )));
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
            unread: false,
        };
        let mut app = App::new(
            workspace_id.clone(),
            vec![WorkspaceSummary {
                id: workspace_id,
                name: "Test".into(),
                created_at: Utc::now(),
                panes: vec![pane],
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HermesSnapshot::default(),
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
        };
        let second = WorkspaceSummary {
            id: "second-id".into(),
            name: "Second".into(),
            created_at: Utc::now(),
            panes: Vec::new(),
        };
        let mut app = App::new(
            first.id.clone(),
            vec![first, second],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HermesSnapshot::default(),
        );

        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Action::Send(Request::SwitchWorkspace { workspace_id }) if workspace_id == "second-id"
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
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
                unread: true,
            };
            let workspace = WorkspaceSummary {
                id: workspace_id.clone(),
                name: "API Platform".into(),
                created_at: Utc::now(),
                panes: vec![pane],
            };
            let mut app = App::new(
                workspace_id,
                vec![workspace],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                HermesSnapshot::default(),
            );
            assert_eq!(app.selected_running_pane_id().as_deref(), Some("pane-id"));
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.render(frame)).unwrap();
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

            app.workspaces[0].panes[0].state = AgentState::Exited;
            app.workspaces[0].panes[0].exited_at = Some(Utc::now());
            assert_eq!(app.selected_running_pane_id(), None);
        }
    }
}
