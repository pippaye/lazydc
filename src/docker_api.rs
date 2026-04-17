use anyhow::{Context, Result};
use bollard::container::{InspectContainerOptions, ListContainersOptions};
use bollard::models::{ContainerInspectResponse, ContainerSummary, Port};
use bollard::Docker;
use serde::Serialize;
use std::collections::HashMap;

use crate::project::Project;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectStatus {
    pub project: Project,
    pub summary: ProjectSummary,
    pub containers: Vec<ContainerStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerStatus {
    pub id: String,
    pub name: String,
    pub service: Option<String>,
    pub image: Option<String>,
    pub state: String,
    pub health: Option<String>,
    pub status: Option<String>,
    pub started_at: Option<String>,
    pub ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub state: ProjectState,
    pub health: HealthSummary,
    pub running_containers: usize,
    pub total_containers: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState {
    Running,
    Partial,
    Stopped,
    Missing,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSummary {
    Healthy,
    Unhealthy,
    Starting,
    NoHealthcheck,
    Unknown,
}

impl ProjectSummary {
    pub fn state_label(&self) -> &'static str {
        match self.state {
            ProjectState::Running => "running",
            ProjectState::Partial => "partial",
            ProjectState::Stopped => "stopped",
            ProjectState::Missing => "missing",
            ProjectState::Error => "error",
            ProjectState::Unknown => "unknown",
        }
    }

    pub fn health_label(&self) -> &'static str {
        match self.health {
            HealthSummary::Healthy => "healthy",
            HealthSummary::Unhealthy => "unhealthy",
            HealthSummary::Starting => "starting",
            HealthSummary::NoHealthcheck => "no-healthcheck",
            HealthSummary::Unknown => "unknown",
        }
    }

    pub fn running_summary(&self) -> String {
        format!(
            "{}/{} running",
            self.running_containers, self.total_containers
        )
    }
}

pub async fn load_statuses_for_projects(projects: &[Project]) -> Vec<ProjectStatus> {
    match Docker::connect_with_local_defaults() {
        Ok(docker) => {
            let mut statuses = Vec::with_capacity(projects.len());
            for project in projects {
                let status = match load_project_status(&docker, project).await {
                    Ok(status) => status,
                    Err(err) => ProjectStatus {
                        project: project.clone(),
                        summary: ProjectSummary {
                            state: ProjectState::Error,
                            health: HealthSummary::Unknown,
                            running_containers: 0,
                            total_containers: 0,
                        },
                        containers: Vec::new(),
                        error: Some(err.to_string()),
                    },
                };
                statuses.push(status);
            }
            statuses
        }
        Err(err) => projects
            .iter()
            .cloned()
            .map(|project| ProjectStatus {
                project,
                summary: ProjectSummary {
                    state: ProjectState::Unknown,
                    health: HealthSummary::Unknown,
                    running_containers: 0,
                    total_containers: 0,
                },
                containers: Vec::new(),
                error: Some(format!("docker api unavailable: {err}")),
            })
            .collect(),
    }
}

async fn load_project_status(docker: &Docker, project: &Project) -> Result<ProjectStatus> {
    let filters = HashMap::from([(
        "label".to_string(),
        vec![format!("com.docker.compose.project={}", project.name)],
    )]);
    let containers = docker
        .list_containers(Some(ListContainersOptions::<String> {
            all: true,
            filters,
            ..Default::default()
        }))
        .await
        .with_context(|| format!("failed to list containers for {}", project.name))?;

    let mut container_statuses = Vec::with_capacity(containers.len());
    for container in containers {
        container_statuses.push(inspect_container(docker, container).await?);
    }

    let summary = summarize_project(&container_statuses);
    Ok(ProjectStatus {
        project: project.clone(),
        summary,
        containers: container_statuses,
        error: None,
    })
}

async fn inspect_container(
    docker: &Docker,
    container: ContainerSummary,
) -> Result<ContainerStatus> {
    let id = container
        .id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let inspect = docker
        .inspect_container(&id, None::<InspectContainerOptions>)
        .await
        .with_context(|| format!("failed to inspect container {id}"))?;

    Ok(ContainerStatus {
        id: id.clone(),
        name: first_name(&container),
        service: container
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.docker.compose.service").cloned()),
        image: container.image.clone(),
        state: container
            .state
            .clone()
            .or_else(|| inspect_state(&inspect))
            .unwrap_or_else(|| "unknown".to_string()),
        health: inspect_health(&inspect),
        status: container.status.clone(),
        started_at: inspect_started_at(&inspect),
        ports: format_ports(container.ports.unwrap_or_default()),
    })
}

fn inspect_state(inspect: &ContainerInspectResponse) -> Option<String> {
    inspect
        .state
        .as_ref()
        .and_then(|state| state.status.clone())
        .map(|status| status.to_string())
}

fn inspect_health(inspect: &ContainerInspectResponse) -> Option<String> {
    inspect
        .state
        .as_ref()
        .and_then(|state| state.health.as_ref())
        .and_then(|health| health.status.clone())
        .map(|status| status.to_string())
}

fn inspect_started_at(inspect: &ContainerInspectResponse) -> Option<String> {
    inspect
        .state
        .as_ref()
        .and_then(|state| state.started_at.clone())
}

fn first_name(container: &ContainerSummary) -> String {
    container
        .names
        .as_ref()
        .and_then(|names| names.first())
        .map(|name| name.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_ports(ports: Vec<Port>) -> Vec<String> {
    ports
        .into_iter()
        .map(
            |port| match (port.public_port, port.private_port, port.typ) {
                (Some(public), private, Some(kind)) => format!("{public}->{private}/{kind}"),
                (None, private, Some(kind)) => format!("{private}/{kind}"),
                _ => "unknown".to_string(),
            },
        )
        .collect()
}

pub fn summarize_project(containers: &[ContainerStatus]) -> ProjectSummary {
    if containers.is_empty() {
        return ProjectSummary {
            state: ProjectState::Missing,
            health: HealthSummary::NoHealthcheck,
            running_containers: 0,
            total_containers: 0,
        };
    }

    let total = containers.len();
    let running = containers
        .iter()
        .filter(|container| container.state == "running")
        .count();
    let state = if running == total {
        ProjectState::Running
    } else if running == 0 {
        ProjectState::Stopped
    } else {
        ProjectState::Partial
    };

    let health = if containers
        .iter()
        .filter_map(|container| container.health.as_deref())
        .any(|health| health == "unhealthy")
    {
        HealthSummary::Unhealthy
    } else if containers
        .iter()
        .filter_map(|container| container.health.as_deref())
        .any(|health| health == "starting")
    {
        HealthSummary::Starting
    } else if containers
        .iter()
        .filter_map(|container| container.health.as_deref())
        .any(|health| health == "healthy")
    {
        HealthSummary::Healthy
    } else {
        HealthSummary::NoHealthcheck
    };

    ProjectSummary {
        state,
        health,
        running_containers: running,
        total_containers: total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_partial_project() {
        let containers = vec![
            ContainerStatus {
                id: "1".to_string(),
                name: "a".to_string(),
                service: None,
                image: None,
                state: "running".to_string(),
                health: Some("healthy".to_string()),
                status: None,
                started_at: None,
                ports: vec![],
            },
            ContainerStatus {
                id: "2".to_string(),
                name: "b".to_string(),
                service: None,
                image: None,
                state: "exited".to_string(),
                health: None,
                status: None,
                started_at: None,
                ports: vec![],
            },
        ];

        let summary = summarize_project(&containers);
        assert!(matches!(summary.state, ProjectState::Partial));
        assert_eq!(summary.running_containers, 1);
        assert_eq!(summary.total_containers, 2);
    }
}
