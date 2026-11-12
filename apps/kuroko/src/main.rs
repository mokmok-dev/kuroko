//! The kuroko daemon binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use kuroko::clock::SystemClock;
use kuroko::config::{BootError, Config, DEFAULT_PATH};
use kuroko::router::{AppState, router};
use kuroko::transport::build_transport;
use kuroko_core::Clock;
use kuroko_engine::spawn;
use kuroko_linear::Gate;
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
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let transport = build_transport(&config.transport);
    let gate = Gate::new(
        config.secret,
        config.replay_window,
        config.triggers,
        config.template,
    );
    let (engine, engine_task) = spawn(transport, config.limits, Arc::clone(&clock));
    let engine = Arc::new(engine);

    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|source| RunError::Bind {
            addr: config.bind,
            source,
        })?;
    log::info!("listening on {}", config.bind);

    axum::serve(
        listener,
        router(AppState::new(gate, Arc::clone(&engine), clock)),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(RunError::Serve)?;

    log::info!("stopping, waiting for dispatches");
    drop(engine);
    let _ = engine_task.await;
    Ok(())
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
