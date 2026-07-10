# Dev Panel — Project Spec

A terminal-based (TUI) dashboard for managing local development environments: ports, processes, and services. Built in Rust as a learning project, with the practical goal of replacing a wall of terminal tabs in VSCode/iTerm with one control panel.

## Problem Statement

Running a typical dev stack (frontend, backend, Docker containers, maybe a worker process) means juggling multiple terminal tabs just to see if something crashed, what's using a port, or to restart a stuck service. Dev Panel consolidates this into a single interactive view.

## Core Value Proposition

One dashboard replaces five terminals: see every service, its port, its status, and its logs — and control all of it — from one screen.

## Tech Stack

- **Language**: Rust (edition 2024)
- **TUI framework**: `ratatui` + `crossterm`
- **Process/system info**: `sysinfo`
- **Signals**: `nix` (SIGTERM / SIGKILL on Unix)
- **Config**: `serde` + `toml`
- **Docker**: shells out to `docker` CLI (no `bollard` / `tokio`)
- **Health probes**: raw `std::net::TcpStream` HTTP/1.1 GET (no TLS crate)
- **Port/proc mapping (Linux)**: parse `/proc/net/tcp` + `/proc/net/tcp6`, cross-reference inode numbers against `/proc/<pid>/fd/*`
- **Port/proc mapping (macOS)**: `lsof -F`
- **Port/proc mapping (Windows)**: `netstat -ano`

## Architecture Overview

```
App {
    manager: Manager,           // dev-panel.toml services + lifecycle
    rows: Vec<Row>,             // managed + external + docker, rebuilt each poll
    pending: Option<Pending>,  // confirm modal state
    events: VecDeque<String>,  // persistent action log (m pane)
    pane: Option<Pane>,       // logs | usage | env | details | events
}
```

Event loop: 200ms key/mouse poll → draw → every 2s refresh ports/processes/docker → confirm modal on kill/restart/adopt/stack actions.

## Implemented Features

### Process table

- Columns: PORT, PID, NAME (with `[svc]` / `[ext]` / `[docker]` badge), STATUS, USER, COMMAND, RESTART (strategy label + ↻)
- Restart strategy shown per row: `managed`, `configured`, `systemd`, `launchd`, `docker`, `shell`, `naive`, `unknown`
- Filter with `/`; mouse click ↻ column to restart, elsewhere to select

### Kill / restart

- **Kill** (`k`): SIGTERM → SIGKILL after 3s; confirmation required
- **Restart** (`r`): strategy-aware confirm modal with cmd/cwd/strategy preview
  - Managed services: shell spawn + piped logs
  - Configured port match: kill external → start managed service
  - Linux systemd: `systemctl restart <unit>` via `/proc/<pid>/cgroup`
  - macOS launchd: `launchctl kickstart -k <label>` via `launchctl list`
  - External fallback: shell-wrapped respawn for npm/pnpm/etc., direct exec for node/python
- **Adopt** (`a`): save external process as `[[service]]` in `dev-panel.toml`; also
  available from the restart confirm modal (`r` then `a`)
- **Configured restart**: if an external process's port matches a `dev-panel.toml`
  service, `r` kills the external process and starts the managed service instead

### Managed services (`dev-panel.toml`)

```toml
[[service]]
name = "frontend"
cmd = "npm run dev"
cwd = "./web"
port = 3000
health_url = "http://localhost:3000/health"
auto_restart = true

[[service]]
name = "backend"
cmd = "cargo run"
cwd = "./api"
port = 8080

[stack]
services = ["backend", "frontend"]
```

- **Health**: HTTP GET when `health_url` set (`up (200)`); TCP connect to `port` otherwise (`up (port)`)
- **Stack**: `S` start in order, `R` restart stack, `K` stop in reverse order
- Log tailing (`l`), crash detection + backoff auto-restart, session persistence (`.dev-panel-session`)
- Port conflict warnings when a configured port is occupied externally
- Session persistence via `.dev-panel-session` (resume on relaunch)

### Service config fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Table display name |
| `cmd` | yes | Shell command |
| `cwd` | no | Working directory |
| `port` | no | TCP probe + conflict detection |
| `health_url` | no | HTTP GET probe (`http://` only, no TLS) |
| `auto_restart` | no | Backoff auto-restart on crash (default: false) |

### Bottom panes

| Key | Pane |
|-----|------|
| `l` | Logs (managed services) |
| `u` | CPU sparkline |
| `v` | Env diff (process vs current shell) |
| `d` | Details (cmd, cwd, uptime, children) |
| `m` | Event log (recent actions) |

### Docker

Containers listed alongside processes; stop/restart via `docker` CLI on a background thread.

### Safety guardrails

PID 1, dev-panel's own process tree, and root-owned processes (when not root) cannot be killed.

## Keybindings

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate table |
| k | Kill / stop selected (with confirm) |
| r | Restart selected (with confirm; shows strategy) |
| R | Restart entire stack (with confirm) |
| K | Stop entire stack in reverse order (with confirm) |
| a | Adopt external process into `dev-panel.toml` |
| s | Start selected managed service |
| S | Start stack |
| l / u / v / d / m | Toggle panes |
| o | Open port in browser |
| c / C | Copy port / PID |
| e | Open working directory in editor |
| / | Filter |
| y | Confirm (`a` switches restart confirm → adopt) |
| Esc | Cancel / close pane |
| click ↻ | Restart row |
| q | Quit |

## Known Limitations

- Shell-wrapped / direct-exec restart can fail for wrapper scripts, docker-compose, or env-dependent launches — use **adopt** (`a`) for reliable control
- `https://` health URLs not supported (no TLS)
- No integration with external terminal emulators (iTerm, tmux) — port-discovered processes only
- Windows has no systemd/launchd equivalent

## Decisions

- **Cross-platform**: Linux, macOS, Windows — per-OS port discovery in dedicated backends
- **Supervisor-aware restart**: systemd (Linux) and launchd (macOS) detected and restarted via native tools
- **Mouse**: supported for row select and ↻ column restart click
- **Docker**: CLI shell-out, not `bollard`
- **HTTP health**: minimal HTTP/1.1 over `TcpStream`, not `reqwest`

## Reference Tools (prior art)

- `lsof`, `ss`, `netstat` — port/process inspection
- `bottom` (btm) — ratatui TUI reference
- `procs` — modern Rust `ps` replacement
