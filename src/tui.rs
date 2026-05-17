use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeSet, VecDeque};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::compose::{
    self, Action, ActionBatchResult, LogsOptions, OutputSink, ProjectFailure, RemoveOptions,
};
use crate::config::AppConfig;
use crate::docker_api::{
    self, ContainerMetrics, HealthSummary, ProjectMetrics, ProjectState, ProjectStatus,
};
use crate::project::{discover_projects, Project};

const OUTPUT_LIMIT: usize = 2_000;

pub async fn run_tui(config: AppConfig) -> Result<()> {
    let terminal = ratatui::init();
    let result = run_tui_inner(terminal, config).await;
    ratatui::restore();
    result
}

async fn run_tui_inner(mut terminal: DefaultTerminal, config: AppConfig) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(config.clone());
    app.refresh(tx.clone());

    let mut reader = EventStream::new();
    let mut interval = time::interval(Duration::from_millis(config.refresh_interval_ms));

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        tokio::select! {
            maybe_event = reader.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    if !app.handle_key(key, tx.clone()) {
                        break;
                    }
                }
            }
            Some(event) = rx.recv() => {
                let should_refresh = matches!(event, TuiEvent::BatchFinished(_));
                app.handle_event(event);
                if should_refresh && !app.refresh_inflight {
                    app.refresh(tx.clone());
                }
            }
            _ = interval.tick() => {
                if !app.refresh_inflight {
                    app.refresh(tx.clone());
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Projects,
    Details,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filter,
}

#[derive(Debug, Clone)]
struct PendingAction {
    label: &'static str,
    action: Action,
}

#[derive(Debug)]
enum TuiEvent {
    StatusesLoaded(Vec<ProjectStatus>),
    StatusRefreshFailed(String),
    CommandStarted {
        project: String,
        command: String,
    },
    CommandOutput {
        line: String,
        stderr: bool,
    },
    CommandFinished {
        project: String,
        success: bool,
        exit_code: Option<i32>,
    },
    BatchFinished(ActionBatchResult),
}

#[derive(Debug)]
struct App {
    config: AppConfig,
    statuses: Vec<ProjectStatus>,
    selected_index: usize,
    marked: BTreeSet<String>,
    focus: Focus,
    input_mode: InputMode,
    filter: String,
    output: VecDeque<(String, bool)>,
    output_scroll: u16,
    details_scroll: u16,
    refresh_inflight: bool,
    command_running: bool,
    status_message: String,
    last_command: Option<String>,
    show_help: bool,
    pending_action: Option<PendingAction>,
}

impl App {
    fn new(config: AppConfig) -> Self {
        Self {
            config,
            statuses: Vec::new(),
            selected_index: 0,
            marked: BTreeSet::new(),
            focus: Focus::Projects,
            input_mode: InputMode::Normal,
            filter: String::new(),
            output: VecDeque::new(),
            output_scroll: 0,
            details_scroll: 0,
            refresh_inflight: false,
            command_running: false,
            status_message: "Loading project status...".to_string(),
            last_command: None,
            show_help: false,
            pending_action: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, tx: mpsc::UnboundedSender<TuiEvent>) -> bool {
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => {
                    self.show_help = false;
                }
                _ => {}
            }
            return true;
        }

        if let Some(pending) = self.pending_action.clone() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.pending_action = None;
                    self.spawn_action(pending.action, tx);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.pending_action = None;
                    self.status_message = format!("{} cancelled", pending.label);
                }
                _ => {}
            }
            return true;
        }

        if self.input_mode == InputMode::Filter {
            return self.handle_filter_key(key);
        }

        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Tab => self.focus = next_focus(self.focus),
            KeyCode::Char('/') => self.input_mode = InputMode::Filter,
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(),
            KeyCode::PageDown => self.scroll_active(5),
            KeyCode::PageUp => self.scroll_active(-5),
            KeyCode::Enter => self.focus = Focus::Details,
            KeyCode::Char('a') => self.toggle_select_all_visible(),
            KeyCode::Char('A') => self.invert_visible_selection(),
            KeyCode::Char(' ') if self.focus == Focus::Projects => self.toggle_mark(),
            KeyCode::Char('u') if !self.command_running => {
                self.confirm_action("deploy", Action::Deploy)
            }
            KeyCode::Char('U') if !self.command_running => {
                self.confirm_action("update", Action::Update)
            }
            KeyCode::Char('s') if !self.command_running => {
                self.confirm_action("stop", Action::Stop)
            }
            KeyCode::Char('r') if !self.command_running => {
                self.confirm_action("restart", Action::Restart)
            }
            KeyCode::Char('d') if !self.command_running => self.confirm_action(
                "remove",
                Action::Remove(RemoveOptions {
                    volumes: false,
                    remove_orphans: false,
                    rmi: None,
                }),
            ),
            KeyCode::Char('l') if !self.command_running => self.spawn_action(
                Action::Logs(LogsOptions {
                    follow: false,
                    tail: self.config.default_log_lines,
                }),
                tx,
            ),
            _ => {}
        }
        true
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.input_mode = InputMode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selection();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(ch);
                self.clamp_selection();
            }
            _ => {}
        }
        true
    }

    fn handle_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::StatusesLoaded(statuses) => {
                self.statuses = statuses;
                self.refresh_inflight = false;
                self.status_message = "Ready".to_string();
                self.clamp_selection();
            }
            TuiEvent::StatusRefreshFailed(message) => {
                self.refresh_inflight = false;
                self.status_message = message;
            }
            TuiEvent::CommandStarted { project, command } => {
                self.command_running = true;
                self.last_command = Some(command.clone());
                self.push_output(format!("==> [{project}] {command}"), false);
            }
            TuiEvent::CommandOutput { line, stderr } => {
                self.push_output(line, stderr);
            }
            TuiEvent::CommandFinished {
                project,
                success,
                exit_code,
            } => {
                let exit_text = exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                self.push_output(
                    format!(
                        "-- [{}] {} (exit {})",
                        project,
                        if success { "ok" } else { "failed" },
                        exit_text
                    ),
                    !success,
                );
            }
            TuiEvent::BatchFinished(result) => {
                self.command_running = false;
                if result.failed_projects.is_empty() {
                    self.status_message = "Command batch completed".to_string();
                } else {
                    self.status_message = format_failures(&result.failed_projects);
                }
            }
        }
    }

    fn refresh(&mut self, tx: mpsc::UnboundedSender<TuiEvent>) {
        self.refresh_inflight = true;
        let compose_dir = self.config.compose_dir.clone();
        tokio::spawn(async move {
            match discover_projects(&compose_dir) {
                Ok(projects) => {
                    let statuses = docker_api::load_statuses_for_projects(&projects).await;
                    let _ = tx.send(TuiEvent::StatusesLoaded(statuses));
                }
                Err(err) => {
                    let _ = tx.send(TuiEvent::StatusRefreshFailed(err.to_string()));
                }
            }
        });
    }

    fn spawn_action(&mut self, action: Action, tx: mpsc::UnboundedSender<TuiEvent>) {
        let projects = self.selected_projects();
        if projects.is_empty() {
            self.status_message = "No project selected".to_string();
            return;
        }

        self.command_running = true;
        let config = self.config.clone();
        tokio::spawn(async move {
            let mut sink = TuiSink { tx: tx.clone() };
            let result = compose::run_action_batch(projects, config, action, &mut sink)
                .await
                .unwrap_or_else(|err| ActionBatchResult {
                    failed_projects: vec![ProjectFailure {
                        project: "batch".to_string(),
                        message: err.to_string(),
                    }],
                });
            let _ = tx.send(TuiEvent::BatchFinished(result));
        });
    }

    fn confirm_action(&mut self, label: &'static str, action: Action) {
        let count = self.selected_projects().len();
        if count == 0 {
            self.status_message = "No project selected".to_string();
            return;
        }

        self.pending_action = Some(PendingAction { label, action });
        self.status_message = format!("Confirm {label} for {count} project(s)");
    }

    fn selected_projects(&self) -> Vec<Project> {
        if !self.marked.is_empty() {
            return self
                .statuses
                .iter()
                .filter(|status| self.marked.contains(&status.project.name))
                .map(|status| status.project.clone())
                .collect();
        }

        self.selected_status()
            .map(|status| vec![status.project.clone()])
            .unwrap_or_default()
    }

    fn selected_status(&self) -> Option<&ProjectStatus> {
        let visible = self.visible_statuses();
        visible.get(self.selected_index).and_then(|name| {
            self.statuses
                .iter()
                .find(|status| &status.project.name == *name)
        })
    }

    fn visible_statuses(&self) -> Vec<&String> {
        self.statuses
            .iter()
            .map(|status| &status.project.name)
            .filter(|name| self.filter.is_empty() || name.contains(&self.filter))
            .collect()
    }

    fn visible_project_names(&self) -> Vec<String> {
        self.visible_statuses()
            .into_iter()
            .map(|name| name.clone())
            .collect()
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_statuses().len();
        if len == 0 {
            self.selected_index = 0;
        } else if self.selected_index >= len {
            self.selected_index = len - 1;
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Projects => {
                let len = self.visible_statuses().len();
                if len > 0 {
                    self.selected_index = (self.selected_index + 1).min(len - 1);
                }
            }
            Focus::Details => self.details_scroll = self.details_scroll.saturating_add(1),
            Focus::Output => self.output_scroll = self.output_scroll.saturating_add(1),
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            Focus::Projects => {
                self.selected_index = self.selected_index.saturating_sub(1);
            }
            Focus::Details => self.details_scroll = self.details_scroll.saturating_sub(1),
            Focus::Output => self.output_scroll = self.output_scroll.saturating_sub(1),
        }
    }

    fn scroll_active(&mut self, delta: i16) {
        match self.focus {
            Focus::Projects => {}
            Focus::Details => self.details_scroll = adjust_scroll(self.details_scroll, delta),
            Focus::Output => self.output_scroll = adjust_scroll(self.output_scroll, delta),
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(name) = self
            .visible_statuses()
            .get(self.selected_index)
            .map(|name| (*name).clone())
        {
            if !self.marked.insert(name.clone()) {
                self.marked.remove(&name);
            }
        }
    }

    fn toggle_select_all_visible(&mut self) {
        let visible = self.visible_project_names();
        if visible.is_empty() {
            self.status_message = "No visible projects".to_string();
            return;
        }

        if visible.iter().all(|name| self.marked.contains(name)) {
            for name in &visible {
                self.marked.remove(name);
            }
            self.status_message = format!("Cleared {} visible project(s)", visible.len());
        } else {
            for name in &visible {
                self.marked.insert(name.clone());
            }
            self.status_message = format!("Selected {} visible project(s)", visible.len());
        }
    }

    fn invert_visible_selection(&mut self) {
        let visible = self.visible_project_names();
        if visible.is_empty() {
            self.status_message = "No visible projects".to_string();
            return;
        }

        for name in &visible {
            if !self.marked.insert(name.clone()) {
                self.marked.remove(name);
            }
        }
        self.status_message = format!("Inverted {} visible project(s)", visible.len());
    }

    fn push_output(&mut self, line: String, stderr: bool) {
        if self.output.len() == OUTPUT_LIMIT {
            self.output.pop_front();
        }
        self.output.push_back((line, stderr));
        self.output_scroll = self.output.len().saturating_sub(1) as u16;
    }
}

struct TuiSink {
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl OutputSink for TuiSink {
    fn command_started(&mut self, project: &Project, command: &str) {
        let _ = self.tx.send(TuiEvent::CommandStarted {
            project: project.name.clone(),
            command: command.to_string(),
        });
    }

    fn command_output(&mut self, line: &str, stderr: bool) {
        let _ = self.tx.send(TuiEvent::CommandOutput {
            line: line.to_string(),
            stderr,
        });
    }

    fn command_finished(&mut self, project: &Project, success: bool, exit_code: Option<i32>) {
        let _ = self.tx.send(TuiEvent::CommandFinished {
            project: project.name.clone(),
            success,
            exit_code,
        });
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(frame.area());

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(root[1]);

    draw_projects(frame, app, root[0]);
    draw_details(frame, app, right[0]);
    draw_output(frame, app, right[1]);

    if app.show_help {
        draw_help(frame);
    }

    if app.pending_action.is_some() {
        draw_confirmation(frame, app);
    }
}

fn draw_projects(frame: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible_statuses();
    let inner_width = area.width.saturating_sub(2) as usize;
    let items = visible
        .iter()
        .map(|name| {
            let status = app
                .statuses
                .iter()
                .find(|status| &status.project.name == *name)
                .expect("status exists");
            let line = project_list_line(status, app.marked.contains(*name), inner_width);
            ListItem::new(line).style(status_style(status.summary.state))
        })
        .collect::<Vec<_>>();

    let block = Block::default()
        .title(format!(
            "Projects [{}]  {}",
            app.statuses.len(),
            focus_label(Focus::Projects, app.focus)
        ))
        .borders(Borders::ALL)
        .border_style(focus_style(Focus::Projects, app.focus));

    let mut state =
        ListState::default().with_selected((!items.is_empty()).then_some(app.selected_index));
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![
        Line::from(format!("Status: {}", app.status_message)),
        Line::from(format!(
            "Filter: {}{}",
            app.filter,
            if app.input_mode == InputMode::Filter {
                " (editing)"
            } else {
                ""
            }
        )),
    ];

    if let Some(status) = app.selected_status() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Project: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(status.project.name.clone()),
        ]));
        lines.push(Line::from(format!(
            "Path: {}",
            status.project.dir.display()
        )));
        lines.push(Line::from(format!(
            "Compose: {}",
            status.project.compose_file.display()
        )));
        lines.push(Line::from(format!(
            "Summary: {} / {} / {}",
            status.summary.state_label(),
            status.summary.health_label(),
            status.summary.running_summary()
        )));
        lines.push(Line::from(format!(
            "Metrics: {}",
            project_metrics_line(status.summary.metrics.as_ref())
        )));
        if let Some(error) = &status.error {
            lines.push(Line::from(format!("API: {error}")));
        }
        lines.push(Line::from(""));
        lines.push(Line::from("Containers:"));
        for container in &status.containers {
            lines.push(Line::from(format!(
                "- {} [{}] {} {}",
                container.name,
                container.state,
                container.health.as_deref().unwrap_or("no-healthcheck"),
                container.status.as_deref().unwrap_or("")
            )));
            if !container.ports.is_empty() {
                lines.push(Line::from(format!(
                    "  ports: {}",
                    container.ports.join(", ")
                )));
            }
            if let Some(started_at) = &container.started_at {
                lines.push(Line::from(format!("  started: {started_at}")));
            }
            lines.push(Line::from(format!(
                "  metrics: {}",
                container_metrics_line(container.metrics.as_ref())
            )));
        }
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from("No project selected"));
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!(
                    "Details {}",
                    focus_label(Focus::Details, app.focus)
                ))
                .borders(Borders::ALL)
                .border_style(focus_style(Focus::Details, app.focus)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.details_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_output(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(command) = &app.last_command {
        lines.push(Line::from(vec![
            Span::styled(
                "Last command: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(command.clone()),
        ]));
        lines.push(Line::from(""));
    }

    for (line, stderr) in &app.output {
        let style = if *stderr {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(line.clone(), style)));
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!("Output {}", focus_label(Focus::Output, app.focus)))
                .borders(Borders::ALL)
                .border_style(focus_style(Focus::Output, app.focus)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.output_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_help(frame: &mut Frame) {
    let area = centered_rect(frame.area(), 72, 70);
    let lines = vec![
        Line::from(vec![
            Span::styled("TUI Help", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  press ? / Esc / q to close"),
        ]),
        Line::from(""),
        Line::from("Navigation"),
        Line::from("  j / k or Up / Down   Move selection"),
        Line::from("  Tab                  Switch focus"),
        Line::from("  Enter                Focus details"),
        Line::from("  PageUp / PageDown    Scroll details or output"),
        Line::from(""),
        Line::from("Selection"),
        Line::from("  Space                Mark or unmark project"),
        Line::from("  a                    Select all visible projects, or clear if all visible are selected"),
        Line::from("  A                    Invert visible project selection"),
        Line::from("  /                    Filter projects"),
        Line::from(""),
        Line::from("Actions"),
        Line::from("  u                    Deploy, with confirmation"),
        Line::from("  U                    Update, with confirmation"),
        Line::from("  s                    Stop, with confirmation"),
        Line::from("  r                    Restart, with confirmation"),
        Line::from("  d                    Remove, with confirmation"),
        Line::from("  l                    Show logs"),
        Line::from(""),
        Line::from("General"),
        Line::from("  q                    Quit"),
        Line::from("  ?                    Toggle this help"),
    ];

    let help = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Help")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black))
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, area);
    frame.render_widget(help, area);
}

fn draw_confirmation(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_action else {
        return;
    };

    let count = app.selected_projects().len();
    let area = centered_rect(frame.area(), 56, 24);
    let lines = vec![
        Line::from(vec![Span::styled(
            "Confirm Action",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(format!("Run {} for {} project(s)?", pending.label, count)),
        Line::from(""),
        Line::from("Enter / y   confirm"),
        Line::from("Esc / n / q cancel"),
    ];

    let modal = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Confirmation")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black))
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, area);
    frame.render_widget(modal, area);
}

fn status_style(state: ProjectState) -> Style {
    match state {
        ProjectState::Running => Style::default().fg(Color::Green),
        ProjectState::Partial => Style::default().fg(Color::Yellow),
        ProjectState::Stopped => Style::default().fg(Color::Gray),
        ProjectState::Missing => Style::default().fg(Color::DarkGray),
        ProjectState::Error => Style::default().fg(Color::Red),
        ProjectState::Unknown => Style::default().fg(Color::Blue),
    }
}

fn project_list_line(status: &ProjectStatus, marked: bool, width: usize) -> String {
    let mark = if marked { "*" } else { " " };
    let prefix = format!("[{mark}] ");
    let summary = project_list_summary(status, width, prefix.len());
    let reserved = prefix.len()
        + if summary.is_empty() {
            0
        } else {
            summary.len() + 1
        };
    let name_width = width.saturating_sub(reserved);
    let name = truncate_to_width(&status.project.name, name_width);
    let line = if summary.is_empty() {
        format!("{prefix}{name}")
    } else {
        format!("{prefix}{name} {summary}")
    };
    truncate_to_width(&line, width)
}

fn project_list_summary(status: &ProjectStatus, width: usize, prefix_len: usize) -> String {
    const MIN_NAME_WIDTH: usize = 4;

    let metrics = status.summary.metrics.as_ref();
    let running = format!(
        "{}/{}",
        status.summary.running_containers, status.summary.total_containers
    );
    let cpu = compact_percent(metrics.and_then(|metrics| metrics.cpu_percent));
    let memory = compact_percent(metrics.and_then(|metrics| metrics.memory_percent));
    let candidates = [
        format!(
            "{} {} {} cpu {} mem {}",
            status.summary.state_label(),
            status.summary.health_label(),
            running,
            cpu,
            memory
        ),
        format!(
            "{}{} {} {} {}",
            project_state_code(status.summary.state),
            health_code(status.summary.health),
            running,
            cpu,
            memory
        ),
        format!("{running} {cpu} {memory}"),
        format!("{running} {cpu}"),
        running,
    ];

    candidates
        .into_iter()
        .find(|candidate| width >= prefix_len + candidate.len() + 1 + MIN_NAME_WIDTH)
        .unwrap_or_else(|| "".to_string())
}

fn project_state_code(state: ProjectState) -> &'static str {
    match state {
        ProjectState::Running => "R",
        ProjectState::Partial => "P",
        ProjectState::Stopped => "S",
        ProjectState::Missing => "M",
        ProjectState::Error => "E",
        ProjectState::Unknown => "?",
    }
}

fn health_code(health: HealthSummary) -> &'static str {
    match health {
        HealthSummary::Healthy => "H",
        HealthSummary::Unhealthy => "U",
        HealthSummary::Starting => "S",
        HealthSummary::NoHealthcheck => "-",
        HealthSummary::Unknown => "?",
    }
}

fn compact_percent(value: Option<f64>) -> String {
    match value {
        Some(value) if value.abs() >= 10.0 => format!("{value:.0}%"),
        Some(value) => format!("{value:.1}%"),
        None => "-".to_string(),
    }
}

fn truncate_to_width(value: &str, width: usize) -> String {
    let len = value.chars().count();
    if len <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "~".to_string();
    }

    let mut truncated = value.chars().take(width - 1).collect::<String>();
    truncated.push('~');
    truncated
}

fn project_metrics_line(metrics: Option<&ProjectMetrics>) -> String {
    metrics
        .map(|metrics| {
            metrics_line(
                metrics.cpu_percent,
                metrics.memory_usage_bytes,
                metrics.memory_limit_bytes,
                metrics.memory_percent,
                metrics.network_rx_bytes,
                metrics.network_tx_bytes,
                metrics.block_read_bytes,
                metrics.block_write_bytes,
                metrics.pids,
            )
        })
        .unwrap_or_else(|| "-".to_string())
}

fn container_metrics_line(metrics: Option<&ContainerMetrics>) -> String {
    metrics
        .map(|metrics| {
            metrics_line(
                metrics.cpu_percent,
                metrics.memory_usage_bytes,
                metrics.memory_limit_bytes,
                metrics.memory_percent,
                metrics.network_rx_bytes,
                metrics.network_tx_bytes,
                metrics.block_read_bytes,
                metrics.block_write_bytes,
                metrics.pids,
            )
        })
        .unwrap_or_else(|| "-".to_string())
}

fn metrics_line(
    cpu_percent: Option<f64>,
    memory_usage_bytes: Option<u64>,
    memory_limit_bytes: Option<u64>,
    memory_percent: Option<f64>,
    network_rx_bytes: u64,
    network_tx_bytes: u64,
    block_read_bytes: u64,
    block_write_bytes: u64,
    pids: Option<u64>,
) -> String {
    format!(
        "cpu {}  mem {}  net rx/tx {}  block r/w {}  pids {}",
        docker_api::format_cpu(cpu_percent),
        docker_api::format_memory(memory_usage_bytes, memory_limit_bytes, memory_percent),
        docker_api::format_io(network_rx_bytes, network_tx_bytes),
        docker_api::format_io(block_read_bytes, block_write_bytes),
        docker_api::format_pids(pids),
    )
}

fn focus_style(panel: Focus, current: Focus) -> Style {
    if panel == current {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn focus_label(panel: Focus, current: Focus) -> &'static str {
    if panel == current {
        "[active]"
    } else {
        ""
    }
}

fn next_focus(current: Focus) -> Focus {
    match current {
        Focus::Projects => Focus::Details,
        Focus::Details => Focus::Output,
        Focus::Output => Focus::Projects,
    }
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn adjust_scroll(current: u16, delta: i16) -> u16 {
    if delta >= 0 {
        current.saturating_add(delta as u16)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    }
}

fn format_failures(failures: &[ProjectFailure]) -> String {
    failures
        .iter()
        .map(|failure| format!("{}: {}", failure.project, failure.message))
        .collect::<Vec<_>>()
        .join(" | ")
}
