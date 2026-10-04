use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;

use camino::Utf8Path;
use camino::Utf8PathBuf;
use docket::jmap::Credentials;
use miette::IntoDiagnostic as _;
use serde::Deserialize;
use serde::Deserializer;
use serde::de;
use serde::de::Visitor;

const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000);
const DEFAULT_DATABASE: &str = "docket.db";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address to listen on. Keep it on localhost; Caddy (caddy-tailscale)
    /// fronts it and supplies the user identity headers.
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,

    /// SQLite database file, created if missing. A relative path is
    /// relative to the working directory.
    #[serde(default = "default_database", deserialize_with = "non_empty_path")]
    pub database: Utf8PathBuf,

    /// One node per Fastmail API token (DESIGN.md, Storage):
    /// `credential "household" token-file="/run/credentials/docket/household"`.
    #[serde(default, rename = "credential")]
    pub credentials: Credentials,
}

impl Config {
    pub fn load(path: &Utf8Path) -> miette::Result<Self> {
        let source = fs_err::read_to_string(path).into_diagnostic()?;
        // Errors carry the source, so they render with their labels.
        let config = parse(&source)?;
        config.validate()?;
        Ok(config)
    }

    /// Credential names become account slugs, so a duplicate would
    /// silently merge two logins into one account.
    fn validate(&self) -> miette::Result<()> {
        let mut seen = BTreeSet::new();
        for credential in self.credentials.iter() {
            if !seen.insert(&credential.name) {
                return Err(miette::miette!("duplicate credential {}", credential.name));
            }
        }
        Ok(())
    }
}

/// Settings come from the KDL file; the CLI only says where to find it.
#[derive(Debug, clap::Parser)]
#[command(version = docket::VERSION, about = "Shared household email triage over JMAP")]
pub struct Args {
    /// KDL file to read settings from.
    #[arg(long, env = "DOCKET_CONFIG", default_value = "docket.kdl")]
    pub config: Utf8PathBuf,
}

fn parse(source: &str) -> Result<Config, kdl::de::Error> {
    kdl::de::from_str(source)
}

fn default_bind() -> SocketAddr {
    DEFAULT_BIND
}

fn default_database() -> Utf8PathBuf {
    Utf8PathBuf::from(DEFAULT_DATABASE)
}

fn non_empty_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Utf8PathBuf, D::Error> {
    deserializer.deserialize_str(NonEmptyPath)
}

// Rejecting inside the visitor, rather than after, lets kdl label the value.
struct NonEmptyPath;

impl Visitor<'_> for NonEmptyPath {
    type Value = Utf8PathBuf;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a non-empty path string")
    }

    fn visit_str<E: de::Error>(self, path: &str) -> Result<Utf8PathBuf, E> {
        if path.is_empty() {
            return Err(E::custom("database path is empty"));
        }
        Ok(Utf8PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use miette::Diagnostic as _;

    use super::{Config, parse};

    /// The error's message, and whether it points into the source.
    fn error(source: &str) -> (String, bool) {
        let err = parse(source).unwrap_err();
        let labeled = err.labels().is_some_and(|mut l| l.next().is_some())
            || err
                .related()
                .is_some_and(|mut r| r.any(|d| d.labels().is_some_and(|mut l| l.next().is_some())));
        (err.to_string(), labeled)
    }

    #[test]
    fn empty_document_uses_defaults() {
        for source in ["", "// nothing configured\n"] {
            let config = parse(source).unwrap();
            assert_eq!(config.bind.to_string(), "127.0.0.1:3000");
            assert_eq!(config.database, "docket.db");
        }
    }

    #[test]
    fn bind_parses_ipv4_ipv6_and_wildcard() {
        for (source, addr) in [
            (r#"bind "0.0.0.0:8080""#, "0.0.0.0:8080"),
            (r#"bind "[::1]:3000""#, "[::1]:3000"),
            (r#"bind "192.168.1.10:0""#, "192.168.1.10:0"),
        ] {
            assert_eq!(parse(source).unwrap().bind.to_string(), addr);
        }
    }

    #[test]
    fn bad_values_point_at_the_value() {
        for (source, message) in [
            (r#"bind "3000""#, "invalid socket address syntax"),
            ("bind 3000", "expected socket address"),
            ("database 1", "expected a non-empty path string"),
            (r#"database """#, "database path is empty"),
        ] {
            let (err, labeled) = error(source);
            assert!(err.contains(message), "{source}: {err}");
            assert!(labeled, "{source}");
        }
    }

    #[test]
    fn database_takes_a_path() {
        assert_eq!(
            parse(r#"database "/var/lib/docket/docket.db""#)
                .unwrap()
                .database,
            "/var/lib/docket/docket.db"
        );
    }

    #[test]
    fn settings_combine() {
        let config = parse("bind \"0.0.0.0:3000\"\ndatabase \"d.db\"\n").unwrap();
        assert_eq!(config.bind.to_string(), "0.0.0.0:3000");
        assert_eq!(config.database, "d.db");
        assert!(config.credentials.is_empty());
    }

    #[test]
    fn a_lone_credential_parses() {
        let config =
            parse(r#"credential "household" token-file="/run/credentials/docket/household""#)
                .unwrap();
        let credential = config.credentials.first().unwrap();
        assert_eq!(credential.name, "household");
        assert_eq!(credential.token_file, "/run/credentials/docket/household");
    }

    #[test]
    fn load_rejects_duplicate_credential_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docket.kdl");
        fs_err::write(
            &path,
            "credential \"household\" token-file=\"/a\"\ncredential \"household\" token-file=\"/b\"\n",
        )
        .unwrap();
        let path = camino::Utf8PathBuf::try_from(path).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(
            err.to_string().contains("duplicate credential household"),
            "{err}"
        );
    }

    #[test]
    fn misshapen_settings_are_rejected() {
        for (source, message) in [
            (r#"bnd "x""#, "unknown field `bnd`"),
            // Dev mode is a build feature, not a setting.
            ("dev", "unknown field `dev`"),
            ("bind", "expected socket address"),
            ("database", "expected a non-empty path string"),
            ("bind \"127.0.0.1:1\"\nbind \"127.0.0.1:2\"", "sequence"),
            (r#"database "a.db" "b.db""#, "sequence"),
            (r#"database path="a.db""#, "map"),
            (r#"database "a.db" { inner }"#, "map"),
        ] {
            let (err, _) = error(source);
            assert!(err.contains(message), "{source}: {err}");
        }
    }

    #[test]
    fn malformed_kdl_points_at_the_problem() {
        for source in [r#"bind "unterminated"#, "="] {
            let (err, labeled) = error(source);
            assert!(
                err.contains("Failed to parse KDL document"),
                "{source}: {err}"
            );
            assert!(labeled, "{source}");
        }
    }
}
