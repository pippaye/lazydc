use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_COMPOSE_DIR: &str = "/var/lib/lazydc";
const DEFAULT_DOCKER_BIN: &str = "docker";
const DEFAULT_REFRESH_INTERVAL_MS: u64 = 2_000;
const DEFAULT_LOG_LINES: usize = 100;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub compose_dir: PathBuf,
    pub docker_bin: PathBuf,
    pub refresh_interval_ms: u64,
    pub default_log_lines: usize,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    compose_dir: Option<PathBuf>,
    docker_bin: Option<PathBuf>,
    refresh_interval_ms: Option<u64>,
    default_log_lines: Option<usize>,
}

impl AppConfig {
    pub fn load(config_path: Option<&Path>, compose_dir_override: Option<&Path>) -> Result<Self> {
        let config_path = config_path.map(PathBuf::from).or_else(default_config_path);
        let file_config = load_file_config(config_path.as_deref())?;

        Ok(Self {
            compose_dir: compose_dir_override
                .map(PathBuf::from)
                .or(file_config.compose_dir)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_COMPOSE_DIR)),
            docker_bin: file_config
                .docker_bin
                .unwrap_or_else(|| PathBuf::from(DEFAULT_DOCKER_BIN)),
            refresh_interval_ms: file_config
                .refresh_interval_ms
                .unwrap_or(DEFAULT_REFRESH_INTERVAL_MS),
            default_log_lines: file_config.default_log_lines.unwrap_or(DEFAULT_LOG_LINES),
        })
    }

    pub fn env_file(&self) -> PathBuf {
        self.compose_dir.join(".env.global")
    }
}

pub fn example_config() -> String {
    format!(
        concat!(
            "compose_dir = \"{compose_dir}\"\n",
            "docker_bin = \"{docker_bin}\"\n",
            "refresh_interval_ms = {refresh_interval_ms}\n",
            "default_log_lines = {default_log_lines}\n"
        ),
        compose_dir = DEFAULT_COMPOSE_DIR,
        docker_bin = DEFAULT_DOCKER_BIN,
        refresh_interval_ms = DEFAULT_REFRESH_INTERVAL_MS,
        default_log_lines = DEFAULT_LOG_LINES,
    )
}

fn load_file_config(path: Option<&Path>) -> Result<FileConfig> {
    let Some(path) = path else {
        return Ok(FileConfig::default());
    };

    if !path.exists() {
        return Ok(FileConfig::default());
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: FileConfig = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(config)
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("lazydc").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_are_used_when_config_is_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing-config.toml");
        let config = AppConfig::load(Some(&missing), None).unwrap();
        assert_eq!(config.compose_dir, PathBuf::from(DEFAULT_COMPOSE_DIR));
        assert_eq!(config.docker_bin, PathBuf::from(DEFAULT_DOCKER_BIN));
    }

    #[test]
    fn cli_override_wins() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
compose_dir = "/tmp/from-file"
docker_bin = "/bin/docker"
"#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&config_path), Some(Path::new("/tmp/override"))).unwrap();
        assert_eq!(config.compose_dir, PathBuf::from("/tmp/override"));
        assert_eq!(config.docker_bin, PathBuf::from("/bin/docker"));
    }

    #[test]
    fn example_config_uses_default_values() {
        let example = example_config();
        assert!(example.contains("compose_dir = \"/var/lib/lazydc\""));
        assert!(example.contains("docker_bin = \"docker\""));
        assert!(example.contains("refresh_interval_ms = 2000"));
        assert!(example.contains("default_log_lines = 100"));
    }
}
