use std::time::Duration;

/// Trait for creating different backoff/timeout routines. Required
/// when reconnecting to avoid hitting the server too many times with
/// requests.
///
/// When a reconnect attempt is about to start, `next_delay` will be
/// called. The [`Duration`] returned determines how long the client
/// will wait before trying to reconnect.
///
/// Once a connection is successfully opened, `reset` will be called.
/// This is an opportunity to reset the routine.
///
/// [`Duration`]: std::time::Duration
pub trait BackoffGenerator {
    fn reset(&mut self);
    fn next_delay(&mut self) -> Duration;
}

/// A fixed backoff which always returns the same [`Duration`].
///
/// [`Duration`]: std::time::Duration
#[derive(Clone)]
pub struct FixedBackoff {
    backoff_duration: Duration,
}

impl FixedBackoff {
    /// Creates a new instance. The passed `backoff_duration` will be
    /// returned on every call to [`next_delay`].
    ///
    /// [`next_delay`]: BackoffGenerator::next_delay
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
