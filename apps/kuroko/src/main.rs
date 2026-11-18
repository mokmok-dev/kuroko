//! The kuroko daemon binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use kuroko::clock::SystemClock;
use kuroko::config::{BootError, Config, DEFAULT_PATH};
use kuroko::http::Http;
use kuroko::linear::TokenProvider;
use kuroko::router::{AppState, Oauth, router};
use kuroko::transport::build_transport;
use kuroko_core::{Book, Clock};
use kuroko_engine::{EngineError, spawn};
use kuroko_linear::Gate;
use kuroko_linear::oauth::{HTTP_TIMEOUT, StateStore};
use kuroko_store::Store;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), RunError> {
    let path = config_path()?;
    let config = Config::load(&path)?;
    let store = Arc::new(Store::open(&config.store).map_err(RunError::Store)?);
    let loaded = store.load().map_err(RunError::Store)?;
    let book = Book::reconcile(config.limits, loaded.tasks, loaded.pending);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let transport = build_transport(&config.transport);
    let gate = Gate::new(
        config.secret,
        config.replay_window,
        config.triggers,
        config.template,
    );
    // Bind before the engine starts: a failed bind must not let `Resume` persist a
    // `Dispatched` row whose agent never ran, because the next boot would then strand
    // that issue as `RecoveryPending`.
    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|source| RunError::Bind {
            addr: config.bind,
            source,
        })?;
    log::info!("listening on {}", config.bind);

    let (engine, mut engine_task) = spawn(
        transport,
        book,
        Arc::clone(&clock),
        Arc::clone(&store) as Arc<dyn kuroko_core::Journal>,
    );
    let engine = Arc::new(engine);

    let state = AppState::new(gate, Arc::clone(&engine), Arc::clone(&clock))
        .with_store(Arc::clone(&store) as Arc<dyn kuroko::router::Installations>);
    let state = match config.oauth {
        Some(oauth) => {
            let http = Http::new(HTTP_TIMEOUT).map_err(|source| RunError::Http {
                source: source.to_string(),
            })?;
            let http: Arc<dyn kuroko_linear::oauth::HttpPost> = Arc::new(http);
            let installations = Arc::clone(&store) as Arc<dyn kuroko::router::Installations>;
            // A cold start looks the workspace up by its most recent row, since
            // there is no app user id to ask for yet.
            let stored = store
                .default_installation()
                .map_err(RunError::Store)?
                .map(kuroko::installations::restore);
            let tokens = TokenProvider::new(
                oauth.client.clone(),
                Arc::clone(&http),
                Arc::clone(&installations),
                Arc::clone(&clock),
                stored,
            );
            state
                .with_store(installations)
                .with_oauth(Oauth {
                    client: oauth.client,
                    install_secret: oauth.install_secret,
                    http,
                    states: StateStore::new(),
                })
                .with_tokens(Arc::new(tokens))
        }
        None => state,
    };

    let server = axum::serve(listener, router(state)).with_graceful_shutdown(shutdown_signal());

    tokio::select! {
        result = server => {
            result.map_err(RunError::Serve)?;
        }
        outcome = &mut engine_task => {
            return engine_outcome(outcome);
        }
    }

    log::info!("stopping, waiting for dispatches");
    drop(engine);
    engine_outcome(engine_task.await)
}

fn engine_outcome(
    result: Result<Result<(), EngineError>, tokio::task::JoinError>,
) -> Result<(), RunError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(RunError::Engine(error.to_string())),
        Err(error) => Err(RunError::Engine(error.to_string())),
    }
}

fn config_path() -> Result<PathBuf, RunError> {
    let mut path = PathBuf::from(DEFAULT_PATH);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            path = args
                .next()
                .ok_or_else(|| RunError::Args("`--config` needs a path".to_owned()))?
                .into();
        } else {
            return Err(RunError::Args(format!(
                "unknown argument `{arg}`, only `--config <path>` is accepted"
            )));
        }
    }
    Ok(path)
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            log::error!("cannot listen for SIGINT: {error}");
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                log::error!("cannot listen for SIGTERM: {error}");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

#[derive(Debug)]
enum RunError {
    Args(String),
    Boot(BootError),
    Store(kuroko_store::StoreError),
    Engine(String),
    Http {
        source: String,
    },
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    Serve(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Args(message) => f.write_str(message),
            Self::Boot(error) => write!(f, "{error}"),
            Self::Store(error) => write!(f, "cannot initialise the task store: {error}"),
            Self::Engine(message) => write!(f, "the engine stopped: {message}"),
            Self::Http { source } => write!(f, "cannot build the HTTP client: {source}"),
            Self::Bind { addr, source } => write!(f, "cannot bind {addr}: {source}"),
            Self::Serve(source) => write!(f, "cannot serve: {source}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<BootError> for RunError {
    fn from(error: BootError) -> Self {
        Self::Boot(error)
    }
}
