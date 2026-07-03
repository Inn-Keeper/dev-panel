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
- **Restart** — for ad-hoc processes: naive kill + respawn from the captured command
  line. For services under systemd (Linux only): detected via `/proc/<pid>/cgroup` and
  restarted with `systemctl restart <unit>` instead of a naive respawn, so anything
  systemd itself manages (env, working directory, restart policy) survives.
- **Managed services** (`dev-panel.toml`) — define your whole stack once, start it with
  `S`, get log tailing, TCP health checks, port-conflict warnings at startup, crash
  detection with exponential backoff auto-restart, and session persistence (relaunch
  `dev-panel` and it resumes what was running).
- **Docker awareness** — running containers show up alongside plain processes; stop/
  restart them the same way.
- **Quick actions** — open a port in the browser (`o`), copy PID/port (`c`/`C`), open
  the service's working directory in your editor (`e`).
- **Usage sparklines** (`u`) — per-process CPU history.
- **Env diffing** (`v`) — diff a running process's inherited environment against your
  current shell, to catch stale-env "works on my machine" bugs.
- **Safety guardrails** — PID 1, dev-panel's own process tree, and root-owned processes
  (when not running as root) can't be killed.

## Platform support

Linux, macOS, and Windows. Port discovery is the one genuinely OS-specific piece, so
it's the one place with three separate backends:

| OS | Backend |
|----|---------|
| macOS | shells out to `lsof -F` and parses its field output |
| Linux | parses `/proc/net/tcp{,6}` directly, cross-referencing socket inodes against `/proc/<pid>/fd/*` |
| Windows | shells out to `netstat -ano` |

Kill/restart signals, clipboard, "open in editor," and systemd detection all have their
own platform branch in `src/platform.rs` — one file, `#[cfg]`-gated, rather than a trait
per feature.

## Known limitations

- **Naive restart** breaks for anything launched via a wrapper script, docker-compose,
  or a systemd unit whose behavior depends on env vars dev-panel didn't capture. The
  systemd-aware path avoids this on Linux; there's no equivalent for launchd (macOS) or
  Windows services yet.
- **Docker** shells out to the `docker` CLI rather than the daemon API (`bollard`) — no
  live event stream, just a poll-driven `docker ps` every tick.
- **Health checks** are a bare TCP connect, not an HTTP GET — a process that accepts
  connections but hangs on every request reads as healthy.

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
- **No Docker crate**: shells out to the `docker` CLI (see Known limitations) instead of
  pulling in `bollard` + `tokio` for an otherwise-sync codebase.
- **No HTTP client crate**: health checks are a raw `std::net::TcpStream` connect —
  didn't justify adding `reqwest`/`ureq` for a localhost port probe.

Verify a fresh checkout builds and passes its test suite:

```sh
rustup component add clippy rustfmt   # once per machine
cargo test
cargo clippy --all-targets
cargo build --release
```

## Configuration

Optional `dev-panel.toml` in the directory you launch `dev-panel` from:

```toml
[[service]]
name = "frontend"
cmd = "npm run dev"
cwd = "./web"
port = 3000

[[service]]
name = "backend"
cmd = "cargo run"
cwd = "./api"
port = 8080
auto_restart = true
```

Without it, dev-panel still shows every listening port and Docker container on the
system — just nothing you can `S`tart, since there's nothing configured to start.

## Keybindings

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate table |
| k | Kill / stop selected (with confirm) |
| r | Restart selected (with confirm) |
| s | Start selected managed service |
| S | Start all managed services |
| l | Toggle log pane (managed services only) |
| u | Toggle CPU usage sparkline |
| v | Toggle env-diff pane |
| o | Open port in browser |
| c / C | Copy port / PID to clipboard |
| e | Open working directory in editor |
| / | Filter/search |
| y | Confirm pending action |
| Esc | Cancel pending action / close pane / clear filter |
| PgUp / PgDn | Scroll log pane |
| q | Quit |

## Portfolio framing

A secondary/side-interest project rather than a lead piece — day-job targets are
Product Engineer / Senior Frontend-Fullstack roles, so this isn't a direct proof point
for that stack. It's a signal of range: OS-level curiosity, and picking up a new
language fast against a problem with real edge cases (the naive-vs-systemd restart
tradeoff above is one of them).
