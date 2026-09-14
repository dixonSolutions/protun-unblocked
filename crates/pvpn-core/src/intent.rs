//! Whether the user asked for the tunnel to be *down*.
//!
//! Nothing in the CLI reads this. `pvpn up`, `down` and `hop` behave exactly
//! as they always have; the marker exists for the opt-in resume hook
//! (`docs/always-on.md`), which must not undo a deliberate `pvpn down` just
//! because the machine woke up.
//!
//! The direction matters. The deleted daemon persisted `want_up`, so a reboot
//! resumed a fight the user had already lost interest in. This persists the
//! opposite: it can only ever cause *less* to happen. Nothing reconnects
//! because of this file, and the worst a stale one can do is make you type
//! `pvpn up`.
//!
//! Every operation here is best-effort. A marker that could not be written
//! costs one unwanted reconnect; an error returned into a disconnect path
//! would cost a working `pvpn down`, so these swallow their errors.

use std::path::{Path, PathBuf};

/// The file whose presence means "the user turned this off".
pub fn down_marker(data_dir: &Path) -> PathBuf {
    data_dir.join("down-by-user")
}

/// Record that the user asked for the tunnel to go down.
pub fn mark_down(data_dir: &Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(down_marker(data_dir), b"");
}

/// Forget it — the user asked for a tunnel again.
pub fn clear_down(data_dir: &Path) {
    let _ = std::fs::remove_file(down_marker(data_dir));
}

/// Did the user turn this off and not turn it back on?
pub fn is_down_by_user(data_dir: &Path) -> bool {
    down_marker(data_dir).exists()
}

/// The file whose presence means "never rebuild the tunnel for me".
///
/// Written by `pvpn-autoconnect --off`, and a stronger statement than
/// [`down_marker`]: that one says "off right now", this one says "off, and
/// stop offering". Lives in the *config* directory rather than the data one
/// because it is a preference, not an observation.
pub fn autoconnect_off_marker(config_dir: &Path) -> PathBuf {
    config_dir.join("autoconnect-off")
}

/// Has the user opted out of anything reconnecting on their behalf?
///
/// Read by the health check before it acts, never before it *looks*. Opting
/// out of automatic reconnects is not opting out of being told the tunnel is
/// dead — suppressing the diagnosis along with the cure is how someone ends
/// up staring at a `Connected` that has meant nothing for ten minutes.
pub fn autoconnect_is_off(config_dir: &Path) -> bool {
    autoconnect_off_marker(config_dir).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pvpn-intent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn absent_by_default() {
        let dir = tmp();
        assert!(!is_down_by_user(&dir));
        assert!(!autoconnect_is_off(&dir));
    }

    /// Two different questions, two different files. Turning the tunnel off
    /// for now must not read as opting out of reconnects forever.
    #[test]
    fn down_by_user_is_not_autoconnect_off() {
        let dir = tmp();
        mark_down(&dir);
        assert!(is_down_by_user(&dir));
        assert!(
            !autoconnect_is_off(&dir),
            "`pvpn down` must not read as `pvpn-autoconnect --off`"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autoconnect_off_is_seen_when_the_file_is_there() {
        let dir = tmp();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(autoconnect_off_marker(&dir), b"");
        assert!(autoconnect_is_off(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_then_clear_round_trips() {
        let dir = tmp();
        mark_down(&dir);
        assert!(is_down_by_user(&dir), "down should stick after mark_down");
        clear_down(&dir);
        assert!(!is_down_by_user(&dir), "up should clear the marker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marking_twice_is_not_an_error() {
        let dir = tmp();
        mark_down(&dir);
        mark_down(&dir);
        assert!(is_down_by_user(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_an_absent_marker_is_not_an_error() {
        let dir = tmp();
        clear_down(&dir);
        clear_down(&dir);
        assert!(!is_down_by_user(&dir));
    }

    /// The marker must never be what decides a tunnel exists — only that the
    /// user last said "off". A directory that cannot be created leaves the
    /// answer "no", which means the hook reconnects rather than refusing to.
    #[test]
    fn unwritable_dir_reads_as_not_down() {
        let dir = PathBuf::from("/proc/nonexistent-pvpn-intent");
        mark_down(&dir);
        assert!(!is_down_by_user(&dir));
    }
}
