//! Ready checks for `run --wait`: tcp, http:PATH and log:REGEX.

use crate::state::Service;
use regex::Regex;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum ReadySpec {
    Tcp,
    Http(String),
    Log(Regex),
}

impl ReadySpec {
    pub fn name(&self) -> &'static str {
        match self {
            ReadySpec::Tcp => "tcp",
            ReadySpec::Http(_) => "http",
            ReadySpec::Log(_) => "log",
        }
    }

    pub fn needs_port(&self) -> bool {
        !matches!(self, ReadySpec::Log(_))
    }
}

/// clap value parser for `--ready`: `tcp`, `http:PATH` or `log:REGEX`.
pub fn parse_ready_spec(s: &str) -> std::result::Result<ReadySpec, String> {
    if s == "tcp" {
        return Ok(ReadySpec::Tcp);
    }
    if let Some(path) = s.strip_prefix("http:") {
        if !path.starts_with('/') || path.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "{path:?} is not an HTTP path (it must start with '/', without spaces)"
            ));
        }
        return Ok(ReadySpec::Http(path.to_string()));
    }
    if let Some(re) = s.strip_prefix("log:") {
        return regex::RegexBuilder::new(re)
            .multi_line(true)
            .build()
            .map(ReadySpec::Log)
            .map_err(|e| format!("invalid regex: {e}"));
    }
    Err("expected tcp, http:PATH or log:REGEX".into())
}

pub enum Outcome {
    Ready { ms: u64 },
    Exited,
    TimedOut { ms: u64 },
}

/// Waits until `check` passes for `svc`, the process exits, or `timeout`
/// passes. `log_offset` is where this instance's output starts in the log.
pub fn wait(svc: &Service, check: &ReadySpec, timeout: Duration, log_offset: u64) -> Outcome {
    let start = Instant::now();
    let deadline = start + timeout;
    loop {
        if !svc.is_running() {
            return Outcome::Exited;
        }
        let ok = match check {
            ReadySpec::Tcp => svc.port.is_some_and(tcp_ready),
            ReadySpec::Http(path) => svc.port.is_some_and(|p| http_ready(p, path)),
            ReadySpec::Log(re) => log_matches(svc, re, log_offset),
        };
        let ms = start.elapsed().as_millis() as u64;
        if ok {
            return Outcome::Ready { ms };
        }
        if Instant::now() >= deadline {
            return Outcome::TimedOut { ms };
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn loopbacks(port: u16) -> [SocketAddr; 2] {
    [
        ([127, 0, 0, 1], port).into(),
        (std::net::Ipv6Addr::LOCALHOST, port).into(),
    ]
}

fn tcp_ready(port: u16) -> bool {
    loopbacks(port)
        .iter()
        .any(|a| TcpStream::connect_timeout(a, Duration::from_millis(200)).is_ok())
}

/// Any HTTP response with a status below 500 counts as ready.
fn http_ready(port: u16, path: &str) -> bool {
    for addr in loopbacks(port) {
        let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) else {
            continue;
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        let req =
            format!("GET {path} HTTP/1.0\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
        if s.write_all(req.as_bytes()).is_err() {
            continue;
        }
        let mut buf = [0u8; 64];
        let mut got = Vec::new();
        while got.len() < 16 {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        let head = String::from_utf8_lossy(&got);
        let status = head
            .strip_prefix("HTTP/")
            .and_then(|r| r.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok());
        if matches!(status, Some(c) if c < 500) {
            return true;
        }
    }
    false
}

fn log_matches(svc: &Service, re: &Regex, offset: u64) -> bool {
    let Ok(mut f) = File::open(&svc.log_path) else {
        return false;
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    // CRLF line ends (Windows programs) are matched like LF, so that `$`
    // works at the end of a line.
    re.is_match(&String::from_utf8_lossy(&buf).replace("\r\n", "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_specs() {
        assert!(matches!(parse_ready_spec("tcp"), Ok(ReadySpec::Tcp)));
        assert!(
            matches!(parse_ready_spec("http:/health"), Ok(ReadySpec::Http(p)) if p == "/health")
        );
        assert!(matches!(
            parse_ready_spec("log:listening on \\d+"),
            Ok(ReadySpec::Log(_))
        ));
        assert!(parse_ready_spec("http:health").is_err());
        assert!(parse_ready_spec("log:(").is_err());
        assert!(parse_ready_spec("udp").is_err());
    }
}
