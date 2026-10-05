//! The real-server smoke test: the deploy path — session open, full
//! import, one poll — against Fastmail itself. Read-only, so a
//! read-scope token suffices. Ignored by default; `just smoke` runs it,
//! reading the token from 1Password via the `op` CLI.

use camino::Utf8PathBuf;
use docket::jmap::{Client, Credential};
use docket::store::{Clock, Store};

/// The smoke test's token, by item ID: the item's name contains a
/// colon, which `op://` references don't allow (alphanumerics, `-`,
/// `_`, `.`, and spaces only), so the ID stands in for the name.
const TOKEN_REF: &str = "op://Private/dpepkrnjuhnao5h7fd52kvqay4/credential";

/// Reads the token via `op`, which must be installed and signed in.
fn token() -> Result<String, Box<dyn std::error::Error>> {
    let output = match std::process::Command::new("op")
        .arg("read")
        .arg(TOKEN_REF)
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
            "op read failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[test]
#[ignore = "run via `just smoke`; reads its token from 1Password"]
fn the_session_open_chain_runs_against_fastmail() -> Result<(), Box<dyn std::error::Error>> {
    let token = token()?;
    // The server sees only the bearer header, so a bad token surfaces
    // as an opaque 401; what 1Password handed over is the other half.
    let pieces = token.split_whitespace().count();
    if pieces != 1 {
        return Err(format!(
            "the 1Password field must hold exactly the token; got {pieces} \
             whitespace-separated pieces"
        )
        .into());
    }
    println!(
        "token from 1Password: {} chars, {}…{}",
        token.chars().count(),
        token.chars().take(4).collect::<String>(),
        token.chars().rev().take(4).collect::<String>()
    );
    // Credentials are files (systemd LoadCredential in production), so
    // hand the token to the Client through one.
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(&file, token)?;
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
