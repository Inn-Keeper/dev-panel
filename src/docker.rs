//! Docker container awareness via the docker CLI.
// ponytail: shells out to `docker` instead of bollard — no tokio, no async.
// Swap for bollard if we ever need the event stream.

use std::io;
use std::process::Command;

const FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Ports}}\t{{.Status}}";
const STOP_TIMEOUT_SECS: &str = "3";

#[derive(Debug, Clone)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub ports: String,
    pub status: String,
}

impl Container {
    /// First published host port, for open-in-browser / clipboard.
    pub fn host_port(&self) -> Option<u16> {
        first_host_port(&self.ports)
    }
}

pub fn list() -> io::Result<Vec<Container>> {
    let out = Command::new("docker")
        .args(["ps", "--format", FORMAT])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

// Blocking calls — callers run these off the UI thread (see
// app.rs::spawn_docker_op), since a docker stop/restart can take up to
// STOP_TIMEOUT_SECS.
pub fn stop(id: &str) -> io::Result<()> {
    run(&["stop", "-t", STOP_TIMEOUT_SECS, id])
}

pub fn restart(id: &str) -> io::Result<()> {
    run(&["restart", "-t", STOP_TIMEOUT_SECS, id])
}

fn run(args: &[&str]) -> io::Result<()> {
    let out = Command::new("docker").args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

fn parse_ps(s: &str) -> Vec<Container> {
    s.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                return None;
            }
            Some(Container {
                id: f[0].to_string(),
                name: f[1].to_string(),
                image: f[2].to_string(),
                ports: f[3].to_string(),
                status: f[4].to_string(),
            })
        })
        .collect()
}

/// Pull the first host port out of a docker ports string like
/// `0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp`.
fn first_host_port(ports: &str) -> Option<u16> {
    ports.split("->").next()?.rsplit(':').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_ps_and_host_port() {
        let fixture = "abc123\tpg\tpostgres:16\t0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp\tUp 2 hours\n\
                       def456\tredis\tredis:7\t\tUp 5 minutes\n";
        let got = parse_ps(fixture);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "pg");
        assert_eq!(got[0].host_port(), Some(5432));
        assert_eq!(got[1].host_port(), None, "no published ports");
    }
}
