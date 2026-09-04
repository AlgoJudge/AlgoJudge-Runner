//! Being told to stop, and hearing it everywhere at once.
//!
//! **Here rather than in one Runner**, because both of them need it and neither
//! is more entitled to it: a Runner waiting on a container and an External
//! Runner waiting on somebody else's archive are stopped the same way and owe
//! the queue the same thing.
//!
//! **A Runner that is stopped in the middle of a job gives that job back.** The
//! alternative — and what happened before this existed — is going quiet and
//! letting the lease expire, which costs a participant up to ten minutes while
//! idle Runners wait for a deadline nobody is going to miss.
//!
//! The signal has to reach two places: the poll, so an idle Runner exits at
//! once rather than after its backoff, and the evaluation, so a busy one stops
//! rather than finishing work whose result nobody is waiting for. Both are
//! `select!` arms against [`Stopping::wait`].

use std::sync::Arc;

/// The word from outside that this Runner is to stop.
///
/// Cloneable and cheap: every place that has to notice holds one.
#[derive(Clone)]
pub struct Stopping {
    /// **Held here as well as by the listener**, so the channel never closes.
    /// A receiver whose sender is gone reports a change that never comes, and
    /// a Runner must not read that as having been told to stop.
    _tell: Arc<tokio::sync::watch::Sender<bool>>,
    told: tokio::sync::watch::Receiver<bool>,
}

impl Stopping {
    /// Starts listening, and answers before anything has been heard.
    ///
    /// **`SIGTERM` is what a container runtime sends**, and it is the one that
    /// matters: `docker stop`, `compose down` and an operator's `systemctl` all
    /// arrive that way. Ctrl-C is the same thing from a developer's terminal.
    ///
    /// A second signal is not caught, deliberately. The default disposition
    /// takes the process down immediately, which is exactly what somebody
    /// sending it a second time is asking for.
    pub fn listen() -> Self {
        let (tell, told) = tokio::sync::watch::channel(false);
        let tell = Arc::new(tell);

        let listener = Arc::clone(&tell);
        tokio::spawn(async move {
            wait_for_a_signal().await;
            tracing::info!("told to stop; the job in hand goes back to the queue");
            let _ = listener.send(true);
        });

        Self { _tell: tell, told }
    }

    /// Whether the word has already come.
    pub fn now(&self) -> bool {
        *self.told.borrow()
    }

    /// Resolves when it comes, and stays resolved afterwards.
    ///
    /// Takes `&self` and clones the receiver, so a caller holding one handle can
    /// wait in several `select!`s without threading a mutable borrow through
    /// every one of them.
    pub async fn wait(&self) {
        let mut told = self.told.clone();
        while !*told.borrow_and_update() {
            if told.changed().await.is_err() {
                // Cannot happen while `_tell` is held, and if it somehow did,
                // waiting for ever is the safe reading: nobody said stop.
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_a_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(e) => {
            // Nothing else can be done, and a Runner that cannot hear `SIGTERM`
            // is killed by it, which is where every Runner started.
            tracing::warn!(%e, "cannot listen for SIGTERM; a stop will be a kill");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// The Runner ships as a Linux container; this is for `cargo test` on a
/// developer's machine, where Ctrl-C is the only signal there is.
#[cfg(not(unix))]
async fn wait_for_a_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Nothing has been said until something is said.** A Runner that read a
    /// fresh handle as a stop would exit before claiming anything.
    #[tokio::test]
    async fn a_fresh_one_has_heard_nothing() {
        let stopping = Stopping::listen();
        assert!(!stopping.now());

        // And waiting on it does not resolve on its own.
        tokio::select! {
            _ = stopping.wait() => panic!("it resolved with nothing said"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }

    /// Every holder hears it, and hears it however late it starts listening.
    #[tokio::test]
    async fn the_word_reaches_a_handle_that_was_not_waiting_yet() {
        let (tell, told) = tokio::sync::watch::channel(false);
        let stopping = Stopping {
            _tell: Arc::new(tell),
            told,
        };
        let elsewhere = stopping.clone();

        stopping._tell.send(true).expect("the channel is open");

        assert!(stopping.now());
        assert!(elsewhere.now());
        // Resolves immediately, rather than waiting for a change that has been
        // and gone.
        tokio::time::timeout(std::time::Duration::from_millis(50), elsewhere.wait())
            .await
            .expect("it had already been said");
    }
}
