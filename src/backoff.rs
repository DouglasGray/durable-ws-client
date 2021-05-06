use std::time::Duration;

/// Trait for creating different backoff/timeout routines. Required
/// when reconnecting to avoid hitting the remote peer too many times
/// with requests.
///
/// When a reconnect attempt is about to start, `next_timeout` will be
/// called. The `Duration` returned determines how long the client
/// will wait before trying to reconnect.
///
/// Once a connection is successfully opened, `reset` will be called.
/// This is an opportunity to reset the routine.
pub trait BackoffGenerator {
    fn reset(&mut self);
    fn next_delay(&mut self) -> Duration;
}

/// Always returns `backoff_duration` as the next timeout.
#[derive(Clone)]
pub struct FixedBackoff {
    backoff_duration: Duration,
}

impl FixedBackoff {
    pub fn new(backoff_duration: Duration) -> Self {
        FixedBackoff { backoff_duration }
    }
}

impl BackoffGenerator for FixedBackoff {
    fn reset(&mut self) {}
    fn next_delay(&mut self) -> Duration {
        self.backoff_duration
    }
}
