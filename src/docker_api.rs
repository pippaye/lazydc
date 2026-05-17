use anyhow::{anyhow, Context, Result};
use bollard::container::{
    BlkioStatsEntry, InspectContainerOptions, ListContainersOptions, MemoryStatsStats,
    NetworkStats, Stats, StatsOptions,
};
use bollard::models::{ContainerInspectResponse, ContainerSummary, Port};
use bollard::Docker;
use futures_util::StreamExt;
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
    pub metrics: Option<ContainerMetrics>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub state: ProjectState,
    pub health: HealthSummary,
    pub running_containers: usize,
    pub total_containers: usize,
    pub metrics: Option<ProjectMetrics>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContainerMetrics {
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub memory_percent: Option<f64>,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
    pub pids: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProjectMetrics {
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub memory_percent: Option<f64>,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
    pub pids: Option<u64>,
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
                            metrics: None,
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
                    metrics: None,
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

    let state = container
        .state
        .clone()
        .or_else(|| inspect_state(&inspect))
        .unwrap_or_else(|| "unknown".to_string());
    let metrics = if state == "running" {
        load_container_metrics(docker, &id).await.ok()
    } else {
        None
    };

    Ok(ContainerStatus {
        id: id.clone(),
        name: first_name(&container),
        service: container
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.docker.compose.service").cloned()),
        image: container.image.clone(),
        state,
        health: inspect_health(&inspect),
        status: container.status.clone(),
        started_at: inspect_started_at(&inspect),
        ports: format_ports(container.ports.unwrap_or_default()),
        metrics,
    })
}

async fn load_container_metrics(docker: &Docker, container_id: &str) -> Result<ContainerMetrics> {
    let mut stream = docker.stats(
        container_id,
        Some(StatsOptions {
            stream: false,
            one_shot: false,
        }),
    );

    let stats = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("docker stats returned no data for {container_id}"))?
        .with_context(|| format!("failed to load stats for container {container_id}"))?;

    Ok(container_metrics_from_stats(&stats))
}

fn container_metrics_from_stats(stats: &Stats) -> ContainerMetrics {
    let memory_usage_bytes = stats
        .memory_stats
        .usage
        .map(|usage| adjusted_memory_usage(usage, stats.memory_stats.stats));
    let memory_limit_bytes = stats.memory_stats.limit;
    let memory_percent = memory_usage_bytes
        .zip(memory_limit_bytes)
        .and_then(|(usage, limit)| percent(usage, limit));
    let (network_rx_bytes, network_tx_bytes) =
        network_totals(stats.network, stats.networks.as_ref());
    let (block_read_bytes, block_write_bytes) =
        block_io_totals(stats.blkio_stats.io_service_bytes_recursive.as_deref());

    ContainerMetrics {
        cpu_percent: calculate_cpu_percent(stats),
        memory_usage_bytes,
        memory_limit_bytes,
        memory_percent,
        network_rx_bytes,
        network_tx_bytes,
        block_read_bytes,
        block_write_bytes,
        pids: stats.pids_stats.current,
    }
}

fn calculate_cpu_percent(stats: &Stats) -> Option<f64> {
    let cpu_delta = stats
        .cpu_stats
        .cpu_usage
        .total_usage
        .saturating_sub(stats.precpu_stats.cpu_usage.total_usage);
    let system_delta = stats
        .cpu_stats
        .system_cpu_usage?
        .saturating_sub(stats.precpu_stats.system_cpu_usage?);
    let online_cpus = stats.cpu_stats.online_cpus.unwrap_or_else(|| {
        stats
            .cpu_stats
            .cpu_usage
            .percpu_usage
            .as_ref()
            .map(|usage| usage.len() as u64)
            .unwrap_or(0)
    });

    calculate_cpu_percent_from_deltas(cpu_delta, system_delta, online_cpus)
}

fn calculate_cpu_percent_from_deltas(
    cpu_delta: u64,
    system_delta: u64,
    online_cpus: u64,
) -> Option<f64> {
    if system_delta == 0 || online_cpus == 0 {
        return None;
    }

    Some((cpu_delta as f64 / system_delta as f64) * online_cpus as f64 * 100.0)
}

fn adjusted_memory_usage(usage: u64, stats: Option<MemoryStatsStats>) -> u64 {
    usage.saturating_sub(memory_cache(stats))
}

fn memory_cache(stats: Option<MemoryStatsStats>) -> u64 {
    match stats {
        Some(MemoryStatsStats::V1(stats)) => stats.total_inactive_file,
        Some(MemoryStatsStats::V2(stats)) => stats.inactive_file,
        None => 0,
    }
}

fn network_totals(
    network: Option<NetworkStats>,
    networks: Option<&HashMap<String, NetworkStats>>,
) -> (u64, u64) {
    if let Some(networks) = networks {
        return networks
            .values()
            .fold((0, 0), |(rx_total, tx_total), stats| {
                (
                    rx_total.saturating_add(stats.rx_bytes),
                    tx_total.saturating_add(stats.tx_bytes),
                )
            });
    }

    network
        .map(|stats| (stats.rx_bytes, stats.tx_bytes))
        .unwrap_or((0, 0))
}

fn block_io_totals(entries: Option<&[BlkioStatsEntry]>) -> (u64, u64) {
    let mut read = 0_u64;
    let mut write = 0_u64;

    for entry in entries.unwrap_or_default() {
        match entry.op.to_ascii_lowercase().as_str() {
            "read" => read = read.saturating_add(entry.value),
            "write" => write = write.saturating_add(entry.value),
            _ => {}
        }
    }

    (read, write)
}

fn percent(usage: u64, limit: u64) -> Option<f64> {
    if limit == 0 {
        return None;
    }

    Some((usage as f64 / limit as f64) * 100.0)
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
            metrics: None,
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
        metrics: summarize_metrics(containers),
    }
}

fn summarize_metrics(containers: &[ContainerStatus]) -> Option<ProjectMetrics> {
    let mut seen_metrics = false;
    let mut cpu_seen = false;
    let mut cpu_percent = 0.0;
    let mut memory_usage_seen = false;
    let mut memory_usage_bytes = 0_u64;
    let mut memory_limit_complete = true;
    let mut memory_limit_bytes = 0_u64;
    let mut network_rx_bytes = 0_u64;
    let mut network_tx_bytes = 0_u64;
    let mut block_read_bytes = 0_u64;
    let mut block_write_bytes = 0_u64;
    let mut pids_complete = true;
    let mut pids = 0_u64;

    for metrics in containers
        .iter()
        .filter_map(|container| container.metrics.as_ref())
    {
        seen_metrics = true;

        if let Some(cpu) = metrics.cpu_percent {
            cpu_seen = true;
            cpu_percent += cpu;
        }

        if let Some(usage) = metrics.memory_usage_bytes {
            memory_usage_seen = true;
            memory_usage_bytes = memory_usage_bytes.saturating_add(usage);
        }

        if let Some(limit) = metrics.memory_limit_bytes {
            memory_limit_bytes = memory_limit_bytes.saturating_add(limit);
        } else {
            memory_limit_complete = false;
        }

        network_rx_bytes = network_rx_bytes.saturating_add(metrics.network_rx_bytes);
        network_tx_bytes = network_tx_bytes.saturating_add(metrics.network_tx_bytes);
        block_read_bytes = block_read_bytes.saturating_add(metrics.block_read_bytes);
        block_write_bytes = block_write_bytes.saturating_add(metrics.block_write_bytes);

        if let Some(current_pids) = metrics.pids {
            pids = pids.saturating_add(current_pids);
        } else {
            pids_complete = false;
        }
    }

    if !seen_metrics {
        return None;
    }

    let memory_usage = memory_usage_seen.then_some(memory_usage_bytes);
    let memory_limit = memory_limit_complete.then_some(memory_limit_bytes);
    let memory_percent = memory_usage
        .zip(memory_limit)
        .and_then(|(usage, limit)| percent(usage, limit));

    Some(ProjectMetrics {
        cpu_percent: cpu_seen.then_some(cpu_percent),
        memory_usage_bytes: memory_usage,
        memory_limit_bytes: memory_limit,
        memory_percent,
        network_rx_bytes,
        network_tx_bytes,
        block_read_bytes,
        block_write_bytes,
        pids: pids_complete.then_some(pids),
    })
}

pub fn format_cpu(cpu_percent: Option<f64>) -> String {
    cpu_percent
        .map(|percent| format!("{percent:.1}%"))
        .unwrap_or_else(|| "-".to_string())
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;

    if bytes >= GIB {
        format!("{:.1}GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1}MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1}KiB", bytes / KIB)
    } else {
        format!("{}B", bytes as u64)
    }
}

pub fn format_memory(
    usage_bytes: Option<u64>,
    limit_bytes: Option<u64>,
    memory_percent: Option<f64>,
) -> String {
    let Some(usage) = usage_bytes else {
        return "-".to_string();
    };

    let usage = format_bytes(usage);
    match (limit_bytes, memory_percent) {
        (Some(limit), Some(percent)) => {
            format!("{usage}/{} ({percent:.1}%)", format_bytes(limit))
        }
        (Some(limit), None) => format!("{usage}/{}", format_bytes(limit)),
        _ => usage,
    }
}

pub fn format_io(read_or_rx: u64, write_or_tx: u64) -> String {
    format!("{}/{}", format_bytes(read_or_rx), format_bytes(write_or_tx))
}

pub fn format_pids(pids: Option<u64>) -> String {
    pids.map(|pids| pids.to_string())
        .unwrap_or_else(|| "-".to_string())
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
                metrics: None,
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
                metrics: None,
            },
        ];

        let summary = summarize_project(&containers);
        assert!(matches!(summary.state, ProjectState::Partial));
        assert_eq!(summary.running_containers, 1);
        assert_eq!(summary.total_containers, 2);
        assert_eq!(summary.metrics, None);
    }

    #[test]
    fn calculates_cpu_percent_from_docker_deltas() {
        let percent = calculate_cpu_percent_from_deltas(50, 1_000, 2).unwrap();
        assert_eq!(percent, 10.0);
    }

    #[test]
    fn adjusts_cgroup_v1_memory_usage_by_inactive_file_cache() {
        let usage = adjusted_memory_usage(1_000, Some(MemoryStatsStats::V1(memory_stats_v1(250))));
        assert_eq!(usage, 750);
    }

    #[test]
    fn adjusts_cgroup_v2_memory_usage_by_inactive_file_cache() {
        let usage = adjusted_memory_usage(1_000, Some(MemoryStatsStats::V2(memory_stats_v2(125))));
        assert_eq!(usage, 875);
    }

    #[test]
    fn sums_networks_across_interfaces() {
        let mut networks = HashMap::new();
        networks.insert("a".to_string(), network_stats(10, 20));
        networks.insert("b".to_string(), network_stats(30, 40));

        assert_eq!(network_totals(None, Some(&networks)), (40, 60));
    }

    #[test]
    fn sums_block_read_and_write_bytes() {
        let entries = vec![
            blkio_entry("Read", 10),
            blkio_entry("Write", 20),
            blkio_entry("Read", 30),
            blkio_entry("Sync", 99),
        ];

        assert_eq!(block_io_totals(Some(&entries)), (40, 20));
    }

    #[test]
    fn aggregates_project_metrics() {
        let containers = vec![
            container_with_metrics(ContainerMetrics {
                cpu_percent: Some(10.0),
                memory_usage_bytes: Some(100),
                memory_limit_bytes: Some(1_000),
                memory_percent: Some(10.0),
                network_rx_bytes: 1,
                network_tx_bytes: 2,
                block_read_bytes: 3,
                block_write_bytes: 4,
                pids: Some(5),
            }),
            container_with_metrics(ContainerMetrics {
                cpu_percent: Some(20.0),
                memory_usage_bytes: Some(200),
                memory_limit_bytes: Some(1_000),
                memory_percent: Some(20.0),
                network_rx_bytes: 10,
                network_tx_bytes: 20,
                block_read_bytes: 30,
                block_write_bytes: 40,
                pids: Some(50),
            }),
        ];

        let summary = summarize_project(&containers);
        let metrics = summary.metrics.unwrap();
        assert_eq!(metrics.cpu_percent, Some(30.0));
        assert_eq!(metrics.memory_usage_bytes, Some(300));
        assert_eq!(metrics.memory_limit_bytes, Some(2_000));
        assert_eq!(metrics.memory_percent, Some(15.0));
        assert_eq!(metrics.network_rx_bytes, 11);
        assert_eq!(metrics.network_tx_bytes, 22);
        assert_eq!(metrics.block_read_bytes, 33);
        assert_eq!(metrics.block_write_bytes, 44);
        assert_eq!(metrics.pids, Some(55));
    }

    #[test]
    fn formats_metrics_for_display() {
        assert_eq!(format_cpu(Some(12.34)), "12.3%");
        assert_eq!(format_bytes(1_536), "1.5KiB");
        assert_eq!(
            format_memory(Some(1_048_576), Some(2_097_152), Some(50.0)),
            "1.0MiB/2.0MiB (50.0%)"
        );
        assert_eq!(format_io(1_024, 2_048), "1.0KiB/2.0KiB");
        assert_eq!(format_pids(Some(42)), "42");
    }

    fn container_with_metrics(metrics: ContainerMetrics) -> ContainerStatus {
        ContainerStatus {
            id: "1".to_string(),
            name: "app".to_string(),
            service: None,
            image: None,
            state: "running".to_string(),
            health: None,
            status: None,
            started_at: None,
            ports: vec![],
            metrics: Some(metrics),
        }
    }

    fn network_stats(rx_bytes: u64, tx_bytes: u64) -> NetworkStats {
        NetworkStats {
            rx_dropped: 0,
            rx_bytes,
            rx_errors: 0,
            tx_packets: 0,
            tx_dropped: 0,
            rx_packets: 0,
            tx_errors: 0,
            tx_bytes,
        }
    }

    fn blkio_entry(op: &str, value: u64) -> BlkioStatsEntry {
        BlkioStatsEntry {
            major: 0,
            minor: 0,
            op: op.to_string(),
            value,
        }
    }

    fn memory_stats_v1(total_inactive_file: u64) -> bollard::container::MemoryStatsStatsV1 {
        bollard::container::MemoryStatsStatsV1 {
            cache: 0,
            dirty: 0,
            mapped_file: 0,
            total_inactive_file,
            pgpgout: 0,
            rss: 0,
            total_mapped_file: 0,
            writeback: 0,
            unevictable: 0,
            pgpgin: 0,
            total_unevictable: 0,
            pgmajfault: 0,
            total_rss: 0,
            total_rss_huge: 0,
            total_writeback: 0,
            total_inactive_anon: 0,
            rss_huge: 0,
            hierarchical_memory_limit: 0,
            total_pgfault: 0,
            total_active_file: 0,
            active_anon: 0,
            total_active_anon: 0,
            total_pgpgout: 0,
            total_cache: 0,
            total_dirty: 0,
            inactive_anon: 0,
            active_file: 0,
            pgfault: 0,
            inactive_file: 0,
            total_pgmajfault: 0,
            total_pgpgin: 0,
            hierarchical_memsw_limit: None,
            shmem: None,
            total_shmem: None,
        }
    }

    fn memory_stats_v2(inactive_file: u64) -> bollard::container::MemoryStatsStatsV2 {
        bollard::container::MemoryStatsStatsV2 {
            anon: 0,
            file: 0,
            kernel_stack: 0,
            slab: 0,
            sock: 0,
            shmem: 0,
            file_mapped: 0,
            file_dirty: 0,
            file_writeback: 0,
            anon_thp: 0,
            inactive_anon: 0,
            active_anon: 0,
            inactive_file,
            active_file: 0,
            unevictable: 0,
            slab_reclaimable: 0,
            slab_unreclaimable: 0,
            pgfault: 0,
            pgmajfault: 0,
            workingset_refault: 0,
            workingset_activate: 0,
            workingset_nodereclaim: 0,
            pgrefill: 0,
            pgscan: 0,
            pgsteal: 0,
            pgactivate: 0,
            pgdeactivate: 0,
            pglazyfree: 0,
            pglazyfreed: 0,
            thp_fault_alloc: 0,
            thp_collapse_alloc: 0,
        }
    }
}
