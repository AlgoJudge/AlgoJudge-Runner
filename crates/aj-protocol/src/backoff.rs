//! How often to ask when the answer keeps being "nothing".
//!
//! The Server has no push for Runners, deliberately: the queue is polled and an
//! empty one answers `204`. That makes the polling interval a real design
//! decision rather than an implementation detail, because it is the whole of
//! the latency between a participant submitting and a Runner noticing.
//!
//! The shape is exponential from one second to thirty, with jitter, reset to
//! the floor on any successful claim. Three things justify each part:
//!
//! - **One second at the floor**, because a busy queue should drain at the
//!   speed of evaluation, not at the speed of polling.
//! - **Thirty seconds at the ceiling**, because an idle installation then costs
//!   two requests a minute per Runner, and because rounds already open on a
//!   scan of roughly fifteen seconds — a slower Runner would not be the reason
//!   anybody waited.
//! - **Jitter**, because without it a room full of Runners that started
//!   together stays synchronised for ever and asks in one burst.

use std::time::Duration;

use rand_core::RngCore as _;

pub struct Backoff {
    min: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        let min = min.max(Duration::from_millis(100));
        Self {
            min,
            max: max.max(min),
            current: min,
        }
    }

    /// There was work. Ask again immediately next time.
    pub fn reset(&mut self) {
        self.current = self.min;
    }

    /// The next interval, jittered by ±20 %, doubling the underlying one.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.current;
        self.current = (self.current * 2).min(self.max);
        jitter(base)
    }

    pub async fn wait(&mut self) {
        let delay = self.next_delay();
        tracing::trace!(?delay, "the queue was empty");
        tokio::time::sleep(delay).await;
    }
}

fn jitter(base: Duration) -> Duration {
    let millis = base.as_millis() as u64;
    let spread = millis / 5; // ±20 %
    if spread == 0 {
        return base;
    }
    let offset = (rand_core::OsRng.next_u32() as u64) % (spread * 2 + 1);
    Duration::from_millis(millis + offset - spread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_doubles_up_to_the_ceiling_and_stops_there() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));

        // Jitter makes each draw approximate, so the assertion is on the band
        // rather than on a number.
        for expected in [1u64, 2, 4, 8, 16, 30, 30, 30] {
            let delay = backoff.next_delay().as_millis() as u64;
            let floor = expected * 800;
            let ceiling = expected * 1200;
            assert!(
                (floor..=ceiling).contains(&delay),
                "{delay} ms is outside {floor}..={ceiling} for an interval around {expected} s",
            );
        }
    }

    #[test]
    fn work_puts_it_back_on_the_floor() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        for _ in 0..6 {
            backoff.next_delay();
        }

        backoff.reset();

        let delay = backoff.next_delay().as_millis() as u64;
        assert!(
            (800..=1200).contains(&delay),
            "{delay} ms is not around a second"
        );
    }

    #[test]
    fn a_ceiling_below_the_floor_is_not_a_way_to_poll_in_a_loop() {
        let mut backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(1));
        assert!(backoff.next_delay() >= Duration::from_secs(4));
    }
}
