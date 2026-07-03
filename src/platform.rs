//! OS-specific glue: shells, signals, openers, clipboard, systemd.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Build a `Command` that runs `cmd` through the platform shell.
pub fn shell(cmd: &str, cwd: Option<&Path>) -> Command {
    #[cfg(unix)]
    let mut c = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    #[cfg(windows)]
    let mut c = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c
}

/// Fallback command-line lookup for when sysinfo's `Process::cmd()` comes
/// back empty — observed on recent macOS, where `KERN_PROCARGS2` is denied
/// to unentitled processes even for their own pid. `ps` is setuid-root and
/// keeps working, so shell out to it as a last resort.
///
/// ponytail: naive whitespace split, not real argv — a command with a
/// quoted argument containing spaces (`node -e "a b"`) mis-splits. Good
/// enough for a fallback; the primary path (sysinfo) doesn't have this
/// problem when it works.
#[cfg(unix)]
pub fn ps_cmdline(pid: i32) -> Option<Vec<String>> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(line.split_whitespace().map(str::to_string).collect())
}

#[cfg(windows)]
pub fn ps_cmdline(_pid: i32) -> Option<Vec<String>> {
    None
}

/// Fallback cwd lookup for when sysinfo's `Process::cwd()` comes back empty
/// — same macOS entitlement restriction as `ps_cmdline`. `lsof -d cwd`
/// reports it without hitting that restriction. This one matters more than
/// the cmd fallback: a naive restart that loses cwd runs the respawned
/// command from dev-panel's own directory instead of the original process's,
/// so e.g. `node server.js` fails with "Cannot find module" and the process
/// silently never comes back.
#[cfg(target_os = "macos")]
pub fn lsof_cwd(pid: i32) -> Option<std::path::PathBuf> {
    let out = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(std::path::PathBuf::from)
}

#[cfg(not(target_os = "macos"))]
pub fn lsof_cwd(_pid: i32) -> Option<std::path::PathBuf> {
    None
}

#[cfg(unix)]
pub fn terminate(pid: i32) -> io::Result<()> {
    use nix::sys::signal::{Signal, kill};
    kill(nix::unistd::Pid::from_raw(pid), Signal::SIGTERM).map_err(io::Error::from)
}

#[cfg(unix)]
pub fn force_kill(pid: i32) -> io::Result<()> {
    use nix::sys::signal::{Signal, kill};
    kill(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL).map_err(io::Error::from)
}

// ponytail: taskkill without /F is WM_CLOSE — console apps ignore it and get
// caught by the /F escalation instead.
#[cfg(windows)]
pub fn terminate(pid: i32) -> io::Result<()> {
    run_ok(Command::new("taskkill").args(["/PID", &pid.to_string()]))
}

#[cfg(windows)]
pub fn force_kill(pid: i32) -> io::Result<()> {
    run_ok(Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]))
}

pub fn is_root() -> bool {
    #[cfg(unix)]
    return nix::unistd::Uid::effective().is_root();
    // ponytail: no admin detection on Windows; the root-owned guard is a Unix concern.
    #[cfg(windows)]
    return false;
}

/// Open a URL or file path with the system opener, detached.
pub fn open_external(target: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut c = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut c = Command::new("xdg-open");
    #[cfg(windows)]
    let mut c = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    c.arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Open a folder in the editor: `code` if available, else system opener.
pub fn open_in_editor(path: &Path) -> io::Result<()> {
    let via_code = Command::new("code")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match via_code {
        Ok(_) => Ok(()),
        Err(_) => open_external(&path.display().to_string()),
    }
}

pub fn copy_clipboard(text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    return pipe_to(&mut Command::new("pbcopy"), text);
    #[cfg(target_os = "linux")]
    {
        for (bin, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["-ib"][..]),
        ] {
            let mut c = Command::new(bin);
            c.args(args);
            match pipe_to(&mut c, text) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        return Err(io::Error::other("no clipboard tool (wl-copy/xclip/xsel)"));
    }
    #[cfg(windows)]
    return pipe_to(&mut Command::new("clip"), text);
}

fn pipe_to(cmd: &mut Command, text: &str) -> io::Result<()> {
    use std::io::Write;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("clipboard tool exited non-zero"))
    }
}

// Only Windows (taskkill) and Linux (systemctl) callers exist.
#[cfg(any(windows, target_os = "linux"))]
fn run_ok(cmd: &mut Command) -> io::Result<()> {
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Extract a systemd unit name from `/proc/<pid>/cgroup` contents.
#[cfg(any(test, target_os = "linux"))]
pub fn parse_cgroup_unit(s: &str) -> Option<String> {
    for line in s.lines() {
        for seg in line.rsplit('/') {
            if seg.ends_with(".service") {
                return Some(seg.to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub fn systemd_unit(pid: i32) -> Option<String> {
    parse_cgroup_unit(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

#[cfg(not(target_os = "linux"))]
pub fn systemd_unit(_pid: i32) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
pub fn systemctl_restart(unit: &str) -> io::Result<()> {
    run_ok(Command::new("systemctl").args(["restart", unit]))
}

#[cfg(not(target_os = "linux"))]
pub fn systemctl_restart(_unit: &str) -> io::Result<()> {
    Err(io::Error::other("systemd restart is Linux-only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_systemd_unit_in_cgroup() {
        let cgroup = "0::/system.slice/myapp.service\n";
        assert_eq!(parse_cgroup_unit(cgroup).as_deref(), Some("myapp.service"));
        let user_scope = "0::/user.slice/user-501.slice/session-3.scope\n";
        assert_eq!(parse_cgroup_unit(user_scope), None);
    }

    #[cfg(unix)]
    #[test]
    fn ps_cmdline_reads_own_process() {
        // Regression check for the case that motivated this fallback: on
        // some macOS configs sysinfo's Process::cmd() is empty even for our
        // own pid. `ps` should still resolve it via setuid.
        let pid = std::process::id() as i32;
        let cmd = ps_cmdline(pid).expect("ps should see our own process");
        assert!(!cmd.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ps_cmdline_none_for_dead_pid() {
        // A pid unlikely to exist; ps prints nothing and we should return None.
        assert_eq!(ps_cmdline(999_999), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lsof_cwd_reads_own_process() {
        let pid = std::process::id() as i32;
        let cwd = lsof_cwd(pid).expect("lsof should see our own process cwd");
        assert!(cwd.is_absolute());
    }
}
