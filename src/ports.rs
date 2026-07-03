//! Listening-port discovery, one backend per OS.
//! Backends fill `port` + `pid` (macOS also name/user); the app enriches
//! the rest from sysinfo.

use std::io;

#[derive(Debug, Clone, Default)]
pub struct PortEntry {
    pub port: u16,
    pub pid: i32,
    pub name: String,
    pub user: String,
    /// Full command line, enriched from sysinfo after parsing. Empty until then.
    pub cmd: String,
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
pub fn listening_ports() -> io::Result<Vec<PortEntry>> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-FpcnL"])
        .output()?;
    // lsof exits non-zero when nothing matches; only stdout matters here.
    Ok(parse_lsof(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `lsof -F` field output: one field per line, first char is the tag.
/// `p` starts a process block (pid), `c` command name, `L` login user,
/// `n` a listening address like `*:3000` or `[::1]:5432`.
#[cfg(any(test, target_os = "macos"))]
fn parse_lsof(s: &str) -> Vec<PortEntry> {
    let mut entries: Vec<PortEntry> = Vec::new();
    let (mut pid, mut name, mut user) = (0i32, String::new(), String::new());
    for line in s.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let val = &line[1..];
        match tag {
            'p' => pid = val.parse().unwrap_or(0),
            'c' => name = val.to_string(),
            'L' => user = val.to_string(),
            'n' => {
                if let Some(port) = val.rsplit(':').next().and_then(|p| p.parse().ok()) {
                    entries.push(PortEntry {
                        port,
                        pid,
                        name: name.clone(),
                        user: user.clone(),
                        cmd: String::new(),
                    });
                }
            }
            _ => {}
        }
    }
    entries.sort_by_key(|e| (e.pid, e.port));
    entries.dedup_by(|a, b| a.pid == b.pid && a.port == b.port);
    entries
}

// ---------------------------------------------------------------- Linux

/// Per the spec: parse /proc/net/tcp{,6}, cross-reference socket inodes
/// against /proc/<pid>/fd/* symlinks.
#[cfg(target_os = "linux")]
pub fn listening_ports() -> io::Result<Vec<PortEntry>> {
    use std::collections::HashMap;

    let mut inode_to_port: Vec<(u64, u16)> = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            inode_to_port.extend(parse_proc_net_tcp(&s));
        }
    }

    let mut inode_to_pid: HashMap<u64, i32> = HashMap::new();
    for proc_entry in std::fs::read_dir("/proc")? {
        let proc_entry = proc_entry?;
        let Ok(pid) = proc_entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(proc_entry.path().join("fd")) else {
            continue; // not ours to inspect (permissions) — skip
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                let target = target.to_string_lossy();
                if let Some(inode) = target
                    .strip_prefix("socket:[")
                    .and_then(|t| t.strip_suffix(']'))
                    .and_then(|t| t.parse().ok())
                {
                    inode_to_pid.insert(inode, pid);
                }
            }
        }
    }

    let mut entries: Vec<PortEntry> = inode_to_port
        .into_iter()
        .filter_map(|(inode, port)| {
            Some(PortEntry {
                port,
                pid: *inode_to_pid.get(&inode)?,
                ..Default::default()
            })
        })
        .collect();
    entries.sort_by_key(|e| (e.port, e.pid));
    entries.dedup_by(|a, b| a.pid == b.pid && a.port == b.port);
    Ok(entries)
}

/// Parse /proc/net/tcp format: whitespace fields, local addr is HEXIP:HEXPORT,
/// state 0A = LISTEN, inode at field index 9.
#[cfg(any(test, target_os = "linux"))]
fn parse_proc_net_tcp(s: &str) -> Vec<(u64, u16)> {
    const STATE_LISTEN: &str = "0A";
    s.lines()
        .skip(1) // header
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != STATE_LISTEN {
                return None;
            }
            let port = u16::from_str_radix(f[1].rsplit(':').next()?, 16).ok()?;
            let inode: u64 = f[9].parse().ok()?;
            Some((inode, port))
        })
        .collect()
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
pub fn listening_ports() -> io::Result<Vec<PortEntry>> {
    let out = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("netstat failed"));
    }
    Ok(parse_netstat(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `netstat -ano` lines: `TCP  0.0.0.0:3000  0.0.0.0:0  LISTENING  1234`.
#[cfg(any(test, windows))]
fn parse_netstat(s: &str) -> Vec<PortEntry> {
    let mut entries: Vec<PortEntry> = s
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 5 || f[0] != "TCP" || f[3] != "LISTENING" {
                return None;
            }
            Some(PortEntry {
                port: f[1].rsplit(':').next()?.parse().ok()?,
                pid: f[4].parse().ok()?,
                ..Default::default()
            })
        })
        .collect();
    entries.sort_by_key(|e| (e.port, e.pid));
    entries.dedup_by(|a, b| a.pid == b.pid && a.port == b.port);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_field_output() {
        let fixture = "p512\ncnode\nLdalton\nf23\nn*:3000\nf24\nn*:3000\np777\ncpostgres\nLdalton\nf7\nn127.0.0.1:5432\nn[::1]:5432\nf9\nn*:*\n";
        let entries = parse_lsof(fixture);
        assert_eq!(
            entries.len(),
            2,
            "dedupes v4/v6 twins, skips unparseable ports"
        );
        assert_eq!(
            (entries[0].port, entries[0].pid, entries[0].name.as_str()),
            (3000, 512, "node")
        );
        assert_eq!(
            (entries[1].port, entries[1].pid, entries[1].user.as_str()),
            (5432, 777, "dalton")
        );
    }

    #[test]
    fn dedupes_non_adjacent_duplicate_pid_port() {
        // pid 512 on port 3000, then a different process, then pid 512 on
        // port 3000 again in a separate (non-contiguous) block — adjacent-
        // only dedup would miss this second occurrence.
        let fixture = "p512\ncnode\nLdalton\nf23\nn*:3000\n\
                       p777\ncpostgres\nLdalton\nf7\nn127.0.0.1:5432\n\
                       p512\ncnode\nLdalton\nf25\nn*:3000\n";
        let entries = parse_lsof(fixture);
        assert_eq!(
            entries.len(),
            2,
            "non-adjacent (512, 3000) duplicate must still be caught: {entries:?}"
        );
    }

    #[test]
    fn parses_proc_net_tcp() {
        let fixture = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 43217 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1538 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 51888 1 0000000000000000 100 0 0 10 0
   2: 0100007F:AAAA 0100007F:0BB8 01 00000000:00000000 00:00000000 00000000  1000        0 99999 1 0000000000000000 100 0 0 10 0
";
        let got = parse_proc_net_tcp(fixture);
        assert_eq!(got, vec![(43217, 0x0BB8), (51888, 0x1538)], "LISTEN only");
    }

    #[test]
    fn parses_netstat_output() {
        let fixture = "\
Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:3000           0.0.0.0:0              LISTENING       1084
  TCP    127.0.0.1:5432         0.0.0.0:0              LISTENING       412
  TCP    127.0.0.1:5432         127.0.0.1:9999         ESTABLISHED     412
  UDP    0.0.0.0:5353           *:*                                    77
";
        let got = parse_netstat(fixture);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].port, got[0].pid), (3000, 1084));
        assert_eq!((got[1].port, got[1].pid), (5432, 412));
    }
}
