//! App state and behavior; drawing lives in ui.rs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;
use sysinfo::{ProcessesToUpdate, System, Users};

use crate::ui::RESTART_COL_WIDTH;
use crate::{docker, platform, ports, services};

pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
const KILL_ESCALATE_AFTER: Duration = Duration::from_secs(3);
const RESPAWN_GIVE_UP: Duration = Duration::from_secs(15);
const CPU_HISTORY: usize = 60;
const EVENT_CAP: usize = 200;
/// Table border (1) + header row (1) above the first data row.
const TABLE_HEADER_ROWS: u16 = 2;

#[derive(Clone, Copy, PartialEq)]
pub enum Pane {
    Logs,
    Usage,
    EnvDiff,
    Details,
    Events,
}

/// How a row will be restarted — shown in the table and confirm modal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    Managed,
    Configured,
    Systemd,
    Launchd,
    Docker,
    Shell,
    Naive,
    Unknown,
}

impl RestartStrategy {
    pub fn label(self) -> &'static str {
        match self {
            RestartStrategy::Managed => "managed",
            RestartStrategy::Configured => "configured",
            RestartStrategy::Systemd => "systemd",
            RestartStrategy::Launchd => "launchd",
            RestartStrategy::Docker => "docker",
            RestartStrategy::Shell => "shell",
            RestartStrategy::Naive => "naive",
            RestartStrategy::Unknown => "unknown",
        }
    }
}

#[derive(Clone)]
pub enum RowKind {
    Service(usize),
    Process,
    Container { id: String },
}

#[derive(Clone)]
pub struct Row {
    pub kind: RowKind,
    pub port: Option<u16>,
    pub pid: Option<i32>,
    pub name: String,
    pub status: String,
    pub user: String,
    pub cmd: String,
    /// Display name with kind badge, e.g. "[ext] node".
    pub display_name: String,
    /// Short restart strategy label for the ↻ column.
    pub restart_label: String,
}

pub enum Pending {
    Kill {
        pid: i32,
        name: String,
    },
    Restart {
        pid: i32,
        name: String,
        cmd: Vec<String>,
        cwd: Option<PathBuf>,
        /// systemd unit (Linux only) — restart via systemctl instead.
        unit: Option<String>,
        /// launchd label (macOS only) — restart via launchctl kickstart.
        launchd: Option<String>,
    },
    ServiceStop(usize),
    ServiceRestart(usize),
    ContainerStop {
        id: String,
        name: String,
    },
    ContainerRestart {
        id: String,
        name: String,
    },
    /// Kill an external process, then start the matching configured service.
    ServiceTakeover {
        service_i: usize,
        pid: i32,
        name: String,
    },
    /// Save an external process as a managed service in dev-panel.toml.
    Adopt {
        cfg: services::ServiceConfig,
    },
    StackRestart,
    StackStop,
}

struct Respawn {
    pid: i32,
    cmd: Vec<String>,
    cwd: Option<PathBuf>,
    /// Run through the platform shell (sh -c) instead of direct exec.
    via_shell: bool,
    since: Instant,
}

struct Takeover {
    service_i: usize,
    pid: i32,
    since: Instant,
}

/// A just-spawned respawn candidate, watched for a couple ticks to catch the
/// instant-exit failure mode (missing shell/wrapper env) without blocking.
struct RespawnWatch {
    child: std::process::Child,
    cmd: Vec<String>,
    since: Instant,
}

const RESPAWN_CRASH_WINDOW: Duration = Duration::from_millis(500);

pub struct App {
    system: System,
    users: Users,
    pub manager: services::Manager,
    containers: Vec<docker::Container>,
    docker_ok: bool,
    external: Vec<ports::PortEntry>,
    pub rows: Vec<Row>,
    pub table: TableState,
    /// Screen area the table was last drawn to; used to hit-test mouse clicks.
    pub table_area: Rect,
    pub pane: Option<Pane>,
    pub log_scroll: usize,
    pub filter: String,
    pub filter_editing: bool,
    pub pending: Option<Pending>,
    term_sent: Vec<(i32, Instant)>,
    respawns: Vec<Respawn>,
    takeovers: Vec<Takeover>,
    respawn_watches: Vec<RespawnWatch>,
    /// docker stop/restart run on a background thread — the CLI call can
    /// take up to the stop timeout, and this is an interactive TUI.
    docker_ops: Vec<Receiver<String>>,
    pub cpu_hist: HashMap<i32, VecDeque<u64>>,
    pub events: VecDeque<String>,
    pub message: String,
    warned_conflicts: bool,
}

impl App {
    pub fn new(manager: services::Manager) -> Self {
        Self {
            system: System::new(),
            users: Users::new_with_refreshed_list(),
            manager,
            containers: Vec::new(),
            docker_ok: true,
            external: Vec::new(),
            rows: Vec::new(),
            table: TableState::default(),
            table_area: Rect::default(),
            pane: None,
            log_scroll: 0,
            filter: String::new(),
            filter_editing: false,
            pending: None,
            term_sent: Vec::new(),
            respawns: Vec::new(),
            takeovers: Vec::new(),
            respawn_watches: Vec::new(),
            docker_ops: Vec::new(),
            cpu_hist: HashMap::new(),
            events: VecDeque::new(),
            message: "ready".into(),
            warned_conflicts: false,
        }
    }

    pub fn poll(&mut self) {
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.manager.tick();
        match ports::listening_ports() {
            Ok(entries) => {
                self.external = entries;
                self.enrich_external();
            }
            Err(e) => self.message = format!("port scan failed: {e}"),
        }
        if self.docker_ok {
            match docker::list() {
                Ok(c) => self.containers = c,
                Err(e) => {
                    // stop retrying, but say why the container view is empty
                    self.docker_ok = false;
                    self.containers.clear();
                    self.message = format!("docker unavailable: {e}");
                }
            }
        }
        self.watch_kills();
        self.watch_respawns();
        self.watch_takeovers();
        self.build_rows();
        self.warn_conflicts_once();
    }

    fn enrich_external(&mut self) {
        for e in &mut self.external {
            let Some(p) = self.system.process(sysinfo::Pid::from(e.pid as usize)) else {
                continue;
            };
            if e.name.is_empty() {
                e.name = p.name().to_string_lossy().into_owned();
            }
            if e.user.is_empty() {
                e.user = p
                    .user_id()
                    .and_then(|uid| self.users.get_user_by_id(uid))
                    .map(|u| u.name().to_string())
                    .unwrap_or_default();
            }
            e.cmd = command_line(p, e.pid).join(" ");
        }
    }

    fn build_rows(&mut self) {
        // ponytail: managed pid is the wrapper shell, the listener is its
        // child — port-based dedupe below catches what pid-based misses.
        let managed_pids: HashSet<i32> = self
            .manager
            .services
            .iter()
            .filter_map(|s| s.pid())
            .collect();
        let managed_ports: HashSet<u16> = self
            .manager
            .services
            .iter()
            .filter(|s| s.running())
            .filter_map(|s| s.cfg.port)
            .collect();

        let mut rows = Vec::new();
        for (i, svc) in self.manager.services.iter().enumerate() {
            let occupied = !svc.running()
                && svc
                    .cfg
                    .port
                    .is_some_and(|p| self.external.iter().any(|e| e.port == p));
            let strategy = RestartStrategy::Managed;
            rows.push(Row {
                kind: RowKind::Service(i),
                port: svc.cfg.port,
                pid: svc.pid(),
                name: svc.cfg.name.clone(),
                status: if occupied {
                    "conflict".into()
                } else {
                    svc.status_display()
                },
                user: "-".into(),
                cmd: svc.cfg.cmd.clone(),
                display_name: format!("[svc] {}", svc.cfg.name),
                restart_label: strategy.label().into(),
            });
        }
        for e in &self.external {
            if managed_pids.contains(&e.pid) || managed_ports.contains(&e.port) {
                continue;
            }
            let strategy = self.external_restart_strategy(e.port, e.pid);
            rows.push(Row {
                kind: RowKind::Process,
                port: Some(e.port),
                pid: Some(e.pid),
                name: e.name.clone(),
                status: "listen".into(),
                user: e.user.clone(),
                cmd: e.cmd.clone(),
                display_name: format!("[ext] {}", e.name),
                restart_label: strategy.label().into(),
            });
        }
        for c in &self.containers {
            rows.push(Row {
                kind: RowKind::Container { id: c.id.clone() },
                port: c.host_port(),
                pid: None,
                name: c.name.clone(),
                status: c.status.clone(),
                user: "docker".into(),
                cmd: format!("{}  {}", c.image, c.ports),
                display_name: format!("[docker] {}", c.name),
                restart_label: RestartStrategy::Docker.label().into(),
            });
        }
        if !self.filter.is_empty() {
            let q = self.filter.to_lowercase();
            rows.retain(|r| {
                r.name.to_lowercase().contains(&q)
                    || r.cmd.to_lowercase().contains(&q)
                    || r.port
                        .map(|p| p.to_string())
                        .unwrap_or_default()
                        .contains(&q)
            });
        }
        self.rows = rows;

        let last = self.rows.len().saturating_sub(1);
        match self.table.selected() {
            Some(i) if i > last => self.table.select(Some(last)),
            None if !self.rows.is_empty() => self.table.select(Some(0)),
            _ => {}
        }
        self.track_cpu();
    }

    fn external_restart_strategy(&self, port: u16, pid: i32) -> RestartStrategy {
        if self.manager.find_by_port(port).is_some() {
            return RestartStrategy::Configured;
        }
        if platform::systemd_unit(pid).is_some() {
            return RestartStrategy::Systemd;
        }
        if platform::launchd_label(pid).is_some() {
            return RestartStrategy::Launchd;
        }
        let proc = self.system.process(sysinfo::Pid::from(pid as usize));
        let cmd = proc
            .map(|p| command_line(p, pid))
            .unwrap_or_default();
        if cmd.is_empty() {
            RestartStrategy::Unknown
        } else if should_respawn_via_shell(&cmd) {
            RestartStrategy::Shell
        } else {
            RestartStrategy::Naive
        }
    }

    pub fn push_event(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.message = msg.clone();
        if self.events.len() >= EVENT_CAP {
            self.events.pop_front();
        }
        self.events.push_back(msg);
    }

    fn watch_takeovers(&mut self) {
        let mut keep = Vec::new();
        for t in std::mem::take(&mut self.takeovers) {
            if self.alive(t.pid) {
                if t.since.elapsed() >= RESPAWN_GIVE_UP {
                    self.push_event(format!(
                        "{} won't exit; configured restart abandoned",
                        t.pid
                    ));
                } else {
                    keep.push(t);
                }
                continue;
            }
            let name = self.manager.services[t.service_i].cfg.name.clone();
            match self.manager.start(t.service_i) {
                Ok(()) => self.push_event(format!("started configured service {name}")),
                Err(e) => self.push_event(format!("start {name} failed: {e}")),
            }
        }
        self.takeovers = keep;
    }

    fn track_cpu(&mut self) {
        let pids: HashSet<i32> = self.rows.iter().filter_map(|r| r.pid).collect();
        self.cpu_hist.retain(|pid, _| pids.contains(pid));
        for pid in pids {
            let cpu = self
                .system
                .process(sysinfo::Pid::from(pid as usize))
                .map(|p| p.cpu_usage() as u64)
                .unwrap_or(0);
            let hist = self.cpu_hist.entry(pid).or_default();
            if hist.len() >= CPU_HISTORY {
                hist.pop_front();
            }
            hist.push_back(cpu);
        }
    }

    /// v2 port-conflict warning, once at startup.
    fn warn_conflicts_once(&mut self) {
        if self.warned_conflicts {
            return;
        }
        self.warned_conflicts = true;
        let conflicts: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.status == "conflict")
            .map(|r| format!("{} (port {})", r.name, r.port.unwrap_or(0)))
            .collect();
        if !conflicts.is_empty() {
            self.push_event(format!("port conflict: {}", conflicts.join(", ")));
        }
    }

    fn watch_kills(&mut self) {
        let mut keep = Vec::new();
        for (pid, when) in std::mem::take(&mut self.term_sent) {
            if !self.alive(pid) {
                self.push_event(format!("{pid} exited after SIGTERM"));
            } else if when.elapsed() >= KILL_ESCALATE_AFTER {
                match platform::force_kill(pid) {
                    Ok(()) => self.push_event(format!("SIGKILL → {pid} (ignored SIGTERM)")),
                    Err(e) => self.push_event(format!("SIGKILL {pid} failed: {e}")),
                }
            } else {
                keep.push((pid, when));
            }
        }
        self.term_sent = keep;
    }

    /// Naive-restart second half: once the old pid is gone, respawn its
    /// captured command line.
    fn watch_respawns(&mut self) {
        let mut keep = Vec::new();
        for r in std::mem::take(&mut self.respawns) {
            if self.alive(r.pid) {
                if r.since.elapsed() >= RESPAWN_GIVE_UP {
                    self.push_event(format!("{} won't exit; respawn abandoned", r.pid));
                } else {
                    keep.push(r);
                }
                continue;
            }
            let mut child_cmd = if r.via_shell {
                let cmd_str = r.cmd.join(" ");
                platform::shell(&cmd_str, r.cwd.as_deref())
            } else {
                let mut cmd = std::process::Command::new(&r.cmd[0]);
                cmd.args(&r.cmd[1..]);
                if let Some(d) = &r.cwd {
                    cmd.current_dir(d);
                }
                cmd
            };
            child_cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped());
            match child_cmd.spawn() {
                Ok(child) => {
                    self.push_event(format!("respawned as {}", child.id()));
                    self.respawn_watches.push(RespawnWatch {
                        child,
                        cmd: r.cmd,
                        since: Instant::now(),
                    });
                }
                Err(e) => self.push_event(format!("respawn failed: {e}")),
            }
        }
        self.respawns = keep;
    }

    /// Non-blocking follow-up to a respawn: catches the instant-exit failure
    /// mode (missing shell/wrapper env — pnpm, nvm shims, etc.) a tick or two
    /// later instead of freezing the poll loop to wait for it.
    pub fn watch_respawn_crashes(&mut self) {
        let mut keep = Vec::new();
        for mut w in std::mem::take(&mut self.respawn_watches) {
            match w.child.try_wait() {
                Ok(Some(status)) => {
                    let stderr = w
                        .child
                        .stderr
                        .take()
                        .map(|mut s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            let _ = s.read_to_string(&mut buf);
                            buf.trim().to_string()
                        })
                        .unwrap_or_default();
                    self.push_event(format!(
                        "respawn of {} exited immediately ({status}): {}. Adopt with `a` or define in {} for reliable restarts.",
                        w.cmd.join(" "),
                        if stderr.is_empty() {
                            "no output".into()
                        } else {
                            stderr
                        },
                        services::CONFIG_FILE
                    ));
                }
                Ok(None) if w.since.elapsed() < RESPAWN_CRASH_WINDOW => keep.push(w),
                _ => {} // survived the crash window, or wait() failed: stop watching
            }
        }
        self.respawn_watches = keep;
    }

    /// Runs a docker CLI call (stop/restart) on a background thread — it can
    /// block for up to the stop timeout, and this is an interactive TUI.
    fn spawn_docker_op(&mut self, op: impl FnOnce() -> String + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(op());
        });
        self.docker_ops.push(rx);
    }

    /// Non-blocking: surfaces a docker op's result message as soon as its
    /// background thread finishes.
    pub fn watch_docker_ops(&mut self) {
        let mut keep = Vec::new();
        for rx in std::mem::take(&mut self.docker_ops) {
            match rx.try_recv() {
                Ok(msg) => self.push_event(msg),
                Err(std::sync::mpsc::TryRecvError::Empty) => keep.push(rx),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }
        self.docker_ops = keep;
    }

    fn alive(&self, pid: i32) -> bool {
        self.system
            .process(sysinfo::Pid::from(pid as usize))
            .is_some()
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.table.selected().and_then(|i| self.rows.get(i))
    }

    /// Guardrails: PID 1, our own process tree, root-owned when we aren't root.
    fn kill_guard(&self, row: &Row) -> Option<String> {
        let pid = row.pid?;
        let own = std::process::id() as i32;
        let parent = self
            .system
            .process(sysinfo::Pid::from_u32(std::process::id()))
            .and_then(|p| p.parent())
            .map(|p| p.as_u32() as i32)
            .unwrap_or(-1);
        if pid == 1 {
            return Some("refusing to touch PID 1".into());
        }
        if pid == own || pid == parent {
            return Some("that's dev-panel's own process tree".into());
        }
        if row.user == "root" && !platform::is_root() {
            return Some(format!("{} is root-owned; not running as root", row.name));
        }
        None
    }

    pub fn pending_lines(&self) -> Option<Vec<String>> {
        let pending = self.pending.as_ref()?;
        Some(match pending {
            Pending::Kill { pid, name } => vec![format!("Kill {pid} ({name})?")],
            Pending::Restart {
                pid,
                name,
                unit: Some(u),
                ..
            } => vec![
                format!("Restart {name} ({pid})"),
                format!("  strategy: systemd → systemctl restart {u}"),
            ],
            Pending::Restart {
                pid,
                name,
                launchd: Some(label),
                ..
            } => vec![
                format!("Restart {name} ({pid})"),
                format!("  strategy: launchd → launchctl kickstart -k {label}"),
            ],
            Pending::Restart {
                pid,
                name,
                cmd,
                cwd,
                unit: None,
                launchd: None,
            } => {
                let cmd_line = if cmd.is_empty() {
                    "-".into()
                } else {
                    cmd.join(" ")
                };
                let cwd_line = cwd
                    .as_ref()
                    .map(|c| c.display().to_string())
                    .unwrap_or_else(|| "-".into());
                vec![
                    format!("Restart {name} ({pid})"),
                    if should_respawn_via_shell(cmd) {
                        "  strategy: shell-wrapped respawn (sh -c)".into()
                    } else {
                        "  strategy: direct exec respawn".into()
                    },
                    format!("  cmd: {cmd_line}"),
                    format!("  cwd: {cwd_line}"),
                    "  logs: none (external process)".into(),
                    "  tip: press `a` to adopt as a managed service".into(),
                ]
            }
            Pending::ServiceStop(i) => {
                vec![format!(
                    "Stop service {}?",
                    self.manager.services[*i].cfg.name
                )]
            }
            Pending::ServiceRestart(i) => {
                let name = self.manager.services[*i].cfg.name.clone();
                let cmd = self.manager.services[*i].cfg.cmd.clone();
                vec![
                    format!("Restart service {name}?"),
                    "  strategy: managed (shell spawn + logs)".into(),
                    format!("  cmd: {cmd}"),
                ]
            }
            Pending::ServiceTakeover {
                service_i,
                pid,
                name,
            } => {
                let svc = &self.manager.services[*service_i];
                vec![
                    format!("Restart {name} ({pid}) via configured service"),
                    "  strategy: kill external → start managed".into(),
                    format!("  service: {}", svc.cfg.name),
                    format!("  cmd: {}", svc.cfg.cmd),
                ]
            }
            Pending::ContainerStop { name, .. } => vec![format!("Stop container {name}?")],
            Pending::ContainerRestart { name, .. } => vec![
                format!("Restart container {name}?"),
                "  strategy: docker restart".into(),
            ],
            Pending::Adopt { cfg } => {
                let mut lines = vec![
                    format!("Adopt as managed service {:?}?", cfg.name),
                    format!("  cmd: {}", cfg.cmd),
                ];
                if let Some(cwd) = &cfg.cwd {
                    lines.push(format!("  cwd: {cwd}"));
                }
                if let Some(port) = cfg.port {
                    lines.push(format!("  port: {port}"));
                }
                lines.push(format!("  saves to {}", services::CONFIG_FILE));
                lines
            }
            Pending::StackRestart => {
                let names = self.manager.stack_names().join(", ");
                vec![
                    "Restart entire stack?".into(),
                    format!("  order: {names}"),
                    "  strategy: managed (shell spawn + logs)".into(),
                ]
            }
            Pending::StackStop => {
                let names = self.manager.stack_names().join(", ");
                vec![
                    "Stop entire stack?".into(),
                    format!("  order (reverse): {names}"),
                ]
            }
        })
    }

    // Key dispatch: one arm per binding, so it runs long but stays flat.
    pub fn on_key(&mut self, code: KeyCode) -> bool {
        if self.filter_editing {
            match code {
                KeyCode::Enter => self.filter_editing = false,
                KeyCode::Esc => {
                    self.filter_editing = false;
                    self.filter.clear();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.build_rows();
            return true;
        }
        match code {
            KeyCode::Char('q') => return false,
            KeyCode::Up => self.table.select_previous(),
            KeyCode::Down => self.table.select_next(),
            KeyCode::Char('/') => self.filter_editing = true,
            KeyCode::Char('l') => self.toggle_pane(Pane::Logs),
            KeyCode::Char('u') => self.toggle_pane(Pane::Usage),
            KeyCode::Char('v') => self.toggle_pane(Pane::EnvDiff),
            KeyCode::Char('d') => self.toggle_pane(Pane::Details),
            KeyCode::Char('m') => self.toggle_pane(Pane::Events),
            KeyCode::PageUp => self.log_scroll += 5,
            KeyCode::PageDown => self.log_scroll = self.log_scroll.saturating_sub(5),
            KeyCode::Char('k') => self.request_kill(),
            KeyCode::Char('r') => self.request_restart(),
            KeyCode::Char('R') => self.request_stack_restart(),
            KeyCode::Char('a') => self.request_adopt(),
            KeyCode::Char('s') => self.start_selected(),
            KeyCode::Char('S') => self.start_all(),
            KeyCode::Char('K') => self.request_stack_stop(),
            KeyCode::Char('y') => self.confirm_pending(),
            KeyCode::Esc => {
                if self.pending.take().is_some() {
                    self.push_event("cancelled");
                } else if self.pane.take().is_none() {
                    self.filter.clear();
                    self.build_rows();
                }
            }
            KeyCode::Char('o') => self.open_browser(),
            KeyCode::Char('c') => self.copy(false),
            KeyCode::Char('C') => self.copy(true),
            KeyCode::Char('e') => self.open_editor(),
            _ => {}
        }
        true
    }

    /// Left-click on the ↻ column restarts that row; anywhere else in the
    /// table just selects the row, mirroring arrow-key navigation.
    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if ev.kind != MouseEventKind::Down(crossterm::event::MouseButton::Left) {
            return;
        }
        let area = self.table_area;
        if ev.column < area.x
            || ev.column >= area.x + area.width
            || ev.row < area.y + TABLE_HEADER_ROWS
            || ev.row >= area.y + area.height.saturating_sub(1)
        {
            return;
        }
        let row_index = self.table.offset() + (ev.row - area.y - TABLE_HEADER_ROWS) as usize;
        if row_index >= self.rows.len() {
            return;
        }
        self.table.select(Some(row_index));
        let restart_col_start = area.x + area.width.saturating_sub(RESTART_COL_WIDTH + 1);
        if ev.column >= restart_col_start {
            self.request_restart();
        }
    }

    fn toggle_pane(&mut self, pane: Pane) {
        self.log_scroll = 0;
        self.pane = if self.pane == Some(pane) {
            None
        } else {
            Some(pane)
        };
    }

    fn request_kill(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        match &row.kind {
            RowKind::Service(i) => {
                if self.manager.services[*i].running() {
                    self.pending = Some(Pending::ServiceStop(*i));
                } else {
                    self.message = "service is not running".into();
                }
            }
            RowKind::Container { id } => {
                self.pending = Some(Pending::ContainerStop {
                    id: id.clone(),
                    name: row.name.clone(),
                });
            }
            RowKind::Process => match self.kill_guard(&row) {
                Some(reason) => self.message = reason,
                None => {
                    self.pending = Some(Pending::Kill {
                        pid: row.pid.expect("guard checked pid"),
                        name: row.name.clone(),
                    });
                }
            },
        }
    }

    fn request_restart(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        match &row.kind {
            RowKind::Service(i) => self.pending = Some(Pending::ServiceRestart(*i)),
            RowKind::Container { id } => {
                self.pending = Some(Pending::ContainerRestart {
                    id: id.clone(),
                    name: row.name.clone(),
                });
            }
            RowKind::Process => {
                if let Some(reason) = self.kill_guard(&row) {
                    self.push_event(reason);
                    return;
                }
                let pid = row.pid.expect("guard checked pid");
                let port = row.port.expect("external process has port");

                // Port matches a configured service — kill external, start managed.
                if let Some(i) = self.manager.find_by_port(port) {
                    if self.manager.services[i].running() {
                        self.pending = Some(Pending::ServiceRestart(i));
                    } else {
                        self.pending = Some(Pending::ServiceTakeover {
                            service_i: i,
                            pid,
                            name: row.name.clone(),
                        });
                    }
                    return;
                }

                let proc = self.system.process(sysinfo::Pid::from(pid as usize));
                let cmd = proc.map(|p| command_line(p, pid)).unwrap_or_default();
                let cwd = proc
                    .and_then(|p| p.cwd())
                    .map(PathBuf::from)
                    .or_else(|| platform::lsof_cwd(pid));
                let unit = platform::systemd_unit(pid);
                let launchd = platform::launchd_label(pid);
                if unit.is_none() && launchd.is_none() && cmd.is_empty() {
                    self.push_event("no command line captured; can't restart — try `a` to adopt");
                    return;
                }
                self.pending = Some(Pending::Restart {
                    pid,
                    name: row.name.clone(),
                    cmd,
                    cwd,
                    unit,
                    launchd,
                });
            }
        }
    }

    fn request_adopt(&mut self) {
        if let Some(Pending::Restart {
            name,
            cmd,
            cwd,
            unit: None,
            launchd: None,
            ..
        }) = self.pending.take()
        {
            let port = self.selected_row().and_then(|r| r.port);
            self.pending = Some(Pending::Adopt {
                cfg: adopt_config_from_process(&name, &cmd.join(" "), cwd.as_deref(), port),
            });
            return;
        }

        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        let RowKind::Process = row.kind else {
            self.push_event("adopt works on external processes only");
            return;
        };
        let pid = row.pid.expect("external has pid");
        let proc = self.system.process(sysinfo::Pid::from(pid as usize));
        let cmd = proc.map(|p| command_line(p, pid)).unwrap_or_default();
        let cwd = proc
            .and_then(|p| p.cwd())
            .map(PathBuf::from)
            .or_else(|| platform::lsof_cwd(pid));
        if cmd.is_empty() && row.cmd.is_empty() {
            self.push_event("no command line captured; can't adopt");
            return;
        }
        let cmd_str = if row.cmd.is_empty() {
            cmd.join(" ")
        } else {
            row.cmd.clone()
        };
        self.pending = Some(Pending::Adopt {
            cfg: adopt_config_from_process(&row.name, &cmd_str, cwd.as_deref(), row.port),
        });
    }

    fn confirm_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending {
            Pending::Kill { pid, name } => match platform::terminate(pid) {
                Ok(()) => {
                    self.push_event(format!("SIGTERM → {pid} ({name})"));
                    self.term_sent.push((pid, Instant::now()));
                }
                Err(e) => self.push_event(format!("SIGTERM {pid} failed: {e}")),
            },
            Pending::Restart {
                pid,
                name,
                cmd,
                cwd,
                unit,
                launchd,
            } => {
                if let Some(unit) = unit {
                    self.push_event(match platform::systemctl_restart(&unit) {
                        Ok(()) => format!("systemctl restarted {unit}"),
                        Err(e) => format!("systemctl restart {unit} failed: {e}"),
                    });
                } else if let Some(label) = launchd {
                    self.push_event(match platform::launchctl_restart(&label) {
                        Ok(()) => format!("launchctl restarted {label}"),
                        Err(e) => format!("launchctl restart {label} failed: {e}"),
                    });
                } else {
                    match platform::terminate(pid) {
                        Ok(()) => {
                            self.push_event(format!("restarting {name}: SIGTERM → {pid}"));
                            self.term_sent.push((pid, Instant::now()));
                            let via_shell = should_respawn_via_shell(&cmd);
                            self.respawns.push(Respawn {
                                pid,
                                cmd,
                                cwd,
                                via_shell,
                                since: Instant::now(),
                            });
                        }
                        Err(e) => self.push_event(format!("SIGTERM {pid} failed: {e}")),
                    }
                }
            }
            Pending::ServiceTakeover {
                service_i,
                pid,
                name,
            } => match platform::terminate(pid) {
                Ok(()) => {
                    self.push_event(format!("restarting {name} via configured service"));
                    self.term_sent.push((pid, Instant::now()));
                    self.takeovers.push(Takeover {
                        service_i,
                        pid,
                        since: Instant::now(),
                    });
                }
                Err(e) => self.push_event(format!("SIGTERM {pid} failed: {e}")),
            },
            Pending::Adopt { cfg } => match self.manager.adopt(cfg.clone()) {
                Ok(i) => {
                    self.push_event(format!(
                        "adopted {} into {} — press s to take over with managed logs",
                        cfg.name, services::CONFIG_FILE
                    ));
                    self.build_rows();
                    let _ = i;
                }
                Err(e) => self.push_event(format!("adopt failed: {e}")),
            },
            Pending::ServiceStop(i) => {
                self.manager.stop(i);
                self.push_event(format!("stopping {}", self.manager.services[i].cfg.name));
            }
            Pending::ServiceRestart(i) => {
                let name = self.manager.services[i].cfg.name.clone();
                let msg = match self.manager.restart(i) {
                    Ok(()) => format!("restarting {name}"),
                    Err(e) => format!("restart failed: {e}"),
                };
                self.push_event(msg);
            }
            Pending::ContainerStop { id, name } => {
                self.push_event(format!("stopping container {name}…"));
                self.spawn_docker_op(move || match docker::stop(&id) {
                    Ok(()) => format!("stopped container {name}"),
                    Err(e) => format!("docker stop {name} failed: {e}"),
                });
            }
            Pending::ContainerRestart { id, name } => {
                self.push_event(format!("restarting container {name}…"));
                self.spawn_docker_op(move || match docker::restart(&id) {
                    Ok(()) => format!("restarted container {name}"),
                    Err(e) => format!("docker restart {name} failed: {e}"),
                });
            }
            Pending::StackRestart => match self.manager.restart_stack() {
                Ok(n) => self.push_event(format!("restarting stack ({n} service(s))")),
                Err(e) => self.push_event(format!("stack restart failed: {e}")),
            },
            Pending::StackStop => {
                self.manager.stop_stack();
                self.push_event("stopping stack (reverse order)");
            }
        }
        self.build_rows();
    }

    fn request_stack_restart(&mut self) {
        if self.manager.services.is_empty() {
            self.push_event(format!("no services configured ({})", services::CONFIG_FILE));
            return;
        }
        self.pending = Some(Pending::StackRestart);
    }

    fn request_stack_stop(&mut self) {
        if self.manager.services.is_empty() {
            self.push_event(format!("no services configured ({})", services::CONFIG_FILE));
            return;
        }
        let any_running = self.manager.services.iter().any(|s| s.running());
        if !any_running {
            self.push_event("no managed services running");
            return;
        }
        self.pending = Some(Pending::StackStop);
    }

    fn start_selected(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        let RowKind::Service(i) = row.kind else {
            self.message = "s starts managed services only (see dev-panel.toml)".into();
            return;
        };
        if self.manager.services[i].running() {
            self.message = "already running (k stops it)".into();
            return;
        }
        self.message = match self.manager.start(i) {
            Ok(()) => format!("started {}", row.name),
            Err(e) => format!("start {} failed: {e}", row.name),
        };
        self.build_rows();
    }

    /// v2: one keypress starts the whole stack (in configured order).
    fn start_all(&mut self) {
        if self.manager.services.is_empty() {
            self.push_event(format!("no services configured ({})", services::CONFIG_FILE));
            return;
        }
        match self.manager.start_stack() {
            Ok(started) => self.push_event(format!("started {started} service(s)")),
            Err(e) => self.push_event(format!("stack start failed: {e}")),
        }
        self.build_rows();
    }

    fn open_browser(&mut self) {
        let Some(port) = self.selected_row().and_then(|r| r.port) else {
            self.message = "no port on this row".into();
            return;
        };
        let url = format!("http://localhost:{port}");
        self.message = match platform::open_external(&url) {
            Ok(()) => format!("opened {url}"),
            Err(e) => format!("open failed: {e}"),
        };
    }

    fn copy(&mut self, pid_first: bool) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let text = if pid_first {
            row.pid.map(|p| p.to_string())
        } else {
            row.port
                .map(|p| p.to_string())
                .or(row.pid.map(|p| p.to_string()))
        };
        let Some(text) = text else {
            self.message = "nothing to copy on this row".into();
            return;
        };
        self.message = match platform::copy_clipboard(&text) {
            Ok(()) => format!("copied {text}"),
            Err(e) => format!("clipboard failed: {e}"),
        };
    }

    fn open_editor(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        let dir = match &row.kind {
            RowKind::Service(i) => Some(PathBuf::from(
                self.manager.services[*i]
                    .cfg
                    .cwd
                    .clone()
                    .unwrap_or_else(|| ".".into()),
            )),
            RowKind::Process => row
                .pid
                .and_then(|pid| self.system.process(sysinfo::Pid::from(pid as usize)))
                .and_then(|p| p.cwd())
                .map(PathBuf::from),
            RowKind::Container { .. } => None,
        };
        let Some(dir) = dir else {
            self.message = "no folder known for this row".into();
            return;
        };
        self.message = match platform::open_in_editor(&dir) {
            Ok(()) => format!("opened {}", dir.display()),
            Err(e) => format!("editor failed: {e}"),
        };
    }

    /// CPU history + one-line summary for the usage sparkline.
    pub fn usage_data(&self) -> Option<(Vec<u64>, String)> {
        let row = self.selected_row()?;
        let pid = row.pid?;
        let hist: Vec<u64> = self.cpu_hist.get(&pid)?.iter().copied().collect();
        let title = self
            .system
            .process(sysinfo::Pid::from(pid as usize))
            .map(|p| {
                format!(
                    "{} — cpu {:.0}%  mem {} MB",
                    row.name,
                    p.cpu_usage(),
                    p.memory() / (1024 * 1024)
                )
            })
            .unwrap_or_else(|| row.name.clone());
        Some((hist, title))
    }

    /// Expandable detail block: full command, cwd, uptime, and child PIDs.
    pub fn details_lines(&self) -> Vec<String> {
        let Some(row) = self.selected_row() else {
            return vec!["nothing selected".into()];
        };
        let Some(pid) = row.pid else {
            return vec![format!("{}: no pid (container or unresolved)", row.name)];
        };
        let Some(p) = self.system.process(sysinfo::Pid::from(pid as usize)) else {
            return vec!["process is gone".into()];
        };
        let cmd = command_line(p, pid);
        let uptime = Duration::from_secs(p.run_time());
        let children: Vec<String> = self
            .system
            .processes()
            .values()
            .filter(|c| c.parent() == Some(p.pid()))
            .map(|c| format!("{} ({})", c.pid(), c.name().to_string_lossy()))
            .collect();
        vec![
            format!("name: {}", row.name),
            format!("pid: {pid}"),
            format!(
                "cmd: {}",
                if cmd.is_empty() {
                    "-".into()
                } else {
                    cmd.join(" ")
                }
            ),
            format!(
                "cwd: {}",
                p.cwd()
                    .map(PathBuf::from)
                    .or_else(|| platform::lsof_cwd(pid))
                    .map(|c| c.display().to_string())
                    .unwrap_or_else(|| "-".into())
            ),
            format!("uptime: {}", format_duration(uptime)),
            format!(
                "children: {}",
                if children.is_empty() {
                    "none".into()
                } else {
                    children.join(", ")
                }
            ),
        ]
    }

    /// Scrollable event history for the events pane.
    pub fn event_lines(&self) -> Vec<String> {
        self.events.iter().cloned().collect()
    }

    /// v3 env diffing: what the selected process inherited vs our shell now.
    pub fn env_diff_lines(&self) -> Vec<String> {
        let Some(pid) = self.selected_row().and_then(|r| r.pid) else {
            return vec!["select a row with a pid".into()];
        };
        let Some(p) = self.system.process(sysinfo::Pid::from(pid as usize)) else {
            return vec!["process is gone".into()];
        };
        let proc_env: Vec<(String, String)> = p
            .environ()
            .iter()
            .filter_map(|kv| {
                let kv = kv.to_string_lossy();
                let (k, v) = kv.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect();
        if proc_env.is_empty() {
            return vec!["no env readable for this process".into()];
        }
        let ours: HashMap<String, String> = std::env::vars().collect();
        let diff = env_diff(&proc_env, &ours);
        if diff.is_empty() {
            vec!["env matches current shell".into()]
        } else {
            diff
        }
    }
}

/// sysinfo's `Process::cmd()` can come back empty (observed on recent macOS,
/// where reading another — or even one's own — process's argv needs an
/// entitlement plain `cargo build` binaries don't have). Fall back to `ps`,
/// which is setuid-root and unaffected.
fn command_line(p: &sysinfo::Process, pid: i32) -> Vec<String> {
    let cmd: Vec<String> = p
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    if !cmd.is_empty() {
        return cmd;
    }
    platform::ps_cmdline(pid).unwrap_or_default()
}

fn should_respawn_via_shell(cmd: &[String]) -> bool {
    cmd.first().is_some_and(|exe| {
        matches!(
            exe.as_str(),
            "npm" | "pnpm" | "yarn" | "npx" | "bun" | "make" | "sh" | "bash" | "zsh"
        )
    })
}

fn adopt_config_from_process(
    name: &str,
    cmd: &str,
    cwd: Option<&std::path::Path>,
    port: Option<u16>,
) -> services::ServiceConfig {
    let cwd = cwd.and_then(|c| {
        std::env::current_dir().ok().and_then(|here| {
            c.strip_prefix(&here).ok().map(|p| {
                if p.as_os_str().is_empty() {
                    ".".into()
                } else {
                    p.display().to_string()
                }
            })
        })
        .or_else(|| Some(c.display().to_string()))
    });
    services::ServiceConfig {
        name: suggest_service_name(name, port),
        cmd: cmd.to_string(),
        cwd,
        port,
        auto_restart: false,
        health_url: None,
    }
}

fn suggest_service_name(process_name: &str, port: Option<u16>) -> String {
    let base = process_name.trim();
    if !base.is_empty() && base != "-" {
        base.to_string()
    } else {
        port.map(|p| format!("port-{p}"))
            .unwrap_or_else(|| "service".into())
    }
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Pure diff: `~` differs, `+` only in process, `-` not inherited.
fn env_diff(proc_env: &[(String, String)], ours: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in proc_env {
        match ours.get(k) {
            Some(o) if o != v => out.push(format!("~ {k}: proc={v} shell={o}")),
            None => out.push(format!("+ {k}={v}")),
            _ => {}
        }
    }
    for k in ours.keys() {
        if !proc_env.iter().any(|(pk, _)| pk == k) {
            out.push(format!("- {k} (not inherited)"));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docker stop/restart can block for up to STOP_TIMEOUT_SECS; spawn_docker_op
    /// must hand that off to a background thread instead of blocking the caller
    /// (which is the UI event loop in real use).
    #[test]
    fn spawn_docker_op_does_not_block_caller() {
        let manager = services::Manager::load().expect("no dev-panel.toml in test cwd");
        let mut app = App::new(manager);

        let start = Instant::now();
        app.spawn_docker_op(|| {
            std::thread::sleep(Duration::from_millis(300));
            "slow op done".to_string()
        });
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "spawn_docker_op must return immediately, not wait for the op"
        );

        // Not done yet — watch_docker_ops shouldn't touch the message.
        app.message = "unrelated".into();
        app.watch_docker_ops();
        assert_eq!(app.message, "unrelated");

        std::thread::sleep(Duration::from_millis(400));
        app.watch_docker_ops();
        assert_eq!(app.message, "slow op done");
    }

    #[test]
    fn env_diff_flags_changed_added_missing() {
        let proc_env = vec![
            ("PATH".into(), "/old".into()),
            ("SAME".into(), "x".into()),
            ("EXTRA".into(), "1".into()),
        ];
        let ours = HashMap::from([
            ("PATH".to_string(), "/new".to_string()),
            ("SAME".to_string(), "x".to_string()),
            ("NEW_VAR".to_string(), "y".to_string()),
        ]);
        let diff = env_diff(&proc_env, &ours);
        assert_eq!(
            diff,
            vec![
                "+ EXTRA=1".to_string(),
                "- NEW_VAR (not inherited)".to_string(),
                "~ PATH: proc=/old shell=/new".to_string(),
            ]
        );
    }

    /// Simulates the pnpm-respawn failure: a respawned process that exits
    /// immediately should surface a clear message with its stderr, and
    /// watch_respawn_crashes must not block waiting for it.
    #[test]
    fn respawn_crash_is_reported_non_blocking() {
        let manager = services::Manager::load().expect("no dev-panel.toml in test cwd");
        let mut app = App::new(manager);
        let child = std::process::Command::new("sh")
            .args(["-c", "echo boom 1>&2; exit 1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        app.respawn_watches.push(RespawnWatch {
            child,
            cmd: vec!["sh".into()],
            since: Instant::now(),
        });

        // Poll immediately: the process may not have exited yet, so this
        // call must return right away either way (no thread::sleep).
        let start = Instant::now();
        app.watch_respawn_crashes();
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "must not block"
        );

        // Give the child a moment to actually exit, then poll again.
        std::thread::sleep(Duration::from_millis(100));
        app.watch_respawn_crashes();

        assert!(
            app.message.contains("exited immediately") && app.message.contains("boom"),
            "message was: {}",
            app.message
        );
    }

    #[test]
    fn adopt_config_uses_port_fallback_name() {
        let cfg = adopt_config_from_process("", "npm run dev", None, Some(3000));
        assert_eq!(cfg.name, "port-3000");
        assert_eq!(cfg.port, Some(3000));
        assert_eq!(cfg.cmd, "npm run dev");
    }

    #[test]
    fn suggest_service_name_prefers_process_name() {
        let cfg = adopt_config_from_process("vite", "vite", None, Some(5173));
        assert_eq!(cfg.name, "vite");
    }

    /// End-to-end: a real foreign process (not a managed service) bound to a
    /// real port, discovered via the actual lsof-backed port scan, restarted
    /// through the exact key path the UI drives (r, then y). Reproduces the
    /// "restart kills but never comes back" report by running the whole
    /// pipeline instead of a hand-copied fragment of it.
    #[test]
    fn restart_of_foreign_process_actually_respawns() {
        use std::io::Read;
        let port: u16 = 42000 + (std::process::id() as u16 % 20000);
        let script = format!("require('http').createServer((q,r)=>r.end('ok')).listen({port});");
        let mut child = std::process::Command::new("node")
            .args(["-e", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn node — test requires node on PATH");
        let original_pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(400));

        let manager = services::Manager::load().expect("no dev-panel.toml in test cwd");
        let mut app = App::new(manager);
        app.poll();

        let row_idx = app
            .rows
            .iter()
            .position(|r| r.pid == Some(original_pid))
            .unwrap_or_else(|| {
                let mut stderr = String::new();
                let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
                panic!(
                    "node process (pid {original_pid}) never showed up in the port scan; rows: {:?}, node stderr: {stderr}",
                    app.rows.iter().map(|r| (r.pid, r.port, &r.name)).collect::<Vec<_>>()
                );
            });
        app.table.select(Some(row_idx));

        app.request_restart();
        assert!(app.pending.is_some(), "r should arm a pending restart");
        app.confirm_pending();

        // Drive the same loop main.rs does: poll every ~POLL_INTERVAL,
        // watch_respawn_crashes every tick, until a new pid owns the port
        // or we give up.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut new_pid = None;
        while Instant::now() < deadline {
            app.watch_respawn_crashes();
            app.poll();
            if let Some(r) = app.rows.iter().find(|r| r.port == Some(port)) {
                if r.pid != Some(original_pid) {
                    new_pid = r.pid;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        let _ = child.kill();
        let _ = child.wait();
        if let Some(pid) = new_pid {
            let _ = platform::force_kill(pid);
        }

        assert!(
            new_pid.is_some(),
            "port {port} never got a new owner after restart; last message: {}",
            app.message
        );
    }

    /// Regression for the pnpm-style report: a process launched with a
    /// *relative* script path from a directory other than dev-panel's own
    /// cwd. sysinfo's Process::cwd() is empty on this macOS config, so
    /// without the lsof_cwd fallback the naive respawn runs `node server.js`
    /// from dev-panel's directory instead of the project's, fails with
    /// "Cannot find module", and the port never comes back — exactly what
    /// was reported as "restart kills but never restarts".
    #[test]
    fn restart_preserves_original_working_directory() {
        // Derived from our own pid so a leftover process from a previously
        // panicked run (before cleanup ran) can't collide with this run.
        let port: u16 = 40000 + (std::process::id() as u16 % 20000);
        let dir = std::env::temp_dir().join(format!("dev-panel-cwd-test-{port}"));
        std::fs::create_dir_all(&dir).expect("create test project dir");
        std::fs::write(
            dir.join("server.js"),
            format!("require('http').createServer((q,r)=>r.end('ok')).listen({port});"),
        )
        .expect("write server.js");

        // Kills and reaps every pid it's holding on drop, including on
        // panic, so a failed assertion never leaks a node process squatting
        // on `port` and breaking the next run of this test. Extra pids
        // (e.g. the respawned child) are signalled but can't be wait()ed —
        // they aren't our direct children — so they're reaped by init/1
        // instead; that's fine, only the original child needs a real wait.
        struct KillOnDrop {
            child: std::process::Child,
            extra_pids: Vec<i32>,
        }
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                for pid in &self.extra_pids {
                    let _ = platform::force_kill(*pid);
                }
            }
        }

        let child = std::process::Command::new("node")
            .arg("server.js") // relative path: only resolves if cwd is set
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn node — test requires node on PATH");
        let original_pid = child.id() as i32;
        let mut guard = KillOnDrop {
            child,
            extra_pids: Vec::new(),
        };
        std::thread::sleep(Duration::from_millis(400));

        let manager = services::Manager::load().expect("no dev-panel.toml in test cwd");
        let mut app = App::new(manager);
        app.poll();

        let row_idx = app
            .rows
            .iter()
            .position(|r| r.pid == Some(original_pid))
            .unwrap_or_else(|| {
                panic!("node process (pid {original_pid}) never showed up in the port scan")
            });
        app.table.select(Some(row_idx));

        app.request_restart();
        let Some(Pending::Restart { cwd, .. }) = &app.pending else {
            panic!("expected a Restart pending after r on a foreign process");
        };
        // Compare canonicalized: macOS's /tmp -> /private/tmp symlink means
        // lsof reports the resolved path even though we created `dir` via
        // its unresolved alias.
        let canon_dir = dir.canonicalize().expect("canonicalize test dir");
        assert_eq!(
            cwd.as_deref().and_then(|c| c.canonicalize().ok()),
            Some(canon_dir),
            "restart must capture the process's real cwd (via lsof fallback), not None"
        );
        app.confirm_pending();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut respawned_ok = false;
        while Instant::now() < deadline {
            app.watch_respawn_crashes();
            app.poll();
            if let Some(r) = app.rows.iter().find(|r| r.port == Some(port)) {
                if r.pid != Some(original_pid) {
                    respawned_ok = true;
                    if let Some(new_pid) = r.pid {
                        guard.extra_pids.push(new_pid);
                    }
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            respawned_ok,
            "respawn from the correct cwd should succeed; last message: {}",
            app.message
        );
    }
}
