mod config;
mod poll;

use clap::Parser as _;
use miette::IntoDiagnostic as _;
use miette::WrapErr as _;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> miette::Result<()> {
    miette::set_panic_hook();

    let args = config::Args::parse();
    let config = config::Config::load(&args.config)?;
    // Sentry's panic hook chains to miette's, so it has to come second.
    // The guard flushes queued events when main returns.
    let _sentry = config.sentry.as_ref().map(|sentry| {
        let mut options = sentry::ClientOptions::default();
        options.release = Some(docket::VERSION.into());
        sentry::init((sentry.dsn.as_str(), options))
    });
    // Tracing starts after the config so its filter comes from the file;
    // config errors report through miette, which needs no subscriber.
    // Sentry turns error events into issues and lesser ones into
    // breadcrumbs; without a DSN its layer has no client and drops them.
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(sentry::integrations::tracing::layer())
        .with(config.log.clone())
        .init();

    // An error that ends the process skips tracing, so report it here.
    run(config).await.inspect_err(|err| {
        let err: &(dyn std::error::Error + Send + Sync) = err.as_ref();
        sentry::capture_error(err);
    })
}

async fn run(config: config::Config) -> miette::Result<()> {
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
