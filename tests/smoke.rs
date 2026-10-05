//! The real-server smoke test: the deploy path — session open, full
//! import, one poll — against Fastmail itself. Read-only, so a
//! read-scope token suffices. Ignored by default; `just smoke` runs it,
//! reading the token from 1Password via the `op` CLI.

use camino::Utf8PathBuf;
use docket::jmap::{Client, Credential};
use docket::store::{Clock, Store};

/// Where the smoke test's token lives in 1Password. The item name's
/// colon is illegal in an `op://` secret reference (alphanumerics,
/// `-`, `_`, `.`, and spaces only), so the token is fetched by name
/// with `op item get` rather than `op read`.
const OP_VAULT: &str = "Private";
const OP_ITEM: &str = "Fastmail API token: docket smoke test";
const OP_FIELD: &str = "credential";

/// Reads the token via `op`, which must be installed and signed in.
fn token() -> Result<String, Box<dyn std::error::Error>> {
    let output = match std::process::Command::new("op")
        .arg("item")
        .arg("get")
        .arg(OP_ITEM)
        .arg("--vault")
        .arg(OP_VAULT)
        .arg("--fields")
        .arg(OP_FIELD)
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("the 1Password CLI (`op`) is not installed".into());
        }
        Err(e) => return Err(e.into()),
    };
    if !output.status.success() {
        return Err(format!(
            "op item get failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[test]
#[ignore = "run via `just smoke`; reads its token from 1Password"]
fn the_session_open_chain_runs_against_fastmail() -> Result<(), Box<dyn std::error::Error>> {
    // Credentials are files (systemd LoadCredential in production), so
    // hand the token to the Client through one.
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(&file, token()?)?;
    let credential = Credential {
        name: "smoke".into(),
        token_file: Utf8PathBuf::from_path_buf(file.path().to_owned())
            .map_err(|_| "the temp token file must be valid UTF-8")?,
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
