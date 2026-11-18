//! The bridge between an OAuth [`Installation`] and the store's row.

use kuroko_core::Millis;
use kuroko_linear::oauth::{Installation, Scopes, Token};
use kuroko_store::{Store, StoredInstallation};
use secrecy::SecretString;

use crate::router::Installations;

impl Installations for Store {
    fn save(&self, installation: &Installation) -> Result<(), String> {
        // The store keeps epoch milliseconds as a raw integer so persistence does
        // not depend on the domain clock; the conversion stays here at the edge.
        let row = StoredInstallation {
            app_user_id: installation.app_user_id.clone(),
            access_token: SecretString::from(installation.access_token.expose().to_owned()),
            refresh_token: installation
                .refresh_token
                .as_ref()
                .map(|token| SecretString::from(token.expose().to_owned())),
            expires_at: installation.expires_at.0,
            scope: installation.scopes.to_string(),
            installed_at: installation.installed_at.0,
        };
        Self::save_installation(self, &row).map_err(|error| error.to_string())
    }
}

/// Reads a stored row back into the domain type.
///
/// A free function rather than a `From` impl because both types live in other
/// crates that do not depend on each other, and this conversion belongs at the
/// daemon's edge either way.
#[must_use]
pub fn restore(row: StoredInstallation) -> Installation {
    use secrecy::ExposeSecret as _;

    Installation {
        app_user_id: row.app_user_id,
        access_token: Token::new(row.access_token.expose_secret()),
        refresh_token: row
            .refresh_token
            .as_ref()
            .map(|token| Token::new(token.expose_secret())),
        expires_at: Millis(row.expires_at),
        scopes: Scopes::parse(&row.scope),
        installed_at: Millis(row.installed_at),
    }
}
