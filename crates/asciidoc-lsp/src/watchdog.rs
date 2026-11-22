//! Ends the server when the client process dies.
//!
//! A dead client does not always close the server's stdin: while another
//! process holds the pipe open, the read never returns and the server stays
//! behind. The watchdog polls the client's process id from `initialize` on
//! its own OS thread, so it fires even while a handler blocks the serve
//! future. Unix only; elsewhere the server still ends when stdin closes.

#[cfg(unix)]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Starts watching the client process `pid` and exits the server once it
/// is gone.
pub fn watch(pid: u32) {
    #[cfg(unix)]
    {
        // `kill` reads 0 and negative values as process groups.
        let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0) else {
            tracing::debug!(
                pid,
                "processId is not a usable process id, no client watchdog runs"
            );
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("client-watchdog".into())
            .spawn(move || {
                while is_alive(pid) {
                    std::thread::sleep(POLL_INTERVAL);
                }
                tracing::warn!(pid, "client process is gone, exiting");
                std::process::exit(0);
            });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not start the client watchdog");
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn is_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 sends nothing; `kill` only checks that the process
    // exists and may be signalled.
    let rc = unsafe { libc::kill(pid, 0) };
    // EPERM: the process exists but belongs to another user.
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        let pid = libc::pid_t::try_from(std::process::id()).unwrap();
        assert!(is_alive(pid));
    }

    #[test]
    fn reaped_child_is_dead() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        child.wait().unwrap();
        assert!(!is_alive(pid));
    }
}
