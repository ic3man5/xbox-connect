//! Timestamped connection history in %LOCALAPPDATA%\XboxConnect\events.log, so a lost pairing
//! or a failed reconnect can be traced afterwards. Kept small: rolled over to events.old.log.
//! The most recent lines are also kept in memory for the window's Events tab.

use std::collections::VecDeque;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 256 * 1024;
const KEEP: usize = 500;

static RECENT: Mutex<Option<VecDeque<String>>> = Mutex::new(None);

/// Runs `f` on the in-memory history, seeding it from the end of the log file the first time.
fn with_recent<R>(f: impl FnOnce(&mut VecDeque<String>) -> R) -> R {
    let mut guard = crate::bt::lock(&RECENT);
    let recent = guard.get_or_insert_with(|| {
        let text = crate::log_dir().and_then(|d| std::fs::read_to_string(d.join("events.log")).ok());
        let mut q: VecDeque<String> = text.unwrap_or_default().lines().map(str::to_string).collect();
        while q.len() > KEEP {
            q.pop_front();
        }
        q
    });
    f(recent)
}

/// Snapshot of the most recent lines, oldest first.
pub fn recent() -> Vec<String> {
    with_recent(|q| q.iter().cloned().collect())
}

pub fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {}",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        msg.as_ref()
    );
    // Seed from the file before appending, so the new line isn't read back in twice.
    with_recent(|q| {
        q.push_back(line.clone());
        if q.len() > KEEP {
            q.pop_front();
        }
    });
    let Some(dir) = crate::log_dir() else { return };
    let path = dir.join("events.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, dir.join("events.old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}
