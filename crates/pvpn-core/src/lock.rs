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

/// Marks a holder nobody is waiting on at a terminal.
const BACKGROUND_TAG: &str = "[background]";

/// Is this process working for nobody in particular — `pvpn-autoconnect`
/// after a resume or a roam — rather than for someone who typed a command?
pub fn running_in_background() -> bool {
    std::env::var_os("PVPN_AUTOCONNECT").is_some_and(|v| !v.is_empty())
}

/// Record who we are for anyone who comes waiting. Best-effort: the lock
/// is the lock, the note is a courtesy.
fn write_holder_note(file: &File) {
    let mut file = file;
    let args: Vec<String> = std::env::args().take(3).collect();
    let mut note = format!("pid {}: {}", std::process::id(), args.join(" "));
    if running_in_background() {
        note.push(' ');
        note.push_str(BACKGROUND_TAG);
    }
    if file.seek(SeekFrom::Start(0)).is_ok() {
        let _ = file.set_len(0);
        let _ = file.write_all(note.as_bytes());
    }
}

/// Who is connecting right now, if anyone: the holder's pid and its note.
///
/// For watchers — the GUI — that must never take the lock to find out,
/// because a watcher holding it for even a moment is a connect that waits a
/// second for no reason. So this reads `/proc/locks` for a lock on the lock
/// file's inode instead of trying one.
pub fn in_flight() -> Option<(u32, String)> {
    let path = lock_path();
    let pid = locked_by(&path)?;
    let note = std::fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    Some((pid, note))
}

/// The pid holding any lock on `path`, from `/proc/locks`, without taking
/// one. Also how the GUI sees `pvpn-autoconnect` running: that script
/// holds its own flock for as long as it works.
pub fn locked_by(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(path).ok()?.ino();
    let locks = std::fs::read_to_string("/proc/locks").ok()?;
    holder_pid(&locks, inode)
}

/// The pid holding a lock on `inode` in `/proc/locks` text. Lines marked
/// `->` are waiters, not holders.
fn holder_pid(locks: &str, inode: u64) -> Option<u32> {
    locks.lines().filter(|l| !l.contains("->")).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // "1: FLOCK  ADVISORY  WRITE 12345 fd:01:1234567 0 EOF"
        let pid = fields.get(4)?.parse().ok()?;
        let ino: u64 = fields.get(5)?.rsplit(':').next()?.parse().ok()?;
        (ino == inode).then_some(pid)
    })
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
    if !running_in_background() && holder.contains(BACKGROUND_TAG) && preempt(&file, &holder).await {
        write_holder_note(&file);
        return ConnectGuard { _file: Some(file) };
    }
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

/// Take the lock from whoever holds it — `--force`.
///
/// For when the command typed now matters more than the one already
/// running, whoever started it: `pvpn up --force` over a `pvpn down` still
/// tearing down, `pvpn hop --force` over a connect that is still grinding
/// through its settle window in another terminal. The holder is asked to
/// step aside exactly as a background connect is — SIGTERM, which runs its
/// own restore — and only if it is still holding the lock after
/// [`PREEMPT_PATIENCE`] is it killed outright; the kernel then releases the
/// lock, and the connect that follows rebuilds whatever it left.
pub async fn acquire_forced() -> ConnectGuard {
    let Some(file) = open_lock_file(&lock_path()) else {
        return ConnectGuard { _file: None };
    };
    if try_acquire(&file) {
        write_holder_note(&file);
        return ConnectGuard { _file: Some(file) };
    }
    let holder = holder_description(&file);
    if preempt(&file, &holder).await {
        write_holder_note(&file);
        return ConnectGuard { _file: Some(file) };
    }
    if let Some(pid) = locked_by(&lock_path()).filter(|pid| *pid != std::process::id()) {
        tracing::warn!("{holder} did not step aside — stopping it");
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if try_acquire(&file) {
                write_holder_note(&file);
                return ConnectGuard { _file: Some(file) };
            }
        }
    }
    // Something we cannot signal holds it; wait as an ordinary command would.
    drop(file);
    acquire().await
}

/// How long a preempted background connect gets to put routing back.
const PREEMPT_PATIENCE: Duration = Duration::from_secs(15);

/// Ask a background holder to step aside, and take the lock when it has.
///
/// A connect nobody asked for must not make someone who did wait for it.
/// Measured 2026-10-14 on `wifi:detnsw`: a roam started `pvpn-autoconnect`,
/// whose `pvpn up` spent three minutes on saved profiles that could not
/// start; the `pvpn hop SG-FREE#2` typed meanwhile sat behind it until the
/// user found the pid and killed it by hand. SIGTERM runs the holder's own
/// cleanup — the same restore as Ctrl-C — so routing is put back before
/// the lock comes free. `false` when it did not step aside in time; the
/// caller then waits as it always has.
async fn preempt(file: &File, holder: &str) -> bool {
    let Some(pid) = locked_by(&lock_path()) else {
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    tracing::info!("asking {holder} to step aside");
    let signalled = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success());
    if !signalled {
        return false;
    }
    let deadline = std::time::Instant::now() + PREEMPT_PATIENCE;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if try_acquire(file) {
            return true;
        }
    }
    false
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
    fn a_holder_is_found_in_proc_locks_text_and_waiters_are_not() {
        let text = "1: FLOCK  ADVISORY  WRITE 4242 00:1a:991 0 EOF\n\
                    1: -> FLOCK  ADVISORY  WRITE 5151 00:1a:991 0 EOF\n\
                    2: POSIX  ADVISORY  WRITE 777 fd:01:12 0 EOF\n";
        assert_eq!(holder_pid(text, 991), Some(4242));
        assert_eq!(holder_pid(text, 12), Some(777));
        assert_eq!(holder_pid(text, 5), None);
    }

    /// On a private file, not the real lock: a live connect, or another
    /// test, holding the real one would make this race.
    #[test]
    fn locked_by_sees_a_holder_and_forgets_it_when_the_fd_closes() {
        let dir = std::env::temp_dir().join(format!("pvpn-lock-peek-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connect.lock");
        let holder = open_lock_file(&path).unwrap();
        assert!(try_acquire(&holder));
        assert_eq!(locked_by(&path), Some(std::process::id()));
        drop(holder);
        // Not instantly: a test thread forking a subprocess holds a copy of
        // every fd until the child execs, and a flock lives as long as any
        // copy does.
        let gone = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            locked_by(&path).is_none()
        });
        assert!(gone, "gone once the fd closes");
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
