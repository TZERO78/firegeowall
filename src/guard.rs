//! Self-protection: find the address of the admin's SSH session, so `fgw add`
//! can refuse to block the country it comes from.

use std::fs;
use std::net::IpAddr;

/// Client address of the SSH session this command runs in. `sudo` drops
/// `SSH_CONNECTION` from the environment, so walk up the process tree and read
/// it from the first ancestor that still has it (root can read every environ).
pub fn ssh_client() -> Option<IpAddr> {
    if let Some(ip) = std::env::var("SSH_CONNECTION")
        .ok()
        .as_deref()
        .and_then(parse)
    {
        return Some(ip);
    }
    let mut pid = parent(std::process::id())?;
    for _ in 0..32 {
        if pid <= 1 {
            break;
        }
        if let Ok(env) = fs::read(format!("/proc/{pid}/environ")) {
            for var in env.split(|b| *b == 0) {
                if let Some(v) = var.strip_prefix(b"SSH_CONNECTION=")
                    && let Some(ip) = std::str::from_utf8(v).ok().and_then(parse)
                {
                    return Some(ip);
                }
            }
        }
        pid = parent(pid)?;
    }
    None
}

/// `SSH_CONNECTION` is "client_ip client_port server_ip server_port".
fn parse(v: &str) -> Option<IpAddr> {
    v.split_whitespace().next()?.parse().ok()
}

fn parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses; fields resume after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_ssh_connection() {
        assert_eq!(
            super::parse("203.0.113.9 51234 10.0.0.1 22"),
            Some("203.0.113.9".parse().unwrap())
        );
        assert_eq!(super::parse(""), None);
        assert!(super::parent(std::process::id()).is_some());
    }
}
