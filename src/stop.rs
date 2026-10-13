//! Stop requests: `SIGTERM`, as systemd sends it, or `SIGINT`, from a
//! terminal, asks a run to stop.
//!
//! The handler only sets a flag. The run stops queueing move units once it is
//! set, and each move polls it between the chunks of a copy, so a move that
//! is copying aborts, removing its temp file, and one that is retiring
//! finishes. A run that stops leaves nothing that recovery has to resolve.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request(_signal: libc::c_int) {
    REQUESTED.store(true, Ordering::Relaxed);
}

/// Makes `SIGTERM` and `SIGINT` request a stop instead of terminating cachy.
pub fn install() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe. SA_RESTART keeps the signal from failing
        // system calls that it interrupts with EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = request as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Whether a stop was requested.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}
