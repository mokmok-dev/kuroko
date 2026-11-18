//! The `reqwest` client behind [`kuroko_linear::oauth::HttpPost`].

use std::time::Duration;

use kuroko_linear::oauth::{BoxFuture, HttpError, HttpPost, HttpResponse, PostRequest};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

/// Sends the two OAuth install requests over HTTPS.
///
/// Built once and cloned into `AppState`: a `reqwest::Client` owns a connection
/// pool, so building one per install would re-handshake on every attempt.
#[derive(Clone, Debug)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    /// Builds a client that gives up on each request after `timeout`.
    ///
    /// # Errors
    ///
    /// Returns the builder error when the TLS backend cannot be initialised.
    pub fn new(timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
        })
    }
}

impl HttpPost for Http {
    fn post<'a>(
        &'a self,
        request: PostRequest<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let mut builder = self
                .client
                .post(request.url)
                .header(CONTENT_TYPE, request.content_type)
                .body(request.body.to_vec());
            if let Some(authorization) = request.authorization {
                builder = builder.header(AUTHORIZATION, authorization);
            }
            let response = builder.send().await.map_err(|error| classify(&error))?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|error| classify(&error))?;
            Ok(HttpResponse {
                status,
                body: body.to_vec(),
            })
        })
    }
}

/// Maps a `reqwest` failure onto the two cases the install tells apart.
fn classify(error: &reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Other(error.to_string())
    }
}
