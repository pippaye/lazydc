# lazydc

`lazydc` is a small CLI + TUI utility for managing a Git-friendly directory of `docker compose` projects in a homelab.

The tool is intentionally narrow:

- read-only state uses the Docker API
- mutating operations use `docker compose` CLI only
- command lines and command output are shown directly so failures are easy to reproduce by hand

## Why

`docker compose` is already the right execution layer for a small self-hosted setup, but once you have multiple services you usually want a few extra things:

- a consistent root directory for all projects
- batch operations across multiple compose projects
- a quick way to inspect status and health
- an interactive view that is still close to the shell commands you already know

`lazydc` aims to provide that without introducing a controller, daemon, or custom deployment model.

## Project Layout

`lazydc` manages a single compose root directory. Each direct child directory that contains a compose file is treated as a project.

Example:

```text
/var/lib/lazydc
├── .env.global
├── immich/
│   ├── .env
│   ├── docker-compose.yaml
│   └── ...
├── jellyfin/
│   ├── .env
│   ├── docker-compose.yaml
│   └── ...
└── prometheus/
    ├── .env
    ├── compose.yaml
    └── ...
```

Supported compose filenames:

- `docker-compose.yaml`
- `docker-compose.yml`
- `compose.yaml`
- `compose.yml`

Notes:

- `.env.global` is optional and lives in the compose root
- project-local `.env` files live inside each project directory
- projects are discovered only one level deep

## Behavior Boundary

`lazydc` deliberately splits responsibilities:

- Read-only operations:
  - project discovery from the filesystem
  - container state, health, resource metrics, port mapping, and start time from Docker API
- Mutating operations:
  - `deploy`
  - `update`
  - `stop`
  - `restart`
  - `remove`
  - `logs`

All mutating operations are executed through `docker compose`. `lazydc` prints the full command before running it and streams stdout/stderr into the CLI or TUI output panel.

## Configuration

Default config path:

```text
~/.config/lazydc/config.toml
```

Current supported keys:

```toml
compose_dir = "/var/lib/lazydc"
docker_bin = "docker"
refresh_interval_ms = 2000
default_log_lines = 100
```

You can print the sample config directly:

```bash
lazydc example-config
```

Precedence:

```text
CLI flags > config file > built-in defaults
```

The most important override is:

```bash
lazydc --compose-dir /srv/compose list
```

`--compose-dir` tells `lazydc` where to look for all managed compose projects.

## CLI

Run without a subcommand to enter the TUI:

```bash
lazydc
```

Available subcommands:

- `tui`
- `example-config`
- `list`
- `status`
- `deploy`
- `update`
- `stop`
- `restart`
- `remove`
- `logs`

Examples:

```bash
lazydc list
lazydc status
lazydc deploy
lazydc status immich jellyfin
lazydc deploy --select
lazydc update immich
lazydc logs jellyfin --tail 200
lazydc remove prometheus --volumes
```

Target selection rules:

- if project names are provided, only those projects are used
- if `--select` is provided, an interactive multi-select prompt is used
- if neither is provided, the command targets all discovered projects

Command semantics:

- `deploy` runs `docker compose up -d`
- `update` runs `docker compose pull` and then `docker compose up -d`
- `remove` runs `docker compose down`
- `remove --volumes` adds `-v`
- `remove --rmi local|all` adds `--rmi`
- `remove --remove-orphans` adds `--remove-orphans`

## TUI

The TUI is a lightweight operations dashboard, not just a launcher.

Layout:

- left: project list
- top right: project details and container state
- bottom right: output panel

The project list shows:

- project name
- project state
- health summary
- running container summary like `3/4 running`
- project CPU and memory metrics when available

The details panel shows:

- project path
- compose file path
- project-level CPU, memory, network I/O, block I/O, and PID metrics
- container names
- image
- state
- health status
- container-level CPU, memory, network I/O, block I/O, and PID metrics
- ports
- start time

The output panel shows:

- the last command line
- live stdout/stderr from compose operations
- scrollable output history

Keyboard shortcuts:

- `j` / `k` or arrow keys: move selection
- `Tab`: switch focus between panels
- `Enter`: focus details
- `Space`: mark or unmark a project
- `/`: filter projects
- `PageUp` / `PageDown`: scroll details or output
- `n`: create a new project
- `u`: deploy
- `U`: update
- `s`: stop
- `r`: restart
- `d`: remove
- `D`: remove and delete volumes
- `x`: delete project files
- `X`: purge project, volumes, and files
- `l`: logs
- `e`: edit the current project's Docker Compose file
- `E`: edit the current project's `.env`
- `Alt+e`: edit `.env.global`
- `c`: open a shell in one of the current project's running containers
- `?`: open help overlay
- `q`: quit

If multiple projects are marked, actions run against the marked set in sequence. Otherwise the action targets the currently selected project. Edit shortcuts `e` and `E`, and the container shell shortcut `c`, always target only the currently highlighted project; `Alt+e` targets the global `.env.global` file.

The container shell selector uses `j`/`k` or arrow keys to choose a running container, `Enter` to open `docker exec -it`, and `Esc`/`q` to cancel.

New project creation is available from the TUI with `n`. It creates `<compose-dir>/<name>/docker-compose.yaml` with a minimal `services: {}` file, then opens the file in `$VISUAL` or `$EDITOR`. Project names follow Docker Compose project name rules: lowercase letters, digits, `-`, and `_`, starting with a lowercase letter or digit.

## Build and Run

### With Nix

Build:

```bash
nix build .#default
```

Run:

```bash
nix run .#default
nix run .#default -- --help
```

The flake exports:

- `packages.default`
- `packages.lazydc`
- `apps.default`
- `apps.lazydc`
- `checks.lazydc`
- `devShells.default`

### Development Shell

```bash
nix develop
```

Inside the shell:

```bash
cargo test
cargo run -- --help
```

## Current Limitations

- project discovery is not recursive
- batch operations are serial, not parallel
- TUI currently exposes the common action set but not every CLI option
- `logs` in the TUI is non-follow mode right now
- no project-specific metadata file exists yet; the directory structure is the source of truth

## Verification

The current repo has been validated with:

- `cargo test`
- `cargo check`
- `nix build .#default`
- `nix run .#default -- --help`
