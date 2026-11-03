//! Small helpers: atomic writes, ownership, locking, time and number formatting.

use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Real uid of this process, from /proc (no libc needed).
pub fn uid() -> Option<u32> {
    let s = fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok() // effective uid
}

/// When an admin runs `sudo fgw add`, files must still belong to the service
/// user that owns the state directory, or the timer could not replace them.
fn match_owner(path: &Path, like: &Path) {
    if uid() != Some(0) {
        return;
    }
    if let Ok(m) = fs::metadata(like) {
        let _ = std::os::unix::fs::chown(path, Some(m.uid()), Some(m.gid()));
    }
}

pub fn ensure_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o750))?;
    if let Some(parent) = dir.parent() {
        match_owner(dir, parent);
    }
    Ok(())
}

/// Write via temp file + fsync + rename, so readers never see a half file.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("fgw"),
        std::process::id()
    ));
    let res = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        match_owner(&tmp, dir);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("cannot write {}", path.display()))
}

/// Exclusive lock on `<state_dir>/lock` for the lifetime of the returned file,
/// so the timer and an admin never rebuild at the same time.
pub fn lock(state_dir: &Path) -> Result<fs::File> {
    ensure_dir(state_dir)?;
    let p = state_dir.join("lock");
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&p)
        .with_context(|| format!("cannot open {}", p.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(fs::TryLockError::WouldBlock) => {
            eprintln!("fgw: waiting for another fgw run to finish ...");
            f.lock()?;
            Ok(f)
        }
        Err(fs::TryLockError::Error(e)) => Err(e).context("cannot lock state directory"),
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// (year, month, day) for days since 1970-01-01 (H. Hinnant's algorithm).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe as i64 + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `2026-11-01 03:30 UTC`
pub fn fmt_time(t: u64) -> String {
    let (y, m, d) = civil((t / 86_400) as i64);
    let s = t % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        s / 3600,
        s % 3600 / 60
    )
}

/// `24,430`
pub fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_numbers() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(fmt_time(1_793_590_200), "2026-11-02 03:30 UTC");
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(1234), "1,234");
        assert_eq!(fmt_num(24_430_000), "24,430,000");
    }
}
