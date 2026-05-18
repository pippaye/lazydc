use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::fs;
use std::io::Write;
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

pub fn create_project(compose_dir: &Path, name: &str) -> Result<Project> {
    validate_project_name(name)?;

    fs::create_dir_all(compose_dir)
        .with_context(|| format!("failed to create compose dir {}", compose_dir.display()))?;

    let dir = compose_dir.join(name);
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(anyhow!("project path already exists: {}", dir.display()));
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to create project dir {}", dir.display()));
        }
    }

    let compose_file = dir.join("docker-compose.yaml");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&compose_file)
        .with_context(|| format!("failed to create compose file {}", compose_file.display()))?;
    file.write_all(b"services: {}\n")
        .with_context(|| format!("failed to write compose file {}", compose_file.display()))?;

    Ok(Project {
        name: name.to_string(),
        dir,
        compose_file,
        env_file: None,
    })
}

pub fn delete_project(compose_dir: &Path, project: &Project) -> Result<()> {
    let compose_dir = compose_dir
        .canonicalize()
        .with_context(|| format!("failed to resolve compose dir {}", compose_dir.display()))?;
    let project_dir = project
        .dir
        .canonicalize()
        .with_context(|| format!("failed to resolve project dir {}", project.dir.display()))?;

    if project_dir.parent() != Some(compose_dir.as_path()) {
        bail!(
            "refusing to delete project outside compose dir: {}",
            project.dir.display()
        );
    }

    if project_dir.file_name().and_then(|name| name.to_str()) != Some(project.name.as_str()) {
        bail!(
            "refusing to delete project with mismatched directory name: {}",
            project.dir.display()
        );
    }

    fs::remove_dir_all(&project_dir)
        .with_context(|| format!("failed to delete project dir {}", project_dir.display()))?;
    Ok(())
}

pub fn validate_project_name(name: &str) -> Result<()> {
    let Some(first) = name.chars().next() else {
        bail!("project name cannot be empty");
    };

    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        bail!("project name must start with a lowercase letter or digit");
    }

    if !name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    {
        bail!("project name can contain only lowercase letters, digits, '-' and '_'");
    }

    Ok(())
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

    #[test]
    fn validates_compose_project_names() {
        for name in ["app", "app-1", "app_1", "1app"] {
            validate_project_name(name).unwrap();
        }

        for name in [
            "",
            "App",
            "-app",
            "_app",
            "app.name",
            "app/name",
            "app name",
            "\u{5e94}\u{7528}",
        ] {
            assert!(validate_project_name(name).is_err(), "{name} should fail");
        }
    }

    #[test]
    fn creates_project_with_minimal_compose_file() {
        let dir = tempdir().unwrap();
        let project = create_project(dir.path(), "app").unwrap();

        assert_eq!(project.name, "app");
        assert_eq!(project.dir, dir.path().join("app"));
        assert_eq!(
            project.compose_file,
            dir.path().join("app/docker-compose.yaml")
        );
        assert_eq!(project.env_file, None);
        assert_eq!(
            fs::read_to_string(project.compose_file).unwrap(),
            "services: {}\n"
        );
    }

    #[test]
    fn creates_missing_compose_dir() {
        let dir = tempdir().unwrap();
        let compose_dir = dir.path().join("compose-root");

        create_project(&compose_dir, "app").unwrap();

        assert!(compose_dir.join("app/docker-compose.yaml").exists());
    }

    #[test]
    fn does_not_overwrite_existing_project_path() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("app");
        fs::create_dir_all(&project_dir).unwrap();

        let err = create_project(dir.path(), "app").unwrap_err();

        assert!(err.to_string().contains("already exists"));
        assert!(!project_dir.join("docker-compose.yaml").exists());
    }

    #[test]
    fn deletes_project_directory() {
        let dir = tempdir().unwrap();
        let project = create_project(dir.path(), "app").unwrap();
        fs::write(project.dir.join("data.txt"), "data\n").unwrap();

        delete_project(dir.path(), &project).unwrap();

        assert!(!project.dir.exists());
    }

    #[test]
    fn delete_project_rejects_paths_outside_compose_dir() {
        let compose_dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let project_dir = outside_dir.path().join("app");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("docker-compose.yaml"), "services: {}\n").unwrap();
        let project = Project {
            name: "app".to_string(),
            dir: project_dir.clone(),
            compose_file: project_dir.join("docker-compose.yaml"),
            env_file: None,
        };

        let err = delete_project(compose_dir.path(), &project).unwrap_err();

        assert!(err.to_string().contains("outside compose dir"));
        assert!(project_dir.exists());
    }
}
