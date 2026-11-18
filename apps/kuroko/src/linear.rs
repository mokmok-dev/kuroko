//! The access token the daemon presents to Linear, renewed before it expires.
//!
//! Linear access tokens last 24 hours and a refresh token rotates on every use,
//! so two callers cannot both spend one. [`Slot`] decides who owns an exchange;
//! this module owns the lock, the HTTP call, and the store around that decision.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kuroko_core::Clock;
use kuroko_linear::oauth::{HttpPost, Installation, OauthClient, RefreshError};
use kuroko_linear::token::{Slot, Step};
use tokio::sync::watch;

use crate::router::Installations;

/// Holds the installation and produces a live access token from it.
pub struct TokenProvider {
    client: OauthClient,
    http: Arc<dyn HttpPost>,
    store: Arc<dyn Installations>,
    clock: Arc<dyn Clock>,
    slot: Mutex<Slot>,
    /// Bumped after every transition settles. Waiters park here rather than on
    /// the slot itself, so a change made between a caller's check and its wait
    /// is still delivered.
    changes: watch::Sender<u64>,
}

impl TokenProvider {
    #[must_use]
    pub fn new(
        client: OauthClient,
        http: Arc<dyn HttpPost>,
        store: Arc<dyn Installations>,
        clock: Arc<dyn Clock>,
        installation: Option<Installation>,
    ) -> Self {
        Self {
            client,
            http,
            store,
            clock,
            slot: Mutex::new(Slot::from(installation)),
            changes: watch::channel(0).0,
        }
    }

    /// A pair whose access token can be sent at this instant, renewing it first
    /// when it is inside the skew.
    ///
    /// # Errors
    ///
    /// Returns [`TokenFailure::NotInstalled`] when nothing is installed or the
    /// grant was withdrawn, and a [`TokenFailure::Transient`] or
    /// [`TokenFailure::NotSaved`] when the exchange did not settle, in which case
    /// the stored pair stays in place and a later call retries.
    pub async fn access_token(&self) -> Result<Installation, TokenFailure> {
        loop {
            let mut changes = self.changes.subscribe();
            // Bound to a local so the guard drops before any await: a `match` on a
            // temporary would hold the lock across the exchange and deadlock.
            let step = {
                let now = self.clock.now_ms();
                self.slot().step(now)
            };
            match step {
                Step::Absent => return Err(TokenFailure::NotInstalled),
                Step::Use(pair) => return Ok(pair),
                Step::Refresh(pair) => return self.refresh(pair).await,
                Step::Wait => {
                    // The sender lives beside this receiver, so the channel cannot
                    // close while `self` is borrowed.
                    let _ = changes.changed().await;
                }
            }
        }
    }

    /// Replaces the held pair with a new install, which is the only thing that
    /// clears a withdrawn grant.
    pub fn adopt(&self, installation: Installation) {
        self.slot().store(installation);
        self.wake();
    }

    async fn refresh(&self, pair: Installation) -> Result<Installation, TokenFailure> {
        let claim = Claim::new(self, pair);
        let Some(refresh_token) = claim.pair().refresh_token.clone() else {
            // Nothing can ever renew this pair, so it is as good as withdrawn.
            claim.withdraw();
            return Err(TokenFailure::NoRefreshToken);
        };
        match self
            .client
            .refresh(self.http.as_ref(), refresh_token.expose())
            .await
        {
            Ok(token) => {
                let renewed = claim.pair().renew(&token, self.clock.now_ms());
                // Persisted before it is returned: Linear already rotated the
                // refresh token, so a crash now would lose the only usable pair.
                if let Err(error) = self.store.save(&renewed) {
                    // Keeping the claimed pair is deliberate. The stored row now
                    // holds a refresh token Linear has already spent, so a restart
                    // discovers that rather than trusting a pair that cannot work.
                    return Err(TokenFailure::NotSaved(error));
                }
                claim.commit(renewed.clone());
                Ok(renewed)
            }
            Err(RefreshError::Revoked) => {
                claim.withdraw();
                Err(TokenFailure::Withdrawn)
            }
            // The claim restores the pair as it drops, which leaves a later call
            // free to try again.
            Err(RefreshError::Transient(detail)) => Err(TokenFailure::Transient(detail)),
        }
    }

    /// Recovers a poisoned lock: the slot holds no partially written value, since
    /// every transition replaces it whole, and a panic elsewhere must not wedge
    /// every later request.
    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self) {
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

/// Owns one in-flight exchange.
///
/// Unless it is settled, dropping it puts the claimed pair back and wakes the
/// waiters. That covers a panic and a cancelled future, which would otherwise
/// leave the slot `Refreshing` with nobody left to finish it.
struct Claim<'a> {
    provider: &'a TokenProvider,
    pair: Installation,
    settled: bool,
}

impl<'a> Claim<'a> {
    const fn new(provider: &'a TokenProvider, pair: Installation) -> Self {
        Self {
            provider,
            pair,
            settled: false,
        }
    }

    const fn pair(&self) -> &Installation {
        &self.pair
    }

    /// Records the renewed pair.
    fn commit(mut self, renewed: Installation) {
        self.provider.slot().store(renewed);
        self.settled = true;
        self.provider.wake();
    }

    /// Records that the grant cannot work again.
    fn withdraw(mut self) {
        self.provider.slot().revoked();
        self.settled = true;
        self.provider.wake();
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        {
            let mut slot = self.provider.slot();
            // A new install that landed meanwhile wins: restoring the claimed pair
            // over it would throw away the operator's newest credential.
            if *slot == Slot::Refreshing(self.pair.clone()) {
                slot.store(self.pair.clone());
            }
        }
        self.provider.wake();
    }
}

/// Why no access token could be produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenFailure {
    /// Nothing is installed in this workspace, so the install flow must run.
    NotInstalled,
    /// Linear withdrew the refresh token. Only a new install recovers it.
    Withdrawn,
    /// The stored pair has no refresh token, so nothing can renew it.
    NoRefreshToken,
    /// The exchange did not finish. The stored pair stays in place and the next
    /// call retries.
    Transient(String),
    /// The renewed pair could not be persisted, so the stored one is kept.
    NotSaved(String),
}

impl fmt::Display for TokenFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("kuroko is not installed in Linear"),
            Self::Withdrawn => f.write_str("Linear withdrew the refresh token"),
            Self::NoRefreshToken => f.write_str("the installation has no refresh token"),
            Self::Transient(detail) => write!(f, "the token refresh failed: {detail}"),
            Self::NotSaved(detail) => write!(f, "the renewed token cannot be saved: {detail}"),
        }
    }
}

impl std::error::Error for TokenFailure {}
