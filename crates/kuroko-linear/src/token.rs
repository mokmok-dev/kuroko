//! The token lifecycle: what the daemon holds and when it must be renewed.
//!
//! The rules live here rather than beside the transport so a mutation run
//! reaches them. Everything in this module is a pure function of the slot and
//! the clock; the daemon owns the lock, the exchange, and the store.

use kuroko_core::Millis;

use crate::oauth::{Installation, REFRESH_SKEW, needs_refresh};

/// What the daemon knows about its installation right now.
///
/// `Refreshing` carries the pair being exchanged so the answer to "what is
/// installed" stays true while a renewal is in flight, and so no caller has to
/// keep a copy of the pair to restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    /// Nothing is installed, or Linear withdrew the grant.
    Missing,
    /// A pair that may be close enough to expiry to renew.
    Fresh(Installation),
    /// One caller is exchanging this pair's refresh token. A second caller that
    /// starts its own exchange would present a refresh token Linear already
    /// rotated, which revokes the result of the first.
    Refreshing(Installation),
}

/// What the caller that looked at a [`Slot`] must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// No credential can be produced until the operator installs again.
    Absent,
    /// This access token is good for the call.
    Use(Installation),
    /// This pair's refresh token must be exchanged, and the caller that received
    /// this step owns that exchange.
    Refresh(Installation),
    /// Another caller owns the exchange; wait for the slot to change.
    Wait,
}

impl Slot {
    /// Decides what the caller must do at `now`, recording the exchange when one
    /// is due so no later caller starts a second.
    ///
    /// The three rules are the whole policy: a missing slot has nothing to try, a
    /// refreshing one is somebody else's business, and a fresh one is used until
    /// it comes within [`REFRESH_SKEW`] of expiry. A clock that jumps past the
    /// expiry meets the same comparison, so it needs no case of its own.
    pub fn step(&mut self, now: Millis) -> Step {
        match self {
            Self::Missing => Step::Absent,
            Self::Refreshing(_) => Step::Wait,
            Self::Fresh(installation) => {
                if !needs_refresh(installation.expires_at, now, REFRESH_SKEW) {
                    return Step::Use(installation.clone());
                }
                let refreshing = Self::Refreshing(installation.clone());
                let due = Step::Refresh(installation.clone());
                *self = refreshing;
                due
            }
        }
    }

    /// Records a pair, which a finished exchange, a restored failure, or a new
    /// install produced.
    pub fn store(&mut self, installation: Installation) {
        *self = Self::Fresh(installation);
    }

    /// Records that Linear withdrew the grant. Only a new install clears this.
    pub fn revoked(&mut self) {
        *self = Self::Missing;
    }
}

impl From<Option<Installation>> for Slot {
    fn from(installation: Option<Installation>) -> Self {
        installation.map_or(Self::Missing, Self::Fresh)
    }
}
