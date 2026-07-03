//! Managed services: dev-panel.toml config, child lifecycle, logs, health,
//! crash detection with backoff, session persistence.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::platform;

pub const CONFIG_FILE: &str = "dev-panel.toml";
const SESSION_FILE: &str = ".dev-panel-session";
const LOG_CAP: usize = 1000;
const STARTING_GRACE: Duration = Duration::from_secs(5);
const STOP_ESCALATE_AFTER: Duration = Duration::from_secs(3);
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Deserialize, Debug, Clone)]
pub struct ServiceConfig {
    pub name: String,
    pub cmd: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub auto_restart: bool,
}

#[derive(Deserialize, Debug, Default)]
struct Config {
    #[serde(default, rename = "service")]
    services: Vec<ServiceConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Stopped,
    Starting,
    Up,
    Down,
    Crashed,
    /// Crashed with auto_restart — waiting out the backoff.
    Scheduled,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Stopped => "stopped",
            Status::Starting => "starting",
            Status::Up => "up",
            Status::Down => "down",
            Status::Crashed => "crashed",
            Status::Scheduled => "restarting",
        }
    }
}

pub struct Service {
    pub cfg: ServiceConfig,
    pub status: Status,
    pub logs: VecDeque<String>,
    child: Option<Child>,
    started: Option<Instant>,
    stop_requested: Option<Instant>,
    user_stopped: bool,
    restart_pending: bool,
    backoff: Duration,
    restart_at: Option<Instant>,
}

impl Service {
    pub fn pid(&self) -> Option<i32> {
        self.child.as_ref().map(|c| c.id() as i32)
    }

    pub fn running(&self) -> bool {
        self.child.is_some()
    }
}

pub struct Manager {
    pub services: Vec<Service>,
    log_tx: Sender<(usize, String)>,
    log_rx: Receiver<(usize, String)>,
    session_path: PathBuf,
}

impl Manager {
    /// Load dev-panel.toml from the working directory. Missing file = no
    /// services; a malformed file is a hard error the caller must surface.
    pub fn load() -> Result<Self, String> {
        let config = match std::fs::read_to_string(CONFIG_FILE) {
            Ok(s) => parse_config(&s)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("{CONFIG_FILE}: {e}")),
        };
        let (log_tx, log_rx) = channel();
        let services = config
            .services
            .into_iter()
            .map(|cfg| Service {
                cfg,
                status: Status::Stopped,
                logs: VecDeque::new(),
                child: None,
                started: None,
                stop_requested: None,
                user_stopped: false,
                restart_pending: false,
                backoff: BACKOFF_INITIAL,
                restart_at: None,
            })
            .collect();
        Ok(Self {
            services,
            log_tx,
            log_rx,
            session_path: SESSION_FILE.into(),
        })
    }

    pub fn start(&mut self, i: usize) -> io::Result<()> {
        let svc = &mut self.services[i];
        if svc.child.is_some() {
            return Ok(());
        }
        let cwd = svc.cfg.cwd.as_ref().map(PathBuf::from);
        let mut child = platform::shell(&svc.cfg.cmd, cwd.as_deref())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        for pipe in [
            child.stdout.take().map(io_box),
            child.stderr.take().map(io_box),
        ]
        .into_iter()
        .flatten()
        {
            let tx = self.log_tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send((i, line)).is_err() {
                        break;
                    }
                }
            });
        }
        svc.child = Some(child);
        svc.status = Status::Starting;
        svc.started = Some(Instant::now());
        svc.stop_requested = None;
        svc.user_stopped = false;
        svc.restart_at = None;
        self.save_session();
        Ok(())
    }

    pub fn stop(&mut self, i: usize) {
        let svc = &mut self.services[i];
        svc.user_stopped = true;
        svc.restart_at = None;
        if let Some(pid) = svc.pid() {
            if let Err(e) = platform::terminate(pid) {
                svc.logs.push_back(format!("[dev-panel] stop failed: {e}"));
            }
            svc.stop_requested = Some(Instant::now());
        }
        self.save_session();
    }

    /// Stop (if running) then start again once the child exits.
    pub fn restart(&mut self, i: usize) -> io::Result<()> {
        if self.services[i].running() {
            self.services[i].restart_pending = true;
            self.stop(i);
            Ok(())
        } else {
            self.start(i)
        }
    }

    /// Drive lifecycles: drain logs, reap exits, escalate stops, health-check,
    /// fire backoff restarts. Call once per poll tick.
    pub fn tick(&mut self) {
        while let Ok((i, line)) = self.log_rx.try_recv() {
            let logs = &mut self.services[i].logs;
            if logs.len() >= LOG_CAP {
                logs.pop_front();
            }
            logs.push_back(line);
        }

        let mut to_start = Vec::new();
        for (i, svc) in self.services.iter_mut().enumerate() {
            if let Some(child) = &mut svc.child {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        svc.child = None;
                        if svc.restart_pending {
                            svc.restart_pending = false;
                            to_start.push(i);
                        } else if svc.user_stopped {
                            svc.status = Status::Stopped;
                        } else {
                            svc.status = Status::Crashed;
                            svc.logs
                                .push_back("[dev-panel] process exited unexpectedly".into());
                            if svc.cfg.auto_restart {
                                svc.status = Status::Scheduled;
                                svc.restart_at = Some(Instant::now() + svc.backoff);
                                svc.backoff = (svc.backoff * 2).min(BACKOFF_MAX);
                            }
                        }
                    }
                    Ok(None) => {
                        if let Some(at) = svc.stop_requested {
                            if at.elapsed() >= STOP_ESCALATE_AFTER {
                                let _ = child.kill(); // SIGKILL; exit reaped next tick
                                svc.logs
                                    .push_back("[dev-panel] escalated to SIGKILL".into());
                                svc.stop_requested = None;
                            }
                        } else {
                            svc.status = health(svc);
                            if svc.status == Status::Up {
                                svc.backoff = BACKOFF_INITIAL;
                            }
                        }
                    }
                    Err(e) => svc.logs.push_back(format!("[dev-panel] wait failed: {e}")),
                }
            } else if svc.restart_at.is_some_and(|t| Instant::now() >= t) {
                to_start.push(i);
            }
        }
        for i in to_start {
            if let Err(e) = self.start(i) {
                let svc = &mut self.services[i];
                svc.logs
                    .push_back(format!("[dev-panel] restart failed: {e}"));
                svc.status = Status::Crashed;
            }
        }
    }

    /// v3 session persistence: names of services meant to be running.
    fn save_session(&self) {
        let names: Vec<&str> = self
            .services
            .iter()
            .filter(|s| s.running() && !s.user_stopped)
            .map(|s| s.cfg.name.as_str())
            .collect();
        if names.is_empty() {
            let _ = std::fs::remove_file(&self.session_path);
        } else if let Err(e) = std::fs::write(&self.session_path, names.join("\n")) {
            eprintln!("session save failed: {e}");
        }
    }

    /// Start whatever the previous session had running. Returns messages.
    pub fn resume_session(&mut self) -> Vec<String> {
        let Ok(saved) = std::fs::read_to_string(&self.session_path) else {
            return Vec::new();
        };
        let mut msgs = Vec::new();
        for name in saved.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match self.services.iter().position(|s| s.cfg.name == name) {
                Some(i) => match self.start(i) {
                    Ok(()) => msgs.push(format!("resumed {name}")),
                    Err(e) => msgs.push(format!("resume {name} failed: {e}")),
                },
                None => msgs.push(format!("session had unknown service {name}")),
            }
        }
        msgs
    }
}

fn io_box(r: impl io::Read + Send + 'static) -> Box<dyn io::Read + Send> {
    Box::new(r)
}

fn parse_config(s: &str) -> Result<Config, String> {
    toml::from_str(s).map_err(|e| format!("{CONFIG_FILE}: {e}"))
}

/// Health = TCP connect to the configured port.
// ponytail: connect success counts as Up; upgrade to a real HTTP GET + status
// check if a wedged-but-listening server ever matters.
fn health(svc: &Service) -> Status {
    let Some(port) = svc.cfg.port else {
        return Status::Up; // no port to probe; running is the best we know
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&addr, HEALTH_TIMEOUT).is_ok() {
        return Status::Up;
    }
    let in_grace = svc.started.is_some_and(|t| t.elapsed() < STARTING_GRACE);
    if in_grace {
        Status::Starting
    } else {
        Status::Down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_with_defaults() {
        let toml = r#"
[[service]]
name = "frontend"
cmd = "npm run dev"
cwd = "./web"
port = 3000

[[service]]
name = "worker"
cmd = "npm run worker"
auto_restart = true
"#;
        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.services.len(), 2);
        assert_eq!(cfg.services[0].port, Some(3000));
        assert!(!cfg.services[0].auto_restart);
        let worker = &cfg.services[1];
        assert_eq!(
            (worker.port, worker.cwd.as_deref(), worker.auto_restart),
            (None, None, true)
        );
    }

    #[test]
    fn rejects_malformed_config() {
        assert!(parse_config("[[service]]\ncmd = \"no name\"").is_err());
    }

    /// End-to-end lifecycle: spawn, capture logs, detect crash, schedule
    /// the backoff restart.
    #[cfg(unix)]
    #[test]
    fn crash_is_detected_and_restart_scheduled() {
        let (log_tx, log_rx) = channel();
        let mut m = Manager {
            services: vec![Service {
                cfg: ServiceConfig {
                    name: "boom".into(),
                    cmd: "echo hi; exit 3".into(),
                    cwd: None,
                    port: None,
                    auto_restart: true,
                },
                status: Status::Stopped,
                logs: VecDeque::new(),
                child: None,
                started: None,
                stop_requested: None,
                user_stopped: false,
                restart_pending: false,
                backoff: BACKOFF_INITIAL,
                restart_at: None,
            }],
            log_tx,
            log_rx,
            session_path: std::env::temp_dir()
                .join(format!("dev-panel-test-{}", std::process::id())),
        };
        m.start(0).unwrap();
        assert!(m.services[0].running());

        let deadline = Instant::now() + Duration::from_secs(5);
        while m.services[0].status != Status::Scheduled {
            assert!(Instant::now() < deadline, "service never reached Scheduled");
            std::thread::sleep(Duration::from_millis(50));
            m.tick();
        }
        // one more drain: the reader thread may deliver "hi" just after exit
        std::thread::sleep(Duration::from_millis(50));
        m.tick();
        assert!(
            m.services[0].logs.iter().any(|l| l == "hi"),
            "stdout captured"
        );
        assert_eq!(
            m.services[0].backoff,
            BACKOFF_INITIAL * 2,
            "backoff doubled"
        );
        let _ = std::fs::remove_file(&m.session_path);
    }
}
