//! Builds the configured transport.

use std::sync::Arc;

use kuroko_engine::Transport;
use kuroko_transport::{Stdio, WebSocket};

use crate::config::TransportChoice;

#[must_use]
pub fn build_transport(choice: &TransportChoice) -> Arc<dyn Transport> {
    match choice {
        TransportChoice::Stdio {
            command,
            args,
            timeout,
        } => Arc::new(Stdio {
            command: command.clone(),
            args: args.clone(),
            timeout: *timeout,
        }),
        TransportChoice::WebSocket { url, timeout } => Arc::new(WebSocket {
            url: url.clone(),
            timeout: *timeout,
        }),
    }
}
