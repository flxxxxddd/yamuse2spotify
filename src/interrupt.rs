//! turning ctrl-c into "stop and keep the progress".
//!
//! without this a signal kills the process where it stands, and everything
//! since the last flush is gone — which, for a phase that spends nine seconds
//! per track, is most of the run. the handler sets a flag instead; the phase
//! loops notice it, stop at the next item, and fall out through the same path
//! as the "abort" answer, which already saves the journal and the cache.

use std::sync::atomic::{AtomicBool, Ordering};

/// set once a signal has been seen.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// start watching for ctrl-c.
///
/// the second one exits immediately: if the first is being honoured and the
/// flush is somehow stuck, a user pressing it again means it, and at that point
/// the journal on disk is still whatever the last flush wrote.
pub fn install() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            REQUESTED.store(true, Ordering::SeqCst);
            eprintln!("\n  останавливаюсь и сохраняю прогресс… (ещё раз ctrl-c — выйти сразу)");
        }

        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n  выхожу немедленно");
            std::process::exit(130);
        }
    });
}

/// whether a stop has been asked for.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_requested_until_a_signal_arrives() {
        // the flag is process-global, and this is the only test that reads it
        // before anything could have set it.
        assert!(!requested());
    }
}
