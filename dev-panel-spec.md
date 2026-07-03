# Dev Panel — Project Spec

A terminal-based (TUI) dashboard for managing local development environments: ports, processes, and services. Built in Rust as a learning project, with the practical goal of replacing a wall of terminal tabs in VSCode/iTerm with one control panel.

## Problem Statement

Running a typical dev stack (frontend, backend, Docker containers, maybe a worker process) means juggling multiple terminal tabs just to see if something crashed, what's using a port, or to restart a stuck service. Dev Panel consolidates this into a single interactive view.

## Core Value Proposition

One dashboard replaces five terminals: see every service, its port, its status, and its logs — and control all of it — from one screen.

## Tech Stack

- **Language**: Rust
- **TUI framework**: `ratatui` + `crossterm`
- **Process/system info**: `sysinfo`
- **Signals**: `nix` (for SIGTERM before SIGKILL)
- **Docker (stretch)**: `bollard`
- **Port/proc mapping (Linux)**: parse `/proc/net/tcp` + `/proc/net/tcp6`, cross-reference inode numbers against `/proc/<pid>/fd/*`

## Architecture Overview

```
AppState {
    processes: Vec<ProcessInfo>,     // refreshed each poll
    services: Vec<ServiceConfig>,    // loaded from dev-panel.toml
    selected: usize,
    pending_action: Option<Action>,  // Some(Kill(pid)) while awaiting confirm
    last_message: String,
}
```

Event loop: poll (1–2s interval) → draw → read key event → mutate state directly (navigation) or set `pending_action` (kill/restart) → next frame shows confirm modal → on confirm, execute and clear.

## MVP Scope (v1)

1. **Process/port table** — list of open ports, owning PID, process name, command line. `ratatui::widgets::Table` with `TableState` for row selection.
2. **Kill action** — SIGTERM first, escalate to SIGKILL after timeout (`k` keybinding). Confirmation modal required before executing — irreversible actions need a "press y to confirm" step.
3. **Restart action** (`r` keybinding) — naive restart: capture original `cmd()`/`cwd()` via `sysinfo`, kill, respawn via `std::process::Command`. Known limitation: breaks for processes launched via wrapper scripts, docker-compose, or systemd units with env vars that matter.
4. **Log tailing pane** — tail stdout/stderr of managed processes into a scrollable pane, switchable per service. Identified as the single biggest terminal-count reducer.
5. **Safety guardrails** — blocklist/warning for PID 1, the tool's own parent shell PID, and root-owned processes when not running as root.

## v2 — Named Service Groups

Project-level config file defining the whole stack, so one keypress starts everything instead of `cd`-ing into multiple terminals.

Example `dev-panel.toml`:

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

[[service]]
name = "worker"
cmd = "npm run worker"
cwd = "./worker"
```

Features on top of this:
- **Health checks** — HTTP ping to `/health` or root, colored status indicator (up/down/starting)
- **Port conflict warnings** — flag at startup if a configured port is already owned by something else
- **Docker container awareness** — list containers alongside plain processes via `bollard`, unifying `docker ps` into the same view

## v3 — Quality of Life

- Quick actions: open port in browser (`o`), copy PID/port to clipboard, open project folder in editor
- Per-service resource sparklines (CPU/mem over time) using ratatui's built-in sparkline widget
- Session persistence — remember which services were running across tool restarts
- Crash detection + optional auto-restart with backoff
- Env var diffing — surface which env vars a process actually inherited, to catch stale-env "works on my machine" bugs

## Keybindings (draft)

| Key | Action |
|-----|--------|
| ↑ / ↓ | Navigate table |
| k | Kill selected process (with confirm) |
| r | Restart selected process (with confirm) |
| o | Open port in browser |
| / | Filter/search |
| y | Confirm pending action |
| Esc | Cancel pending action |
| q | Quit |

## Reference Tools (prior art)

- `lsof -i -P -n`, `ss -tulpn`, `netstat -tulpn` — standard Unix port/process inspection
- `bottom` (btm) — htop-style Rust TUI, architecture reference for ratatui usage
- `bandwhich` — per-process network utilization in Rust, closest existing analog
- `procs` — modern Rust `ps` replacement

## Portfolio Framing

Positioned as a secondary/side-interest project rather than a lead piece, given primary job targets are Product Engineer / Senior Frontend-Fullstack roles. Value is as a signal of range, OS-level curiosity, and ability to pick up a new language fast — not a direct proof point for frontend stack depth. README should lead with the *why* (built to solve a real daily annoyance) and can call out the naive-vs-systemd restart tradeoff as evidence of thinking through edge cases.

## Environment Setup

**System-level (one-time):**

1. **Rust toolchain** via rustup (not distro package manager, keeps you on stable and lets you switch toolchains):
   ```
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
   Provides `rustc`, `cargo`, `rustup`.

2. **Build essentials** — linker/C toolchain, needed even for pure-Rust projects since some crates compile C shims:
   ```
   sudo apt install build-essential pkg-config
   ```
   (`base-devel` on Arch, Xcode Command Line Tools on macOS)

3. **Docker CLI** — only needed for container-awareness; dev-panel shells out to `docker ps`/`stop`/`restart` rather than linking `bollard`, so no daemon *library* dependency, just the CLI on `PATH` if you want containers to show up.

**VSCode extensions/components (recommended):**

1. **rust-analyzer** — LSP extension, inline errors/autocomplete/go-to-def
2. **clippy** — `rustup component add clippy`
3. **rustfmt** — `rustup component add rustfmt`

**Not needed as separate installs** — `ratatui`, `crossterm`, `sysinfo`, `serde`, `toml` are just `Cargo.toml` entries; Cargo fetches and compiles them on first build. `nix` is a Unix-only target dependency (Windows uses `taskkill`/no crate). No OpenSSL/native-TLS headers needed — health checks are a raw `TcpStream` connect, not an HTTP client crate.

**Sanity check:**
```
rustc --version && cargo --version
```

## Decisions

- **Cross-platform scope**: all three — Linux, macOS, Windows. Only port discovery
  needed a per-OS backend (`lsof` / `/proc` / `netstat`); everything else in
  `src/platform.rs` is a thin `#[cfg]` branch (signals, clipboard, editor/browser
  opener, systemd detection).
- **Systemd-aware restart**: built now, not deferred to v2/v3. Detects the unit via
  `/proc/<pid>/cgroup` (Linux only) and restarts with `systemctl restart <unit>`
  instead of the naive kill+respawn, so systemd-managed env/cwd/restart-policy
  survives. No equivalent for launchd or Windows services.
- **Mouse support**: skipped, as originally suggested — keybindings only.
- **Docker**: shells out to the `docker` CLI rather than adding `bollard` (would drag
  in `tokio` for an otherwise-sync codebase). See README's Known Limitations.
