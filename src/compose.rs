use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::ffi::OsString;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::cli::RmiMode;
use crate::config::AppConfig;
use crate::project::Project;

#[derive(Debug, Clone)]
pub enum Action {
    Deploy,
    Update,
    Stop,
    Restart,
    Remove(RemoveOptions),
    Logs(LogsOptions),
}

#[derive(Debug, Clone)]
pub struct RemoveOptions {
    pub volumes: bool,
    pub remove_orphans: bool,
    pub rmi: Option<RmiMode>,
}

#[derive(Debug, Clone)]
pub struct LogsOptions {
    pub follow: bool,
    pub tail: usize,
}

#[derive(Debug, Clone)]
pub struct PreparedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub display: String,
}

#[derive(Debug, Default)]
pub struct CliOutputSink;

#[derive(Debug, Default)]
pub struct ActionBatchResult {
    pub failed_projects: Vec<ProjectFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectFailure {
    pub project: String,
    pub message: String,
}

pub trait OutputSink {
    fn command_started(&mut self, project: &Project, command: &str);
    fn command_output(&mut self, line: &str, stderr: bool);
    fn command_finished(&mut self, project: &Project, success: bool, exit_code: Option<i32>);
}

impl OutputSink for CliOutputSink {
    fn command_started(&mut self, project: &Project, command: &str) {
        println!("\n==> [{}] {}", project.name, command);
    }

    fn command_output(&mut self, line: &str, stderr: bool) {
        if stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    fn command_finished(&mut self, project: &Project, success: bool, exit_code: Option<i32>) {
        let code = exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        println!(
            "-- [{}] {} (exit {})",
            project.name,
            if success { "ok" } else { "failed" },
            code
        );
    }
}

pub async fn run_action_batch<S: OutputSink>(
    projects: Vec<Project>,
    config: AppConfig,
    action: Action,
    sink: &mut S,
) -> Result<ActionBatchResult> {
    let mut failures = Vec::new();

    for project in projects {
        let commands = action.prepare_commands(&config, &project);
        for prepared in commands {
            sink.command_started(&project, &prepared.display);
            let result = run_prepared_command(&prepared, |line, stderr| {
                sink.command_output(line, stderr);
            })
            .await;

            match result {
                Ok(exit) => {
                    sink.command_finished(&project, exit.success, exit.exit_code);
                    if !exit.success {
                        failures.push(ProjectFailure {
                            project: project.name.clone(),
                            message: format!(
                                "command exited with {}",
                                exit.exit_code
                                    .map(|code| code.to_string())
                                    .unwrap_or_else(|| "signal".to_string())
                            ),
                        });
                        break;
                    }
                }
                Err(err) => {
                    sink.command_finished(&project, false, None);
                    failures.push(ProjectFailure {
                        project: project.name.clone(),
                        message: err.to_string(),
                    });
                    break;
                }
            }
        }
    }

    Ok(ActionBatchResult {
        failed_projects: failures,
    })
}

#[derive(Debug, Clone)]
pub struct CommandExit {
    pub success: bool,
    pub exit_code: Option<i32>,
}

impl Action {
    pub fn prepare_commands(&self, config: &AppConfig, project: &Project) -> Vec<PreparedCommand> {
        match self {
            Self::Deploy => vec![build_compose_command(config, project, ["up", "-d"])],
            Self::Update => vec![
                build_compose_command(config, project, ["pull"]),
                build_compose_command(config, project, ["up", "-d"]),
            ],
            Self::Stop => vec![build_compose_command(config, project, ["stop"])],
            Self::Restart => vec![build_compose_command(config, project, ["restart"])],
            Self::Remove(options) => {
                let mut args = vec!["down".to_string()];
                if options.volumes {
                    args.push("-v".to_string());
                }
                if options.remove_orphans {
                    args.push("--remove-orphans".to_string());
                }
                if let Some(rmi) = options.rmi {
                    args.push("--rmi".to_string());
                    args.push(rmi.to_string());
                }
                vec![build_compose_command_owned(config, project, args)]
            }
            Self::Logs(options) => vec![build_compose_command_owned(config, project, {
                let mut args = vec![
                    "logs".to_string(),
                    "--tail".to_string(),
                    options.tail.to_string(),
                ];
                if options.follow {
                    args.push("--follow".to_string());
                }
                args
            })],
        }
    }
}

fn build_compose_command<const N: usize>(
    config: &AppConfig,
    project: &Project,
    subcommand: [&str; N],
) -> PreparedCommand {
    build_compose_command_owned(
        config,
        project,
        subcommand.iter().map(|item| item.to_string()).collect(),
    )
}

fn build_compose_command_owned(
    config: &AppConfig,
    project: &Project,
    subcommand: Vec<String>,
) -> PreparedCommand {
    let mut args = vec![OsString::from("compose")];
    if config.env_file().exists() {
        args.push(OsString::from("--env-file"));
        args.push(config.env_file().into_os_string());
    }
    if let Some(project_env) = &project.env_file {
        args.push(OsString::from("--env-file"));
        args.push(project_env.clone().into_os_string());
    }
    args.push(OsString::from("--project-name"));
    args.push(OsString::from(project.name.clone()));
    args.push(OsString::from("-f"));
    args.push(project.compose_file.clone().into_os_string());
    for item in subcommand {
        args.push(OsString::from(item));
    }

    let display = render_command(&config.docker_bin, &args);
    PreparedCommand {
        program: config.docker_bin.clone(),
        args,
        cwd: project.dir.clone(),
        display,
    }
}

pub async fn run_prepared_command<F>(
    prepared: &PreparedCommand,
    mut on_line: F,
) -> Result<CommandExit>
where
    F: FnMut(&str, bool),
{
    let mut child = Command::new(&prepared.program)
        .args(&prepared.args)
        .current_dir(&prepared.cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", prepared.display))?;

    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take().context("missing child stderr")?;
    let (tx, mut rx) = mpsc::unbounded_channel();

    tokio::spawn(read_stream(stdout, false, tx.clone()));
    tokio::spawn(read_stream(stderr, true, tx));

    let mut wait_fut = Box::pin(child.wait());
    let mut exit_code = None;

    loop {
        tokio::select! {
            maybe_line = rx.recv() => {
                if let Some((stderr, line)) = maybe_line {
                    on_line(&line, stderr);
                } else if exit_code.is_some() {
                    break;
                }
            }
            status = &mut wait_fut, if exit_code.is_none() => {
                let status = status?;
                exit_code = status.code();
                if rx.is_closed() {
                    break;
                }
            }
        }
    }

    let success = exit_code == Some(0);
    if exit_code.is_none() {
        return Err(anyhow!("process terminated unexpectedly"));
    }

    Ok(CommandExit { success, exit_code })
}

async fn read_stream<R>(stream: R, stderr: bool, tx: mpsc::UnboundedSender<(bool, String)>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let _ = tx.send((stderr, line));
    }
}

fn render_command(program: &std::path::Path, args: &[OsString]) -> String {
    std::iter::once(shell_escape(program.to_string_lossy().as_ref()))
        .chain(
            args.iter()
                .map(|arg| shell_escape(arg.to_string_lossy().as_ref())),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(input: &str) -> String {
    if input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '='))
    {
        return input.to_string();
    }
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::project::Project;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn sample_config() -> AppConfig {
        AppConfig {
            compose_dir: PathBuf::from("/srv/cmps"),
            docker_bin: PathBuf::from("docker"),
            refresh_interval_ms: 2000,
            default_log_lines: 50,
        }
    }

    fn sample_project() -> Project {
        Project {
            name: "app".to_string(),
            dir: PathBuf::from("/srv/cmps/app"),
            compose_file: PathBuf::from("/srv/cmps/app/docker-compose.yaml"),
            env_file: Some(PathBuf::from("/srv/cmps/app/.env")),
        }
    }

    #[test]
    fn update_builds_pull_then_up() {
        let commands = Action::Update.prepare_commands(&sample_config(), &sample_project());
        assert_eq!(commands.len(), 2);
        assert!(commands[0].display.contains("pull"));
        assert!(commands[1].display.contains("up -d"));
    }

    #[test]
    fn remove_can_add_rmi_and_volumes() {
        let command = Action::Remove(RemoveOptions {
            volumes: true,
            remove_orphans: true,
            rmi: Some(RmiMode::All),
        })
        .prepare_commands(&sample_config(), &sample_project())
        .pop()
        .unwrap();
        assert!(command
            .display
            .contains("down -v --remove-orphans --rmi all"));
    }

    #[test]
    fn compose_command_includes_global_and_project_env_files() {
        let dir = tempdir().unwrap();
        let compose_dir = dir.path().join("compose");
        let project_dir = compose_dir.join("app");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(compose_dir.join(".env.global"), "GLOBAL=1\n").unwrap();
        fs::write(project_dir.join(".env"), "PROJECT=1\n").unwrap();
        fs::write(project_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();

        let config = AppConfig {
            compose_dir: compose_dir.clone(),
            docker_bin: PathBuf::from("docker"),
            refresh_interval_ms: 2000,
            default_log_lines: 50,
        };
        let project = Project {
            name: "app".to_string(),
            dir: project_dir.clone(),
            compose_file: project_dir.join("docker-compose.yaml"),
            env_file: Some(project_dir.join(".env")),
        };

        let command = Action::Deploy
            .prepare_commands(&config, &project)
            .pop()
            .unwrap();
        assert!(command.display.contains(&format!(
            "--env-file {}",
            compose_dir.join(".env.global").display()
        )));
        assert!(command.display.contains(&format!(
            "--env-file {}",
            project_dir.join(".env").display()
        )));
    }
}
