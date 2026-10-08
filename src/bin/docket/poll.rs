//! The polling loop (task lylzwoyo, "Sync changes by polling"): one
//! task per credential, applying `/changes` every 30s so new and
//! changed messages show up within a minute. Push (task nwylszul,
//! "Sync changes via JMAP push") replaces the timer later; the changes
//! application it triggers stays the same.

use std::time::Duration;

use docket::jmap::{Client, Credential, Sync};
use docket::store::Store;

/// "Within a minute or so" with headroom for the fetches a cycle makes.
const INTERVAL: Duration = Duration::from_secs(30);

/// Runs the loop in the background. Errors are logged and retried on
/// the next tick — a blip must not take syncing down with it — at
/// error level, so each failure reaches Sentry.
pub fn spawn(client: Client, credential: Credential, store: Store, sync: Sync) {
    tokio::spawn(async move {
        let name = credential.name.clone();
        let mut sync = sync;
        loop {
            tokio::time::sleep(INTERVAL).await;
            match client.poll_once(&credential, &mut sync, &store).await {
                Ok(counts) if counts.is_quiet() => {
                    tracing::debug!(credential = %name, ?counts, "poll");
                }
                Ok(counts) => {
                    tracing::info!(credential = %name, ?counts, "poll");
                }
                Err(err) => {
                    tracing::error!(credential = %name, %err, "poll failed; retrying next tick");
                }
            }
        }
    });
}
