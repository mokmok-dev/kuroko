//! The wall clock the daemon stamps tasks with.

use kuroko_core::{Clock, Millis};

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> Millis {
        // System time can precede the epoch, so an unreadable clock reads as the epoch.
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        Millis(i64::try_from(since_epoch).unwrap_or(i64::MAX))
    }
}
