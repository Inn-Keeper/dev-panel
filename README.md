# Dev Panel

A terminal dashboard that replaces a wall of terminal tabs with one screen: every
local dev process, its port, its status, its logs — controllable from one place.

Built as a side project to scratch a real annoyance (juggling frontend/backend/worker
terminals just to check if something crashed) and to pick up Rust by building something
with actual OS surface area: signals, `/proc`, process trees, sockets.

## Why Rust

Not the obvious choice for a TUI (a Node or Go CLI would've shipped faster) — picked
deliberately as a vehicle to learn the language against a problem with real edges:
process lifecycles, signal handling, and platform-specific process/port inspection.

## Features

- **Live table** of listening ports, PIDs, managed services, and Docker containers in
  one view, filterable with `/`.
- **Kill** — SIGTERM, escalates to SIGKILL after 3s if ignored. Confirmation modal
  before anything irreversible runs.
- **Restart** — for ad-hoc processes: shell-wrapped or direct-exec respawn from the
  captured command line. For services under systemd (Linux): detected via
  `/proc/<pid>/cgroup` and restarted with `systemctl restart <unit>`. For launchd
  agents (macOS): detected via `launchctl list` and restarted with
  `launchctl kickstart -k <label>`.
- **Managed services** (`dev-panel.toml`) — define your whole stack once, start it with
  `S`, restart with `R`, stop with `K`. Optional `health_url` for HTTP probes
  (status shows `up (200)` vs `up (port)` for TCP-only). Stack start order via
  `[stack] services = [...]`. Session persistence resumes running services on relaunch.
- **Docker awareness** — running containers show up alongside plain processes; stop/
  restart them the same way.
- **Quick actions** — open a port in the browser (`o`), copy PID/port (`c`/`C`), open
  the service's working directory in your editor (`e`).
- **Usage sparklines** (`u`) — per-process CPU history.
- **Env diffing** (`v`) — diff a running process's inherited environment against your
  current shell, to catch stale-env "works on my machine" bugs.
- **Safety guardrails** — PID 1, dev-panel's own process tree, and root-owned processes
  (when not running as root) can't be killed.
- **Adopt** (`a`) — one-shot promote an external process into `dev-panel.toml` so future
  restarts use the managed-service path (shell spawn, logs, health checks).
- **Restart transparency** — each row shows its restart strategy (`↻ managed`, `↻ launchd`,
  `↻ shell`, etc.); the confirm modal previews cmd, cwd, and strategy before you press `y`.
- **Event log** (`m`) — scrollable history of kills, restarts, adopts, and failures
  (not just the single-line footer).

## Platform support

Linux, macOS, and Windows. Port discovery is the one genuinely OS-specific piece, so
it's the one place with three separate backends:

| OS | Backend |
|----|---------|
| macOS | shells out to `lsof -F` and parses its field output |
| Linux | parses `/proc/net/tcp{,6}` directly, cross-referencing socket inodes against `/proc/<pid>/fd/*` |
| Windows | shells out to `netstat -ano` |

Kill/restart signals, clipboard, "open in editor," systemd/launchd detection all have their
own platform branch in `src/platform.rs` — one file, `#[cfg]`-gated, rather than a trait
per feature. Click the ↻ column to restart a row; click elsewhere in the table to select it.

## Known limitations

- **Naive restart** breaks for anything launched via a wrapper script, docker-compose,
  or a systemd unit whose behavior depends on env vars dev-panel didn't capture. The
  systemd-aware path avoids this on Linux; launchd detection covers macOS agents.
  Use **adopt** (`a`) for external processes you want reliable restarts on.
- **Health checks** use HTTP GET when `health_url` is set, otherwise TCP connect to
  `port` — a process that accepts connections but hangs on every request may still
  read as healthy without `health_url`.
- **Docker** shells out to the `docker` CLI (poll-driven `docker ps`, no live event stream).
- **External terminals** — dev-panel does not attach to iTerm/tmux/VS Code tabs; it
  discovers processes by listening port. Use **adopt** for anything you need to restart reliably.

## Configuration reference

`dev-panel.toml` lives in the directory you launch `dev-panel` from.

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Display name in the table |
| `cmd` | yes | Shell command (run via `sh -c` / `cmd /C`) |
| `cwd` | no | Working directory |
| `port` | no | TCP health probe target; also used for port-conflict detection |
| `health_url` | no | HTTP GET probe, e.g. `http://localhost:3000/health` (no TLS) |
| `auto_restart` | no | Crash recovery with exponential backoff (default: false) |

`[stack].services` lists service names in start order. `S` / `R` / `K` use this order
(stop reverses it). If omitted, all configured services are used in file order.

Example:

```toml
[[service]]
name = "frontend"
cmd = "npm run dev"
cwd = "./web"
port = 3000
health_url = "http://localhost:3000/health"

[[service]]
name = "backend"
cmd = "cargo run"
cwd = "./api"
port = 8080
auto_restart = true

[stack]
services = ["backend", "frontend"]
```

Without a config file, dev-panel still shows every listening port and Docker container
on the system — just nothing you can `S`tart.

### Adopting an external process

For something already running in another terminal tab:

1. Select the `[ext]` row in the table
2. Press `a` (or `r` then `a` from the restart confirm modal)
3. Confirm with `y` — appends a `[[service]]` block to `dev-panel.toml`
4. Press `s` to take over with managed logs, or `r` next time for configured restart

Session persistence: running managed services are saved to `.dev-panel-session` and
resumed on the next launch.

## Tech stack & Rust tooling

- **Toolchain**: stable Rust via `rustup`, edition 2024. `clippy` and `rustfmt`
  components installed alongside the toolchain (`rustup component add clippy rustfmt`).
- **TUI**: [`ratatui`](https://docs.rs/ratatui) + [`crossterm`](https://docs.rs/crossterm)
  for the terminal backend and widgets (`Table`, `Sparkline`, event polling).
- **Process/system info**: [`sysinfo`](https://docs.rs/sysinfo) for cross-platform
  process enumeration, CPU/memory, and env var inspection.
- **Signals**: [`nix`](https://docs.rs/nix) (Unix-only target dependency) for
  `SIGTERM`/`SIGKILL` and root detection; Windows uses `taskkill` instead (no crate —
  it's one CLI call).
- **Config**: [`serde`](https://docs.rs/serde) + [`toml`](https://docs.rs/toml) for
  `dev-panel.toml` parsing.
- **No Docker crate**: shells out to the `docker` CLI instead of
  pulling in `bollard` + `tokio` for an otherwise-sync codebase.
- **No HTTP client crate**: health probes are a raw `TcpStream` HTTP/1.1 GET (no TLS).

Verify a fresh checkout builds and passes its test suite:

```sh
rustup component add clippy rustfmt   # once per machine
cargo test
cargo clippy --all-targets
cargo build --release
```

## Keybindings

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate table |
| k | Kill / stop selected (with confirm) |
| r | Restart selected (with confirm; shows strategy) |
| R | Restart entire stack (with confirm) |
| K | Stop entire stack in reverse order (with confirm) |
| a | Adopt external process as managed service in `dev-panel.toml` |
| s | Start selected managed service |
| S | Start stack (configured order, or all services) |
| l | Toggle log pane (managed services only) |
| m | Toggle event log pane |
| d | Toggle details pane (cmd, cwd, uptime, children) |
| u | Toggle CPU usage sparkline |
| v | Toggle env-diff pane |
| o | Open port in browser |
| c / C | Copy port / PID to clipboard |
| e | Open working directory in editor |
| / | Filter/search |
| y | Confirm pending action (`a` also switches restart → adopt) |
| Esc | Cancel pending action / close pane / clear filter |
| click ↻ | Restart row (same as `r`) |
| PgUp / PgDn | Scroll log pane |
| q | Quit |
