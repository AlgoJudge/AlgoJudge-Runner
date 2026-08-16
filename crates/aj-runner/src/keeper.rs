//! Holding a lease for as long as the work it covers takes.
//!
//! **The lease is a correctness mechanism, not a safety net.** `LeaseReaper`
//! gives a job back the moment its deadline passes, and a Runner still computing
//! then shares that job with whoever claims it next.
//!
//! Nothing here renewed until this module existed. The loop asked for ten
//! minutes at claim and never mentioned the lease again, which is right for
//! every package anyone has built so far and wrong in a way that would be very
//! hard to recognise. An evaluation is a sum of bounded steps — sixty seconds to
//! compile, a limit and a second per test, thirty for a checker — but **nothing
//! bounds the number of tests**, so the sum is not bounded either. A package
//! whose tests genuinely take longer than one lease is not judged slowly: it is
//! delivered, expires, is delivered again, and after the fifth delivery the
//! Server fails it with *"Given up after 5 deliveries without a result"*. A
//! submission that would judge perfectly well reads as an infrastructure fault,
//! roughly an hour later.
//!
//! The policy is deliberately dull, for the same reason it is elsewhere in the
//! product: **renew on a timer, unconditionally, for as long as the work runs.**
//! Renewing never shortens a lease — the Server keeps the later of the two
//! deadlines, and its conformance suite pins that — so there is no arithmetic
//! here to get wrong and no second opinion about when the deadline is.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aj_protocol::Server;

use crate::config::Config;

/// Renews one lease in the background until it is dropped.
///
/// Held by whoever holds the work. There is deliberately no way to ask it
/// anything: the question it could answer — "is the lease still ours" — is
/// answered better by the next call against that lease, which refuses with
/// `runner.lease.stale` rather than with an opinion this task formed some
/// seconds ago.
pub struct Keeper {
    task: tokio::task::JoinHandle<()>,
}

impl Keeper {
    /// Holds a **job's** lease while it is being judged.
    pub fn hold_job(
        server: Arc<Server>,
        job_id: String,
        lease_token: String,
        config: &Config,
    ) -> Self {
        let seconds = config.lease_seconds;
        let logged = job_id.clone();

        Self::spawn("job", logged, every(config.lease_granted()), move || {
            // Cloned per attempt rather than borrowed, so each future owns
            // everything it needs and nothing is tied to this call.
            let server = Arc::clone(&server);
            let job_id = job_id.clone();
            let lease_token = lease_token.clone();

            async move {
                server
                    .renew(&job_id, &lease_token, Some(seconds))
                    .await
                    .map(|lease| lease.lease_expires_at)
            }
        })
    }

    /// Holds a **trial's** lease. Same deadline, same reason, its own path.
    ///
    /// A trial measures a package's own model solutions against every test in
    /// it, which makes it the one kind of work reliably *slower* than judging a
    /// submission against the same package.
    pub fn hold_trial(
        server: Arc<Server>,
        trial_id: String,
        lease_token: String,
        config: &Config,
    ) -> Self {
        let seconds = config.lease_seconds;
        let logged = trial_id.clone();

        Self::spawn("trial", logged, every(config.lease_granted()), move || {
            let server = Arc::clone(&server);
            let trial_id = trial_id.clone();
            let lease_token = lease_token.clone();

            async move {
                server
                    .renew_trial(&trial_id, &lease_token, Some(seconds))
                    .await
                    .map(|lease| lease.lease_expires_at)
            }
        })
    }

    /// The loop both of the above run, with the endpoint left to the caller so
    /// that a test can drive it without a Server.
    fn spawn<F, Fut>(kind: &'static str, id: String, interval: Duration, renew: F) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, aj_protocol::Error>> + Send + 'static,
    {
        let task = tokio::spawn(async move {
            loop {
                // **After the interval, not before it.** The claim that started
                // this work has just granted a full lease; renewing immediately
                // would spend a request saying so.
                tokio::time::sleep(interval).await;

                match renew().await {
                    Ok(until) => tracing::debug!(kind, %id, %until, "the lease was renewed"),

                    // Somebody else has the work now. Nothing being computed for
                    // it will be wanted, and renewing again cannot change that —
                    // so the task ends rather than repeating one refusal every
                    // interval for the rest of the evaluation.
                    Err(e) if e.lease_lost() => {
                        tracing::warn!(%e, kind, %id, "the lease is gone; another Runner has this");
                        return;
                    }

                    // Anything else is transient by assumption, and one missed
                    // renewal is not a lost lease: the interval is a quarter of
                    // the deadline, so two in a row can fail with the work still
                    // safely held.
                    Err(e) => tracing::warn!(%e, kind, %id, "the lease was not renewed"),
                }
            }
        });

        Self { task }
    }
}

impl Drop for Keeper {
    /// **Renewal stops with the work, however the work stopped.**
    ///
    /// Written as a `Drop` rather than as a call at the end of the happy path
    /// because the paths that would skip such a call are exactly the ones that
    /// must not leak a task: an early return when the Server went away, and a
    /// panic inside the pipeline.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How often to renew, given the lease the Server actually granted.
///
/// A quarter of it. Three renewals fit inside every lease, so two may fail in a
/// row with the deadline still comfortably ahead — and the cost of being early
/// is one request, while the cost of being late is somebody's submission judged
/// twice or given up on.
///
/// The floor is for a caller that passes nothing sensible; a real configuration
/// cannot reach it, because the granted lease is itself floored at a minute.
pub fn every(granted: Duration) -> Duration {
    (granted / 4).max(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use aj_protocol::error::Refusal;

    fn refused(status: u16, code: &str) -> aj_protocol::Error {
        aj_protocol::Error::Refused {
            status,
            refusal: Box::new(Refusal {
                code: Some(code.to_owned()),
                ..Default::default()
            }),
            retry_after: None,
        }
    }

    /// Three renewals inside every lease, so two may fail in a row.
    #[test]
    fn a_lease_is_renewed_well_before_it_would_expire() {
        assert_eq!(every(Duration::from_secs(600)), Duration::from_secs(150));

        // The margin, stated as the property that matters rather than as the
        // number that happens to satisfy it today.
        let granted = Duration::from_secs(600);
        assert!(
            every(granted) * 3 < granted,
            "three intervals must fit inside the lease they are protecting",
        );
    }

    /// The whole point: work that outlasts a lease still keeps the job.
    #[tokio::test(start_paused = true)]
    async fn a_lease_is_renewed_for_as_long_as_the_work_runs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);

        let _keeper = Keeper::spawn("job", "j-1".into(), Duration::from_secs(150), move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok("2026-08-16T12:00:00Z".to_owned())
            }
        });

        // Ten minutes of work against a lease of ten minutes: the case that used
        // to be delivered five times and then failed.
        tokio::time::sleep(Duration::from_secs(605)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "the lease was not renewed while the work was still running",
        );
    }

    /// A refusal meaning somebody else has the job ends the renewing.
    #[tokio::test(start_paused = true)]
    async fn a_lost_lease_stops_the_renewing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);

        let _keeper = Keeper::spawn("job", "j-2".into(), Duration::from_secs(150), move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Err::<String, _>(refused(409, "runner.lease.stale"))
            }
        });

        tokio::time::sleep(Duration::from_secs(605)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "renewing carried on against a lease that had already been reclaimed",
        );
    }

    /// A Server that cannot be reached is not a lease that has been lost, and
    /// that difference is the whole reason a missed renewal is survivable.
    #[tokio::test(start_paused = true)]
    async fn an_unreachable_server_does_not_give_the_work_up() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);

        let _keeper = Keeper::spawn("job", "j-3".into(), Duration::from_secs(150), move || {
            let counted = Arc::clone(&counted);
            async move {
                // Away for the first two attempts, back for the rest.
                let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 2 {
                    Err(refused(503, "server.maintenance"))
                } else {
                    Ok("2026-08-16T12:00:00Z".to_owned())
                }
            }
        });

        tokio::time::sleep(Duration::from_secs(605)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "two failed renewals ended the holding, which is what the margin exists to prevent",
        );
    }

    /// Nothing renews a lease for work that has finished.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_keeper_stops_the_renewing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);

        let keeper = Keeper::spawn("job", "j-4".into(), Duration::from_secs(150), move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok("2026-08-16T12:00:00Z".to_owned())
            }
        });

        tokio::time::sleep(Duration::from_secs(305)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        drop(keeper);
        tokio::time::sleep(Duration::from_secs(605)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a lease was still being renewed after the work holding it had gone",
        );
    }
}
