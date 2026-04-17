use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

const COMPOSE_FILES: &[&str] = &[
    "docker-compose.yaml",
    "docker-compose.yml",
    "compose.yaml",
    "compose.yml",
];

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub dir: PathBuf,
    pub compose_file: PathBuf,
    pub env_file: Option<PathBuf>,
}

pub fn discover_projects(compose_dir: &Path) -> Result<Vec<Project>> {
    if !compose_dir.exists() {
        return Ok(Vec::new());
    }

    let mut projects = Vec::new();
    for entry in fs::read_dir(compose_dir)
        .with_context(|| format!("failed to read compose dir {}", compose_dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }

        let dir = entry.path();
        let Some(compose_file) = find_compose_file(&dir) else {
            continue;
        };

        let name = entry.file_name().to_string_lossy().to_string();
        let env_file = dir.join(".env");
        projects.push(Project {
            name,
            dir,
            compose_file,
            env_file: env_file.exists().then_some(env_file),
        });
    }

    projects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(projects)
}

fn find_compose_file(dir: &Path) -> Option<PathBuf> {
    COMPOSE_FILES
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_only_directories_with_compose_files() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("app");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("file.txt"), "x").unwrap();

        let projects = discover_projects(dir.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "app");
    }
}
