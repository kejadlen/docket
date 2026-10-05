mod config;
mod poll;

use clap::Parser as _;
use miette::IntoDiagnostic as _;
use miette::WrapErr as _;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> miette::Result<()> {
    miette::set_panic_hook();
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let args = config::Args::parse();
    let config = config::Config::load(&args.config)?;
    // Dev pins the fixture clock so sample ages stay stable; production
    // runs on real time.
    #[cfg(feature = "dev")]
    let clock = docket::store::Clock::Fixed(docket::fixtures::now());
    #[cfg(not(feature = "dev"))]
    let clock = docket::store::Clock::System;
    let store = docket::store::Store::open(&config.database, clock)?;
    #[cfg(feature = "dev")]
    if store.is_empty()? {
        docket::fixtures::seed(&store)?;
        tracing::info!(database = %config.database, "seeded fixtures");
    }
    // One session per credential, before serving: a credential that
    // can't open one is a config or token problem to fix, not an account
    // to quietly leave out. Each sync then polls for changes in the
    // background.
    let jmap = docket::jmap::Client::fastmail().into_diagnostic()?;
    for credential in config.credentials.iter().cloned() {
        let sync = jmap
            .sync_account(&credential, &store)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("opening the JMAP session for {}", credential.name))?;
        poll::spawn(jmap.clone(), credential, store.clone(), sync);
    }
    let state = docket::routes::AppState::new(store);
    #[cfg(feature = "dev")]
    let app = docket::dev::router(state);
    #[cfg(not(feature = "dev"))]
    let app = docket::routes::router(state);

    let listener = TcpListener::bind(config.bind).await.into_diagnostic()?;
    tracing::info!(addr = %config.bind, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .into_diagnostic()?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = signal::ctrl_c().await {
            tracing::error!(%err, "failed to install SIGINT handler");
            std::future::pending::<()>().await;
        }
    };

    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        () = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
