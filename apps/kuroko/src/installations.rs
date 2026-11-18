//! The bridge from an OAuth [`Installation`] to the store's row.

use kuroko_linear::oauth::Installation;
use kuroko_store::{Store, StoredInstallation};

use crate::router::Installations;

impl Installations for Store {
    fn save(&self, installation: &Installation) -> Result<(), String> {
        // The store keeps epoch milliseconds as a raw integer so persistence does
        // not depend on the domain clock; the conversion stays here at the edge.
        let row = StoredInstallation {
            app_user_id: installation.app_user_id.clone(),
            access_token: installation.access_token.clone(),
            refresh_token: installation.refresh_token.clone(),
            expires_at: installation.expires_at.0,
            scope: installation.scopes.to_string(),
            installed_at: installation.installed_at.0,
        };
        Self::save_installation(self, &row).map_err(|error| error.to_string())
    }
}
