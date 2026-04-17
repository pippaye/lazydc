mod cli;
mod compose;
mod config;
mod docker_api;
mod project;
mod tui;

use anyhow::{anyhow, Result};
use clap::Parser;
use cli::{Cli, Commands, TargetArgs};
use compose::{Action, ActionBatchResult, CliOutputSink};
use config::AppConfig;
use project::{discover_projects, Project};
use std::collections::BTreeSet;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load(cli.config.as_deref(), cli.compose_dir.as_deref())?;

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => tui::run_tui(config).await,
        Commands::ExampleConfig => {
            print!("{}", config::example_config());
            Ok(())
        }
        Commands::List { json } => run_list(&config, json).await,
        Commands::Status { targets, json } => run_status(&config, targets, json).await,
        Commands::Deploy(args) => run_action(&config, args, Action::Deploy).await,
        Commands::Update(args) => run_action(&config, args, Action::Update).await,
        Commands::Stop(args) => run_action(&config, args, Action::Stop).await,
        Commands::Restart(args) => run_action(&config, args, Action::Restart).await,
        Commands::Remove(args) => {
            let remove = compose::RemoveOptions {
                volumes: args.volumes,
                remove_orphans: args.remove_orphans,
                rmi: args.rmi,
            };
            run_action(&config, args.targets, Action::Remove(remove)).await
        }
        Commands::Logs(args) => {
            let logs = compose::LogsOptions {
                follow: args.follow,
                tail: args.tail.unwrap_or(config.default_log_lines),
            };
            run_action(&config, args.targets, Action::Logs(logs)).await
        }
    }
}

async fn run_list(config: &AppConfig, json: bool) -> Result<()> {
    let projects = discover_projects(&config.compose_dir)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&projects)?);
        return Ok(());
    }

    for project in projects {
        println!("{:<20} {}", project.name, project.compose_file.display());
    }
    Ok(())
}

async fn run_status(config: &AppConfig, target_args: TargetArgs, json: bool) -> Result<()> {
    let projects = resolve_projects(config, target_args)?;
    let statuses = docker_api::load_statuses_for_projects(&projects).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
        return Ok(());
    }

    for status in statuses {
        println!(
            "{:<20} {:<8} {:<16} {}",
            status.project.name,
            status.summary.state_label(),
            status.summary.health_label(),
            status.summary.running_summary()
        );
    }
    Ok(())
}

async fn run_action(config: &AppConfig, target_args: TargetArgs, action: Action) -> Result<()> {
    let projects = resolve_projects(config, target_args)?;
    if projects.is_empty() {
        return Err(anyhow!("no projects selected"));
    }

    let mut sink = CliOutputSink::default();
    let result = compose::run_action_batch(projects, config.clone(), action, &mut sink).await?;
    handle_batch_result(result)
}

fn resolve_projects(config: &AppConfig, args: TargetArgs) -> Result<Vec<Project>> {
    args.validate()?;
    let discovered = discover_projects(&config.compose_dir)?;
    let names: BTreeSet<_> = discovered
        .iter()
        .map(|project| project.name.clone())
        .collect();

    let selected_names = if !args.projects.is_empty() {
        args.projects
    } else if args.select {
        cli::select_projects(&discovered)?
    } else {
        discovered
            .iter()
            .map(|project| project.name.clone())
            .collect()
    };

    for name in &selected_names {
        if !names.contains(name) {
            return Err(anyhow!("unknown project: {name}"));
        }
    }

    let selected = discovered
        .into_iter()
        .filter(|project| selected_names.iter().any(|name| name == &project.name))
        .collect();
    Ok(selected)
}

fn handle_batch_result(result: ActionBatchResult) -> Result<()> {
    if result.failed_projects.is_empty() {
        return Ok(());
    }

    let failures = result
        .failed_projects
        .iter()
        .map(|failure| format!("{} ({})", failure.project, failure.message))
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!("one or more projects failed: {failures}"))
}
