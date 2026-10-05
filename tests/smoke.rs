//! The real-server smoke test: the deploy path — session open, full
//! import, one poll — against Fastmail itself. Read-only, so a
//! read-scope token suffices. Ignored by default; `just smoke` runs it
//! with DOCKET_SMOKE_TOKEN_FILE naming a Fastmail token file.

use camino::Utf8PathBuf;
use docket::jmap::{Client, Credential};
use docket::store::{Clock, Store};

#[test]
#[ignore = "run via `just smoke` with DOCKET_SMOKE_TOKEN_FILE set"]
fn the_session_open_chain_runs_against_fastmail() -> Result<(), Box<dyn std::error::Error>> {
    let token_file = match std::env::var("DOCKET_SMOKE_TOKEN_FILE") {
        Ok(path) => Utf8PathBuf::from(path),
        Err(_) => {
            return Err("DOCKET_SMOKE_TOKEN_FILE must name a Fastmail token file".into());
        }
    };
    let credential = Credential {
        name: "smoke".into(),
        token_file,
    };
    let store = Store::open_in_memory(Clock::System)?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let client = Client::fastmail()?;
        let mut sync = client.sync_account(&credential, &store).await?;
        println!(
            "session opened: email state {}, mailbox state {}",
            sync.email_state, sync.mailbox_state
        );
        let counts = client.poll_once(&credential, &mut sync, &store).await?;
        println!("poll: {counts:?}");
        Ok(())
    })
}
