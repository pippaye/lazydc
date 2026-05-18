use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::compose::{
    self, Action, ActionBatchResult, LogsOptions, OutputSink, ProjectFailure, RemoveOptions,
};
use crate::config::AppConfig;
use crate::docker_api::{
    self, ContainerMetrics, ContainerStatus, HealthSummary, ProjectMetrics, ProjectState,
    ProjectStatus,
};
use crate::project::{
    create_project, delete_project, discover_projects, validate_project_name, Project,
};

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
                    match app.handle_key(key, tx.clone()) {
                        KeyOutcome::Continue => {}
                        KeyOutcome::Quit => break,
                        KeyOutcome::OpenEditor(launch) => {
                            terminal.draw(|frame| draw(frame, &app))?;
                            let result = suspend_for_editor(&mut terminal, &launch);
                            reader = EventStream::new();
                            app.refresh_inflight = false;
                            match result {
                                Ok(()) => {
                                    if let Some(project_name) = &launch.created_project {
                                        app.status_message = format!("Created project {project_name}");
                                        app.push_output(
                                            format!(
                                                "Created project {project_name} at {}",
                                                launch.path.display()
                                            ),
                                            false,
                                        );
                                    } else {
                                        app.status_message =
                                            format!("Edited {}", launch.target_label);
                                        app.push_output(
                                            format!(
                                                "Edited {} at {}",
                                                launch.target_label,
                                                launch.path.display()
                                            ),
                                            false,
                                        );
                                    }
                                }
                                Err(err) => {
                                    if let Some(project_name) = &launch.created_project {
                                        app.status_message = format!(
                                            "Project {project_name} created, editor failed: {err}"
                                        );
                                    } else {
                                        app.status_message =
                                            format!("Editor failed for {}: {err}", launch.target_label);
                                    }
                                    app.push_output(
                                        format!("Editor failed for {}: {err}", launch.target_label),
                                        true,
                                    );
                                }
                            }
                            app.refresh(tx.clone());
                        }
                        KeyOutcome::OpenShell(launch) => {
                            terminal.draw(|frame| draw(frame, &app))?;
                            let result = suspend_for_shell(&mut terminal, &launch);
                            reader = EventStream::new();
                            app.refresh_inflight = false;
                            match result {
                                Ok(()) => {
                                    app.status_message =
                                        format!("Shell exited for {}", launch.container_name);
                                    app.push_output(
                                        format!(
                                            "Shell exited for {} in {}",
                                            launch.container_name, launch.project_name
                                        ),
                                        false,
                                    );
                                }
                                Err(err) => {
                                    app.status_message = format!(
                                        "Shell failed for {}: {err}",
                                        launch.container_name
                                    );
                                    app.push_output(
                                        format!(
                                            "Shell failed for {} in {}: {err}",
                                            launch.container_name, launch.project_name
                                        ),
                                        true,
                                    );
                                }
                            }
                            app.refresh(tx.clone());
                        }
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
    NewProject,
    ContainerShell,
}

#[derive(Debug, Clone)]
struct PendingAction {
    label: &'static str,
    action: TuiAction,
}

#[derive(Debug)]
enum KeyOutcome {
    Continue,
    Quit,
    OpenEditor(EditorLaunch),
    OpenShell(ShellLaunch),
}

#[derive(Debug)]
struct EditorLaunch {
    editor: String,
    target_label: String,
    path: PathBuf,
    created_project: Option<String>,
}

#[derive(Debug, Clone)]
struct ShellLaunch {
    docker_bin: PathBuf,
    project_name: String,
    container_id: String,
    container_name: String,
}

#[derive(Debug, Clone)]
enum TuiAction {
    Compose(Action),
    DeleteProjectFiles,
    PurgeProject,
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
    help_scroll: u16,
    pending_action: Option<PendingAction>,
    shell_container_index: usize,
    new_project_name: String,
    new_project_error: Option<String>,
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
            help_scroll: 0,
            pending_action: None,
            shell_container_index: 0,
            new_project_name: String::new(),
            new_project_error: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, tx: mpsc::UnboundedSender<TuiEvent>) -> KeyOutcome {
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => self.scroll_help(1),
                KeyCode::Char('k') | KeyCode::Up => self.scroll_help(-1),
                KeyCode::PageDown => self.scroll_help(8),
                KeyCode::PageUp => self.scroll_help(-8),
                _ => {}
            }
            return KeyOutcome::Continue;
        }

        if let Some(pending) = self.pending_action.clone() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.pending_action = None;
                    self.spawn_tui_action(pending.action, tx);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.pending_action = None;
                    self.status_message = format!("{} cancelled", pending.label);
                }
                _ => {}
            }
            return KeyOutcome::Continue;
        }

        if self.input_mode == InputMode::Filter {
            self.handle_filter_key(key);
            return KeyOutcome::Continue;
        }

        if self.input_mode == InputMode::NewProject {
            return self.handle_new_project_key(key);
        }

        if self.input_mode == InputMode::ContainerShell {
            return self.handle_container_shell_key(key);
        }

        match key.code {
            KeyCode::Char('q') => return KeyOutcome::Quit,
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Tab => self.focus = next_focus(self.focus),
            KeyCode::Char('/') => self.input_mode = InputMode::Filter,
            KeyCode::Char('n') if !self.command_running => self.start_new_project(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(),
            KeyCode::PageDown => self.scroll_active(5),
            KeyCode::PageUp => self.scroll_active(-5),
            KeyCode::Enter => self.focus = Focus::Details,
            KeyCode::Char('a') => self.toggle_select_all_visible(),
            KeyCode::Char('A') => self.invert_visible_selection(),
            KeyCode::Char(' ') if self.focus == Focus::Projects => self.toggle_mark(),
            KeyCode::Char('e')
                if !self.command_running && key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(launch) = self.edit_global_env() {
                    return KeyOutcome::OpenEditor(launch);
                }
            }
            KeyCode::Char('e') if !self.command_running => {
                if let Some(launch) = self.edit_selected_compose() {
                    return KeyOutcome::OpenEditor(launch);
                }
            }
            KeyCode::Char('E') if !self.command_running => {
                if let Some(launch) = self.edit_selected_env() {
                    return KeyOutcome::OpenEditor(launch);
                }
            }
            KeyCode::Char('c') if !self.command_running => self.start_container_shell(),
            KeyCode::Char('u') if !self.command_running => {
                self.confirm_action("deploy", TuiAction::Compose(Action::Deploy))
            }
            KeyCode::Char('U') if !self.command_running => {
                self.confirm_action("update", TuiAction::Compose(Action::Update))
            }
            KeyCode::Char('s') if !self.command_running => {
                self.confirm_action("stop", TuiAction::Compose(Action::Stop))
            }
            KeyCode::Char('r') if !self.command_running => {
                self.confirm_action("restart", TuiAction::Compose(Action::Restart))
            }
            KeyCode::Char('d') if !self.command_running => self.confirm_action(
                "remove",
                TuiAction::Compose(Action::Remove(RemoveOptions {
                    volumes: false,
                    remove_orphans: false,
                    rmi: None,
                })),
            ),
            KeyCode::Char('D') if !self.command_running => self.confirm_action(
                "remove with volumes",
                TuiAction::Compose(Action::Remove(RemoveOptions {
                    volumes: true,
                    remove_orphans: false,
                    rmi: None,
                })),
            ),
            KeyCode::Char('x') if !self.command_running => {
                self.confirm_action("delete project files", TuiAction::DeleteProjectFiles)
            }
            KeyCode::Char('X') if !self.command_running => {
                self.confirm_action("purge project", TuiAction::PurgeProject)
            }
            KeyCode::Char('l') if !self.command_running => self.spawn_action(
                Action::Logs(LogsOptions {
                    follow: false,
                    tail: self.config.default_log_lines,
                }),
                tx,
            ),
            _ => {}
        }
        KeyOutcome::Continue
    }

    fn handle_container_shell_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.input_mode = InputMode::Normal;
                self.shell_container_index = 0;
                self.status_message = "Container shell cancelled".to_string();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_shell_container(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_shell_container(-1),
            KeyCode::Enter => {
                if let Some(launch) = self.selected_shell_launch() {
                    self.input_mode = InputMode::Normal;
                    self.shell_container_index = 0;
                    self.status_message = format!("Opening shell for {}", launch.container_name);
                    return KeyOutcome::OpenShell(launch);
                }
            }
            _ => {}
        }

        KeyOutcome::Continue
    }

    fn start_container_shell(&mut self) {
        let project_name = {
            let Some(status) = self.selected_status() else {
                self.status_message = "No project selected".to_string();
                return;
            };

            if running_containers(status).is_empty() {
                self.status_message = "No running containers".to_string();
                return;
            }

            status.project.name.clone()
        };

        self.input_mode = InputMode::ContainerShell;
        self.shell_container_index = 0;
        self.status_message = format!("Select container for {project_name}");
    }

    fn move_shell_container(&mut self, delta: i16) {
        let len = self
            .selected_status()
            .map(|status| running_containers(status).len())
            .unwrap_or_default();
        if len == 0 {
            self.shell_container_index = 0;
            return;
        }

        let next = adjust_scroll(self.shell_container_index as u16, delta) as usize;
        self.shell_container_index = next.min(len - 1);
    }

    fn selected_shell_launch(&mut self) -> Option<ShellLaunch> {
        let (project_name, container_id, container_name) = {
            let Some(status) = self.selected_status() else {
                self.status_message = "No project selected".to_string();
                return None;
            };
            let containers = running_containers(status);
            let Some(container) = containers.get(self.shell_container_index) else {
                self.status_message = "No running containers".to_string();
                return None;
            };
            (
                status.project.name.clone(),
                container.id.clone(),
                container.name.clone(),
            )
        };

        Some(ShellLaunch {
            docker_bin: self.config.docker_bin.clone(),
            project_name,
            container_id,
            container_name,
        })
    }

    fn open_help(&mut self) {
        self.show_help = true;
        self.help_scroll = 0;
    }

    fn scroll_help(&mut self, delta: i16) {
        let max_scroll = help_lines().len().saturating_sub(1) as u16;
        self.help_scroll = adjust_scroll(self.help_scroll, delta).min(max_scroll);
    }

    fn edit_selected_compose(&mut self) -> Option<EditorLaunch> {
        let project = match self.selected_status().map(|status| status.project.clone()) {
            Some(project) => project,
            None => {
                self.status_message = "No project selected".to_string();
                return None;
            }
        };

        self.build_editor_launch(
            format!("compose for {}", project.name),
            project.compose_file,
            false,
        )
    }

    fn edit_selected_env(&mut self) -> Option<EditorLaunch> {
        let project = match self.selected_status().map(|status| status.project.clone()) {
            Some(project) => project,
            None => {
                self.status_message = "No project selected".to_string();
                return None;
            }
        };

        self.build_editor_launch(
            format!(".env for {}", project.name),
            project.dir.join(".env"),
            true,
        )
    }

    fn edit_global_env(&mut self) -> Option<EditorLaunch> {
        self.build_editor_launch(".env.global".to_string(), self.config.env_file(), true)
    }

    fn build_editor_launch(
        &mut self,
        target_label: String,
        path: PathBuf,
        create_missing: bool,
    ) -> Option<EditorLaunch> {
        let editor = match configured_editor() {
            Some(editor) => editor,
            None => {
                self.status_message = "set VISUAL or EDITOR to edit files".to_string();
                return None;
            }
        };

        if create_missing {
            if let Err(err) = ensure_edit_file(&path) {
                self.status_message = err.to_string();
                self.push_output(format!("Failed to prepare {target_label}: {err}"), true);
                return None;
            }
        }

        self.status_message = format!("Opening editor for {target_label}");
        Some(EditorLaunch {
            editor,
            target_label,
            path,
            created_project: None,
        })
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
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
    }

    fn handle_new_project_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.new_project_name.clear();
                self.new_project_error = None;
                self.status_message = "New project cancelled".to_string();
            }
            KeyCode::Enter => {
                if let Some(launch) = self.submit_new_project() {
                    return KeyOutcome::OpenEditor(launch);
                }
            }
            KeyCode::Backspace => {
                self.new_project_name.pop();
                self.new_project_error = None;
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_project_name.push(ch);
                self.new_project_error = None;
            }
            _ => {}
        }

        KeyOutcome::Continue
    }

    fn start_new_project(&mut self) {
        self.input_mode = InputMode::NewProject;
        self.new_project_name.clear();
        self.new_project_error = None;
        self.status_message = "Enter new project name".to_string();
    }

    fn submit_new_project(&mut self) -> Option<EditorLaunch> {
        self.submit_new_project_with_editor(configured_editor())
    }

    fn submit_new_project_with_editor(&mut self, editor: Option<String>) -> Option<EditorLaunch> {
        let name = self.new_project_name.clone();
        if let Err(err) = validate_project_name(&name) {
            self.new_project_error = Some(err.to_string());
            self.status_message = err.to_string();
            return None;
        }

        let editor = match editor {
            Some(editor) => editor,
            None => {
                let message = "set VISUAL or EDITOR to create projects".to_string();
                self.input_mode = InputMode::Normal;
                self.new_project_name.clear();
                self.new_project_error = None;
                self.status_message = message;
                return None;
            }
        };

        let project = match create_project(&self.config.compose_dir, &name) {
            Ok(project) => project,
            Err(err) => {
                self.new_project_error = Some(err.to_string());
                self.status_message = err.to_string();
                return None;
            }
        };

        self.input_mode = InputMode::Normal;
        self.new_project_name.clear();
        self.new_project_error = None;
        self.status_message = format!("Opening editor for {}", project.name);

        Some(EditorLaunch {
            editor,
            target_label: project.name.clone(),
            path: project.compose_file,
            created_project: Some(project.name),
        })
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

    fn spawn_tui_action(&mut self, action: TuiAction, tx: mpsc::UnboundedSender<TuiEvent>) {
        match action {
            TuiAction::Compose(action) => self.spawn_action(action, tx),
            TuiAction::DeleteProjectFiles => self.spawn_delete_projects(false, tx),
            TuiAction::PurgeProject => self.spawn_delete_projects(true, tx),
        }
    }

    fn spawn_delete_projects(&mut self, purge: bool, tx: mpsc::UnboundedSender<TuiEvent>) {
        let projects = self.selected_projects();
        if projects.is_empty() {
            self.status_message = "No project selected".to_string();
            return;
        }

        self.command_running = true;
        let config = self.config.clone();
        tokio::spawn(async move {
            let mut sink = TuiSink { tx: tx.clone() };
            let result = delete_project_batch(projects, config, purge, &mut sink)
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

    fn confirm_action(&mut self, label: &'static str, action: TuiAction) {
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

async fn delete_project_batch<S: OutputSink>(
    projects: Vec<Project>,
    config: AppConfig,
    purge: bool,
    sink: &mut S,
) -> Result<ActionBatchResult> {
    let mut failures = Vec::new();
    let remove_action = Action::Remove(RemoveOptions {
        volumes: true,
        remove_orphans: false,
        rmi: None,
    });

    for project in projects {
        if purge {
            let result = compose::run_action_batch(
                vec![project.clone()],
                config.clone(),
                remove_action.clone(),
                sink,
            )
            .await?;
            if let Some(failure) = result.failed_projects.into_iter().next() {
                failures.push(failure);
                continue;
            }
        }

        let command = format!("delete project dir {}", project.dir.display());
        sink.command_started(&project, &command);
        match delete_project(&config.compose_dir, &project) {
            Ok(()) => sink.command_finished(&project, true, Some(0)),
            Err(err) => {
                sink.command_output(&err.to_string(), true);
                sink.command_finished(&project, false, None);
                failures.push(ProjectFailure {
                    project: project.name.clone(),
                    message: err.to_string(),
                });
            }
        }
    }

    Ok(ActionBatchResult {
        failed_projects: failures,
    })
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
        draw_help(frame, app);
    }

    if app.pending_action.is_some() {
        draw_confirmation(frame, app);
    }

    if app.input_mode == InputMode::NewProject {
        draw_new_project(frame, app);
    }

    if app.input_mode == InputMode::ContainerShell {
        draw_container_shell(frame, app);
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

fn draw_help(frame: &mut Frame, app: &App) {
    let area = centered_rect(frame.area(), 72, 70);
    let lines = help_lines();
    let max_scroll = lines.len().saturating_sub(1) as u16;
    let title = format!("Help {}/{}", app.help_scroll.min(max_scroll), max_scroll);

    let help = Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black))
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.help_scroll.min(max_scroll), 0));

    frame.render_widget(Clear, area);
    frame.render_widget(help, area);
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("TUI Help", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  j/k scroll  PgUp/PgDn page  ?/Esc/q close"),
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
        Line::from("  D                    Remove and delete volumes, with confirmation"),
        Line::from("  x                    Delete project files, with confirmation"),
        Line::from("  X                    Purge project, volumes, and files, with confirmation"),
        Line::from("  l                    Show logs"),
        Line::from("  e                    Edit current project compose file"),
        Line::from("  E                    Edit current project .env"),
        Line::from("  Alt+e                Edit .env.global"),
        Line::from("  c                    Open shell in a project container"),
        Line::from(""),
        Line::from("Container Shell"),
        Line::from("  Enter                Open selected container shell"),
        Line::from("  j / k or Up / Down   Move container selection"),
        Line::from("  Esc / q              Cancel"),
        Line::from(""),
        Line::from("General"),
        Line::from("  n                    Create a new project"),
        Line::from("  q                    Quit"),
        Line::from("  ?                    Toggle this help"),
    ]
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

fn draw_new_project(frame: &mut Frame, app: &App) {
    let area = centered_rect(frame.area(), 60, 28);
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "New Project",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(format!("Name: {}", app.new_project_name)),
        Line::from(""),
    ];

    if let Some(error) = &app.new_project_error {
        lines.push(Line::from(Span::styled(
            format!("Error: {error}"),
            Style::default().fg(Color::Red),
        )));
    } else {
        lines.push(Line::from("Allowed: lowercase letters, digits, - and _"));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("Enter create and edit"));
    lines.push(Line::from("Esc cancel"));

    let modal = Paragraph::new(lines)
        .block(
            Block::default()
                .title("New Project")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black))
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, area);
    frame.render_widget(modal, area);
}

fn draw_container_shell(frame: &mut Frame, app: &App) {
    let area = centered_rect(frame.area(), 68, 48);
    let Some(status) = app.selected_status() else {
        return;
    };
    let containers = running_containers(status);
    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!("Container Shell: {}", status.project.name),
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
    ];

    for (index, container) in containers.iter().enumerate() {
        let mark = if index == app.shell_container_index {
            ">"
        } else {
            " "
        };
        let service = container.service.as_deref().unwrap_or("-");
        let status = container.status.as_deref().unwrap_or("");
        lines.push(Line::from(format!(
            "{mark} {service:<16} {}  {status}",
            container.name
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("Enter open shell"));
    lines.push(Line::from("j/k or Up/Down move"));
    lines.push(Line::from("Esc/q cancel"));

    let modal = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Container Shell")
                .borders(Borders::ALL)
                .style(Style::default().bg(Color::Black))
                .border_style(Style::default().fg(Color::Cyan)),
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

fn running_containers(status: &ProjectStatus) -> Vec<&ContainerStatus> {
    status
        .containers
        .iter()
        .filter(|container| container.state == "running")
        .collect()
}

fn format_failures(failures: &[ProjectFailure]) -> String {
    failures
        .iter()
        .map(|failure| format!("{}: {}", failure.project, failure.message))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn configured_editor() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

fn suspend_for_editor(terminal: &mut DefaultTerminal, launch: &EditorLaunch) -> Result<()> {
    ratatui::try_restore().context("failed to suspend TUI for editor")?;
    let editor_result = run_editor(&launch.editor, &launch.path);
    let restore_result = ratatui::try_init()
        .map(|new_terminal| {
            *terminal = new_terminal;
        })
        .context("failed to restore TUI after editor");

    match (editor_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(editor_err), Ok(())) => Err(editor_err),
        (Ok(()), Err(restore_err)) => Err(restore_err),
        (Err(editor_err), Err(restore_err)) => Err(anyhow!(
            "{editor_err}; additionally failed to restore TUI after editor: {restore_err}"
        )),
    }
}

fn suspend_for_shell(terminal: &mut DefaultTerminal, launch: &ShellLaunch) -> Result<()> {
    ratatui::try_restore().context("failed to suspend TUI for shell")?;
    let shell_result = run_shell(launch);
    let restore_result = ratatui::try_init()
        .map(|new_terminal| {
            *terminal = new_terminal;
        })
        .context("failed to restore TUI after shell");

    match (shell_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(shell_err), Ok(())) => Err(shell_err),
        (Ok(()), Err(restore_err)) => Err(restore_err),
        (Err(shell_err), Err(restore_err)) => Err(anyhow!(
            "{shell_err}; additionally failed to restore TUI after shell: {restore_err}"
        )),
    }
}

fn run_editor(editor: &str, path: &Path) -> Result<()> {
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("lazydc-editor")
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor {editor}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "editor exited with {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ))
    }
}

fn run_shell(launch: &ShellLaunch) -> Result<()> {
    let command = shell_command(&launch.docker_bin, &launch.container_id);
    let status = Command::new(&command.program)
        .args(&command.args)
        .status()
        .with_context(|| format!("failed to launch {}", command.display))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "shell exited with {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ))
    }
}

#[derive(Debug)]
struct ShellCommand {
    program: PathBuf,
    args: Vec<String>,
    display: String,
}

fn shell_command(docker_bin: &Path, container_id: &str) -> ShellCommand {
    let args = vec![
        "exec".to_string(),
        "-it".to_string(),
        container_id.to_string(),
        "sh".to_string(),
        "-lc".to_string(),
        "command -v bash >/dev/null 2>&1 && exec bash || exec sh".to_string(),
    ];
    let display = std::iter::once(shell_command_escape(docker_bin.to_string_lossy().as_ref()))
        .chain(args.iter().map(|arg| shell_command_escape(arg)))
        .collect::<Vec<_>>()
        .join(" ");

    ShellCommand {
        program: docker_bin.to_path_buf(),
        args,
        display,
    }
}

fn shell_command_escape(input: &str) -> String {
    if input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '='))
    {
        return input.to_string();
    }
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn ensure_edit_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create dir {}", parent.display()))?;
    }

    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to create file {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_test_editor() -> MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("VISUAL", "test-editor");
        std::env::remove_var("EDITOR");
        guard
    }

    fn without_test_editor() -> MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        guard
    }

    fn test_config() -> AppConfig {
        AppConfig {
            compose_dir: PathBuf::from("/tmp/lazydc-test"),
            docker_bin: PathBuf::from("docker"),
            refresh_interval_ms: 2_000,
            default_log_lines: 100,
        }
    }

    fn test_status() -> ProjectStatus {
        test_status_for(Path::new("/tmp/lazydc-test"), "app")
    }

    fn test_status_for(compose_dir: &Path, name: &str) -> ProjectStatus {
        let dir = compose_dir.join(name);
        ProjectStatus {
            project: Project {
                name: name.to_string(),
                dir: dir.clone(),
                compose_file: dir.join("docker-compose.yaml"),
                env_file: None,
            },
            summary: docker_api::ProjectSummary {
                state: ProjectState::Stopped,
                health: HealthSummary::Unknown,
                running_containers: 0,
                total_containers: 0,
                metrics: None,
            },
            containers: Vec::new(),
            error: None,
        }
    }

    fn test_status_with_containers(
        compose_dir: &Path,
        name: &str,
        containers: Vec<ContainerStatus>,
    ) -> ProjectStatus {
        ProjectStatus {
            containers,
            ..test_status_for(compose_dir, name)
        }
    }

    fn test_container(id: &str, name: &str, service: &str, state: &str) -> ContainerStatus {
        ContainerStatus {
            id: id.to_string(),
            name: name.to_string(),
            service: Some(service.to_string()),
            image: Some("image:latest".to_string()),
            state: state.to_string(),
            health: None,
            status: Some(state.to_string()),
            started_at: None,
            ports: Vec::new(),
            metrics: None,
        }
    }

    fn test_config_for(compose_dir: &Path) -> AppConfig {
        AppConfig {
            compose_dir: compose_dir.to_path_buf(),
            docker_bin: PathBuf::from("docker"),
            refresh_interval_ms: 2_000,
            default_log_lines: 100,
        }
    }

    #[derive(Default)]
    struct TestSink {
        started: Vec<String>,
        finished: Vec<(String, bool, Option<i32>)>,
        stderr: Vec<String>,
    }

    impl OutputSink for TestSink {
        fn command_started(&mut self, _project: &Project, command: &str) {
            self.started.push(command.to_string());
        }

        fn command_output(&mut self, line: &str, stderr: bool) {
            if stderr {
                self.stderr.push(line.to_string());
            }
        }

        fn command_finished(&mut self, project: &Project, success: bool, exit_code: Option<i32>) {
            self.finished
                .push((project.name.clone(), success, exit_code));
        }
    }

    #[test]
    fn n_enters_new_project_input_mode() {
        let mut app = App::new(test_config());
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::NewProject);
        assert_eq!(app.new_project_name, "");
    }

    #[test]
    fn help_overlay_scrolls_and_resets_on_close() {
        let mut app = App::new(test_config());
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            tx.clone(),
        );
        assert!(matches!(outcome, KeyOutcome::Continue));
        assert!(app.show_help);
        assert_eq!(app.help_scroll, 0);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            tx.clone(),
        );
        assert_eq!(app.help_scroll, 1);

        app.handle_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            tx.clone(),
        );
        assert_eq!(app.help_scroll, 9);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            tx.clone(),
        );
        assert_eq!(app.help_scroll, 8);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), tx);
        assert!(!app.show_help);
        assert_eq!(app.help_scroll, 0);
    }

    #[test]
    fn help_text_lists_destructive_shortcuts() {
        let rendered = help_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("D                    Remove and delete volumes"));
        assert!(rendered.contains("x                    Delete project files"));
        assert!(rendered.contains("X                    Purge project, volumes, and files"));
    }

    #[test]
    fn help_text_lists_edit_shortcuts() {
        let rendered = help_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("e                    Edit current project compose file"));
        assert!(rendered.contains("E                    Edit current project .env"));
        assert!(rendered.contains("Alt+e                Edit .env.global"));
    }

    #[test]
    fn help_text_lists_container_shell_shortcut() {
        let rendered = help_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("c                    Open shell in a project container"));
        assert!(rendered.contains("Container Shell"));
        assert!(rendered.contains("Enter                Open selected container shell"));
    }

    #[test]
    fn esc_cancels_new_project_input() {
        let mut app = App::new(test_config());
        app.start_new_project();
        app.new_project_name = "app".to_string();
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.new_project_name, "");
        assert!(app.new_project_error.is_none());
    }

    #[test]
    fn enter_with_invalid_name_stays_in_new_project_input() {
        let mut app = App::new(test_config());
        app.start_new_project();
        app.new_project_name = "App".to_string();
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::NewProject);
        assert!(app.new_project_error.is_some());
    }

    #[test]
    fn enter_without_editor_exits_new_project_input() {
        let _env = without_test_editor();
        let mut app = App::new(test_config());
        app.start_new_project();
        app.new_project_name = "app".to_string();
        let launch = app.submit_new_project_with_editor(None);

        assert!(launch.is_none());
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.new_project_name, "");
        assert!(app.status_message.contains("VISUAL or EDITOR"));
    }

    #[test]
    fn e_opens_current_project_compose_file() {
        let _env = with_test_editor();
        let dir = tempdir().unwrap();
        let compose_dir = dir.path();
        let app_dir = compose_dir.join("app");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        let mut app = App::new(test_config_for(compose_dir));
        app.statuses = vec![test_status_for(compose_dir, "app")];

        let launch = app.edit_selected_compose().expect("compose should open");

        assert_eq!(launch.editor, "test-editor");
        assert_eq!(launch.target_label, "compose for app");
        assert_eq!(launch.path, app_dir.join("docker-compose.yaml"));
        assert_eq!(launch.created_project, None);
        assert!(app.status_message.contains("Opening editor"));
    }

    #[test]
    fn uppercase_e_creates_and_opens_current_project_env() {
        let _env = with_test_editor();
        let dir = tempdir().unwrap();
        let compose_dir = dir.path();
        let app_dir = compose_dir.join("app");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        let mut app = App::new(test_config_for(compose_dir));
        app.statuses = vec![test_status_for(compose_dir, "app")];
        let env_file = app_dir.join(".env");

        let launch = app.edit_selected_env().expect("env should open");

        assert_eq!(launch.editor, "test-editor");
        assert_eq!(launch.target_label, ".env for app");
        assert_eq!(launch.path, env_file);
        assert_eq!(fs::read_to_string(&launch.path).unwrap(), "");
    }

    #[test]
    fn alt_e_creates_and_opens_global_env_without_project() {
        let _env = with_test_editor();
        let dir = tempdir().unwrap();
        let compose_dir = dir.path().join("compose");
        let mut app = App::new(test_config_for(&compose_dir));

        let launch = app.edit_global_env().expect("global env should open");

        assert_eq!(launch.editor, "test-editor");
        assert_eq!(launch.target_label, ".env.global");
        assert_eq!(launch.path, compose_dir.join(".env.global"));
        assert_eq!(fs::read_to_string(&launch.path).unwrap(), "");
    }

    #[test]
    fn edit_shortcuts_return_open_editor() {
        let _env = with_test_editor();
        let dir = tempdir().unwrap();
        let compose_dir = dir.path();
        let app_dir = compose_dir.join("app");
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(app_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        let mut app = App::new(test_config_for(compose_dir));
        app.statuses = vec![test_status_for(compose_dir, "app")];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), tx);

        match outcome {
            KeyOutcome::OpenEditor(launch) => {
                assert_eq!(launch.target_label, "compose for app");
                assert_eq!(launch.path, app_dir.join("docker-compose.yaml"));
            }
            outcome => panic!("expected editor launch, got {outcome:?}"),
        }
    }

    #[test]
    fn alt_e_shortcut_returns_global_env_editor() {
        let _env = with_test_editor();
        let dir = tempdir().unwrap();
        let compose_dir = dir.path().join("compose");
        let mut app = App::new(test_config_for(&compose_dir));
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), tx);

        match outcome {
            KeyOutcome::OpenEditor(launch) => {
                assert_eq!(launch.target_label, ".env.global");
                assert_eq!(launch.path, compose_dir.join(".env.global"));
            }
            outcome => panic!("expected editor launch, got {outcome:?}"),
        }
    }

    #[test]
    fn e_and_uppercase_e_ignore_marked_projects() {
        let _env = with_test_editor();
        let dir = tempdir().unwrap();
        let compose_dir = dir.path();
        let app_dir = compose_dir.join("app");
        let other_dir = compose_dir.join("other");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&other_dir).unwrap();
        fs::write(app_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        fs::write(other_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        let mut app = App::new(test_config_for(compose_dir));
        app.statuses = vec![
            test_status_for(compose_dir, "app"),
            test_status_for(compose_dir, "other"),
        ];
        app.selected_index = 0;
        app.marked.insert("other".to_string());

        let compose_launch = app.edit_selected_compose().expect("compose should open");
        let env_launch = app.edit_selected_env().expect("env should open");

        assert_eq!(compose_launch.path, app_dir.join("docker-compose.yaml"));
        assert_eq!(env_launch.path, app_dir.join(".env"));
        assert!(!other_dir.join(".env").exists());
    }

    #[test]
    fn e_without_project_reports_no_selection() {
        let mut app = App::new(test_config());

        let launch = app.edit_selected_compose();

        assert!(launch.is_none());
        assert_eq!(app.status_message, "No project selected");
    }

    #[test]
    fn edit_without_editor_reports_configuration_error() {
        let _env = without_test_editor();
        let mut app = App::new(test_config());

        let launch =
            app.build_editor_launch("target".to_string(), PathBuf::from("/tmp/target"), false);

        assert!(launch.is_none());
        assert!(app.status_message.contains("VISUAL or EDITOR"));
    }

    #[test]
    fn c_opens_container_shell_selector_for_running_containers() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status_with_containers(
            Path::new("/tmp/lazydc-test"),
            "app",
            vec![
                test_container("running-id", "app-web-1", "web", "running"),
                test_container("exited-id", "app-job-1", "job", "exited"),
            ],
        )];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::ContainerShell);
        assert_eq!(app.shell_container_index, 0);
        assert!(app.status_message.contains("Select container"));
    }

    #[test]
    fn c_without_project_reports_no_selection() {
        let mut app = App::new(test_config());
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.status_message, "No project selected");
    }

    #[test]
    fn c_without_running_containers_reports_status() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status_with_containers(
            Path::new("/tmp/lazydc-test"),
            "app",
            vec![test_container("exited-id", "app-job-1", "job", "exited")],
        )];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.status_message, "No running containers");
    }

    #[test]
    fn container_shell_selector_moves_and_clamps() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status_with_containers(
            Path::new("/tmp/lazydc-test"),
            "app",
            vec![
                test_container("web-id", "app-web-1", "web", "running"),
                test_container("db-id", "app-db-1", "db", "running"),
            ],
        )];
        app.start_container_shell();

        app.handle_container_shell_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.shell_container_index, 1);

        app.handle_container_shell_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.shell_container_index, 1);

        app.handle_container_shell_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.shell_container_index, 0);
    }

    #[test]
    fn container_shell_selector_cancel_resets_mode() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status_with_containers(
            Path::new("/tmp/lazydc-test"),
            "app",
            vec![test_container("web-id", "app-web-1", "web", "running")],
        )];
        app.start_container_shell();

        let outcome =
            app.handle_container_shell_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(matches!(outcome, KeyOutcome::Continue));
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.shell_container_index, 0);
        assert_eq!(app.status_message, "Container shell cancelled");
    }

    #[test]
    fn container_shell_selector_enter_returns_shell_launch() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status_with_containers(
            Path::new("/tmp/lazydc-test"),
            "app",
            vec![
                test_container("web-id", "app-web-1", "web", "running"),
                test_container("db-id", "app-db-1", "db", "running"),
            ],
        )];
        app.start_container_shell();
        app.shell_container_index = 1;

        let outcome =
            app.handle_container_shell_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        match outcome {
            KeyOutcome::OpenShell(launch) => {
                assert_eq!(launch.docker_bin, PathBuf::from("docker"));
                assert_eq!(launch.project_name, "app");
                assert_eq!(launch.container_id, "db-id");
                assert_eq!(launch.container_name, "app-db-1");
            }
            outcome => panic!("expected shell launch, got {outcome:?}"),
        }
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.shell_container_index, 0);
    }

    #[test]
    fn c_ignores_marked_projects() {
        let mut app = App::new(test_config());
        app.statuses = vec![
            test_status_with_containers(
                Path::new("/tmp/lazydc-test"),
                "app",
                vec![test_container("app-id", "app-web-1", "web", "running")],
            ),
            test_status_with_containers(
                Path::new("/tmp/lazydc-test"),
                "other",
                vec![test_container("other-id", "other-web-1", "web", "running")],
            ),
        ];
        app.selected_index = 0;
        app.marked.insert("other".to_string());
        let (tx, _rx) = mpsc::unbounded_channel();

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), tx);
        let launch = app.selected_shell_launch().expect("shell should launch");

        assert_eq!(launch.container_id, "app-id");
    }

    #[test]
    fn shell_command_uses_docker_exec_with_bash_fallback() {
        let command = shell_command(Path::new("/usr/bin/docker"), "container-id");

        assert_eq!(command.program, PathBuf::from("/usr/bin/docker"));
        assert_eq!(
            command.args,
            vec![
                "exec",
                "-it",
                "container-id",
                "sh",
                "-lc",
                "command -v bash >/dev/null 2>&1 && exec bash || exec sh"
            ]
        );
        assert!(command
            .display
            .contains("/usr/bin/docker exec -it container-id"));
    }

    #[test]
    fn d_confirms_remove_without_volumes() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status()];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        let pending = app
            .pending_action
            .expect("remove should require confirmation");
        assert_eq!(pending.label, "remove");
        match pending.action {
            TuiAction::Compose(Action::Remove(options)) => assert!(!options.volumes),
            action => panic!("expected remove action, got {action:?}"),
        }
    }

    #[test]
    fn uppercase_d_confirms_remove_with_volumes() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status()];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        let pending = app
            .pending_action
            .expect("remove should require confirmation");
        assert_eq!(pending.label, "remove with volumes");
        match pending.action {
            TuiAction::Compose(Action::Remove(options)) => assert!(options.volumes),
            action => panic!("expected remove action, got {action:?}"),
        }
    }

    #[test]
    fn x_confirms_delete_project_files() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status()];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        let pending = app
            .pending_action
            .expect("delete should require confirmation");
        assert_eq!(pending.label, "delete project files");
        assert!(matches!(pending.action, TuiAction::DeleteProjectFiles));
    }

    #[test]
    fn uppercase_x_confirms_purge_project() {
        let mut app = App::new(test_config());
        app.statuses = vec![test_status()];
        let (tx, _rx) = mpsc::unbounded_channel();

        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE), tx);

        assert!(matches!(outcome, KeyOutcome::Continue));
        let pending = app
            .pending_action
            .expect("purge should require confirmation");
        assert_eq!(pending.label, "purge project");
        assert!(matches!(pending.action, TuiAction::PurgeProject));
    }

    #[tokio::test]
    async fn delete_project_batch_deletes_project_files_without_purge() {
        let dir = tempdir().unwrap();
        let project = create_project(dir.path(), "app").unwrap();
        let config = AppConfig {
            compose_dir: dir.path().to_path_buf(),
            docker_bin: PathBuf::from("docker"),
            refresh_interval_ms: 2_000,
            default_log_lines: 100,
        };
        let mut sink = TestSink::default();

        let result = delete_project_batch(vec![project.clone()], config, false, &mut sink)
            .await
            .unwrap();

        assert!(result.failed_projects.is_empty());
        assert!(!project.dir.exists());
        assert_eq!(
            sink.started,
            vec![format!("delete project dir {}", project.dir.display())]
        );
        assert_eq!(sink.finished, vec![("app".to_string(), true, Some(0))]);
        assert!(sink.stderr.is_empty());
    }
}
