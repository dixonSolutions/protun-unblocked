//! One connect at a time, process-wide and machine-wide.
//!
//! NetworkManager's `protun` plugin allows exactly one active connection,
//! so two overlapping connects do not double the odds of a tunnel — they
//! guarantee a refusal: `The 'protun' plugin only supports a single active
//! connection`. Measured on 2026-09-11: a manual `pvpn hop` and
//! `pvpn-autoconnect`'s `pvpn up` overlapped, the hop's connect was
//! refused sixteen seconds after Proton's state machine had said
//! Disconnected, and the hop's failure cleanup then tore down the working
//! tunnel the autoconnect had just built. The autoconnect script has
//! always serialised its own instances with a flock and says why in a
//! comment: "pvpn has no inter-process lock of its own, and two
//! overlapping connects are what strand the kill switch." This is that
//! lock.
//!
//! An advisory file lock (`std::fs::File::try_lock`, flock on Linux) held
//! for the whole of an `up`, `hop`, `try` or `down`. The kernel releases
//! it when the holding process dies for any reason — Ctrl-C, a panic, a
//! hard kill, a reboot — so only a live connect can hold everyone else
//! out, and a stale lock file can never exist.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

/// How often to repeat the waiting message, so a long connect happening
/// in another process does not look like this one has hung.
const WAIT_NARRATION_EVERY: Duration = Duration::from_secs(20);

/// Held for the duration of one network-moving command. Dropping it
/// releases the lock.
#[derive(Debug)]
pub struct ConnectGuard {
    /// `None` when the lock file itself could not be opened — a connect
    /// must never be refused over a bookkeeping failure, so the guard
    /// still exists and simply excludes nobody.
    _file: Option<File>,
}

/// The lock lives in the runtime directory: per-user, on tmpfs, and gone
/// at reboot, which is one more way a stale lock can never outlive its
/// process. The data directory is the fallback for a session that has no
/// runtime directory at all.
fn lock_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(runtime);
        if dir.is_dir() {
            return dir.join("pvpn-connect.lock");
        }
    }
    crate::config::Config::data_dir().join("connect.lock")
}

fn open_lock_file(path: &PathBuf) -> Option<File> {
    // No truncate: a concurrent holder's note must survive our open. The
    // note is replaced explicitly after the lock is taken, and only then.
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => Some(file),
        Err(err) => {
            tracing::warn!("could not open {}: {err} — connecting without the lock", path.display());
            None
        }
    }
}

fn try_acquire(file: &File) -> bool {
    match file.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        // Fail open, as with an unopenable lock file: a connect is never
        // refused over the bookkeeping that protects it.
        Err(std::fs::TryLockError::Error(err)) => {
            tracing::warn!("could not take the connect lock: {err} — proceeding without it");
            true
        }
    }
}

/// Who holds the lock, for the waiting message. The holder writes its pid
/// and command line into the file once it owns the lock; an empty file
/// means the lock was taken between our open and this read.
fn holder_description(file: &File) -> String {
    let mut file = file;
    let mut text = String::new();
    if file.seek(SeekFrom::Start(0)).is_ok() && file.read_to_string(&mut text).is_ok() {
        let text = text.trim();
        if !text.is_empty() {
            return format!("another pvpn ({text})");
        }
    }
    "another pvpn".to_string()
}

/// Record who we are for anyone who comes waiting. Best-effort: the lock
/// is the lock, the note is a courtesy.
fn write_holder_note(file: &File) {
    let mut file = file;
    let args: Vec<String> = std::env::args().take(3).collect();
    let note = format!("pid {}: {}", std::process::id(), args.join(" "));
    if file.seek(SeekFrom::Start(0)).is_ok() {
        let _ = file.set_len(0);
        let _ = file.write_all(note.as_bytes());
    }
}

/// Take the lock, waiting as long as the other connect takes.
///
/// There is deliberately no timeout: a connect on a hostile network can
/// legitimately run for minutes, and the alternative to waiting is the
/// overlap this module exists to prevent. Ctrl-C is the way out — nothing
/// here installs a signal handler, so it still exits the process outright,
/// which is safe: a waiter has touched nothing and holds nothing.
pub async fn acquire() -> ConnectGuard {
    let Some(file) = open_lock_file(&lock_path()) else {
        return ConnectGuard { _file: None };
    };
    if try_acquire(&file) {
        write_holder_note(&file);
        return ConnectGuard { _file: Some(file) };
    }
    let holder = holder_description(&file);
    tracing::info!("{holder} is already working — waiting for it to finish (Ctrl-C to cancel)");
    let mut since_narration = WAIT_NARRATION_EVERY;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if try_acquire(&file) {
            tracing::info!("the other pvpn finished — continuing");
            write_holder_note(&file);
            return ConnectGuard { _file: Some(file) };
        }
        since_narration += Duration::from_secs(1);
        if since_narration >= WAIT_NARRATION_EVERY {
            since_narration = Duration::ZERO;
            tracing::info!("still waiting for {holder} — Ctrl-C to cancel");
        }
    }
}

/// Take the lock, giving up after `patience`. For `down`: an explicit
/// disconnect is the user reaching for the off switch, and making them
/// sit through someone else's whole connect first is the worse evil —
/// after the wait, `down` proceeds unlocked and tears down anyway.
pub async fn acquire_bounded(patience: Duration) -> Option<ConnectGuard> {
    let file = open_lock_file(&lock_path())?;
    if try_acquire(&file) {
        write_holder_note(&file);
        return Some(ConnectGuard { _file: Some(file) });
    }
    let holder = holder_description(&file);
    tracing::info!("{holder} is already working — giving it {}s to finish", patience.as_secs());
    let deadline = std::time::Instant::now() + patience;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if try_acquire(&file) {
            write_holder_note(&file);
            return Some(ConnectGuard { _file: Some(file) });
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!("{holder} is still working — tearing down anyway");
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_lock_on_the_same_file_is_refused_until_the_first_drops() {
        let dir = std::env::temp_dir().join(format!("pvpn-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connect.lock");
        let first = open_lock_file(&path).unwrap();
        let second = open_lock_file(&path).unwrap();

        assert!(try_acquire(&first));
        assert!(!try_acquire(&second), "held by this process's first fd");

        drop(first);
        assert!(try_acquire(&second), "freed when the holder's fd closed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_holder_note_is_written_and_read_back() {
        let dir = std::env::temp_dir().join(format!("pvpn-lock-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connect.lock");
        let holder = open_lock_file(&path).unwrap();
        assert!(try_acquire(&holder));
        write_holder_note(&holder);

        let waiter = open_lock_file(&path).unwrap();
        let description = holder_description(&waiter);
        assert!(
            description.contains(&std::process::id().to_string()),
            "the waiter is told who holds it, got: {description}"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
