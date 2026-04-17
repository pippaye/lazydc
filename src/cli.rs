use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use dialoguer::MultiSelect;
use serde::Serialize;
use std::path::PathBuf;

use crate::project::Project;

#[derive(Debug, Parser)]
#[command(
    name = "lazydc",
    version,
    about = "Manage docker compose homelab projects",
    after_help = "Configuration:\n  Default config file: ~/.config/lazydc/config.toml\n  Supported keys: compose_dir, docker_bin, refresh_interval_ms, default_log_lines\n  Precedence: CLI flags override config file values.\n  See: lazydc example-config"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub compose_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Tui,
    ExampleConfig,
    List {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[command(flatten)]
        targets: TargetArgs,
        #[arg(long)]
        json: bool,
    },
    Deploy(TargetArgs),
    Update(TargetArgs),
    Stop(TargetArgs),
    Restart(TargetArgs),
    Remove(RemoveArgs),
    Logs(LogsArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TargetArgs {
    #[arg(value_name = "PROJECT")]
    pub projects: Vec<String>,
    #[arg(long, conflicts_with = "select")]
    pub all: bool,
    #[arg(long)]
    pub select: bool,
}

impl TargetArgs {
    pub fn validate(&self) -> Result<()> {
        if !self.projects.is_empty() && (self.all || self.select) {
            return Err(anyhow!(
                "cannot combine explicit projects with --all or --select"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Args)]
pub struct RemoveArgs {
    #[command(flatten)]
    pub targets: TargetArgs,
    #[arg(long)]
    pub volumes: bool,
    #[arg(long)]
    pub remove_orphans: bool,
    #[arg(long)]
    pub rmi: Option<RmiMode>,
}

#[derive(Debug, Clone, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub targets: TargetArgs,
    #[arg(long)]
    pub follow: bool,
    #[arg(long)]
    pub tail: Option<usize>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RmiMode {
    Local,
    All,
}

impl std::fmt::Display for RmiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::All => write!(f, "all"),
        }
    }
}

pub fn select_projects(projects: &[Project]) -> Result<Vec<String>> {
    let items = projects
        .iter()
        .map(|project| format!("{}  {}", project.name, project.compose_file.display()))
        .collect::<Vec<_>>();

    let selections = MultiSelect::new()
        .with_prompt("Select projects")
        .items(&items)
        .interact()?;

    Ok(selections
        .into_iter()
        .map(|index| projects[index].name.clone())
        .collect())
}
