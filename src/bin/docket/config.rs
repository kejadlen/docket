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
use std::str::FromStr as _;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;

const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000);
const DEFAULT_DATABASE: &str = "docket.db";
const DEFAULT_LOG: LevelFilter = LevelFilter::WARN;

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

    /// Log level as an optional node argument (default `warn`), with
    /// per-target overrides as properties: `log docket=info`.
    #[serde(default = "default_log", deserialize_with = "log_targets")]
    pub log: Targets,

    /// One node per Fastmail API token (DESIGN.md, Storage):
    /// `credential "household" token-file="/run/credentials/docket/household"`.
    #[serde(default, rename = "credential")]
    pub credentials: Credentials,

    /// Where to report errors and panics: `sentry dsn="https://…"`.
    /// Without it, they only reach the log.
    #[serde(default, deserialize_with = "present")]
    pub sentry: Option<Sentry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sentry {
    pub dsn: String,
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

fn default_log() -> Targets {
    Targets::new().with_default(DEFAULT_LOG)
}

fn log_targets<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Targets, D::Error> {
    deserializer.deserialize_struct("Log", LOG_FIELDS, LogNode)
}

/// The kdl field names of a `log` node: the level argument (`#0`) and a
/// would-be second (`#1`, rejected). Properties override per target.
/// A derived struct can't express this: flattening the properties into a
/// map forces deserialize_map, which hides kdl's `#0` argument naming.
const LOG_FIELDS: &[&str] = &["#0", "#1"];

struct LogNode;

impl<'de> Visitor<'de> for LogNode {
    type Value = Targets;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a log node")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Targets, A::Error> {
        let mut level = None;
        let mut overrides = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "#0" => level = Some(map.next_value::<Level>()?.0),
                "#1" => return Err(de::Error::custom("log takes one argument, its level")),
                target => {
                    let Level(target_level) = map.next_value()?;
                    overrides.push((target.to_owned(), target_level));
                }
            }
        }
        let level = level.unwrap_or(DEFAULT_LOG);
        let mut log = Targets::new().with_default(level);
        for (target, level) in overrides {
            log = log.with_target(target, level);
        }
        Ok(log)
    }
}

/// A level parsed inside `visit_str`, where kdl can label a bad value.
struct Level(LevelFilter);

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = Level;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a log level")
            }

            fn visit_str<E: de::Error>(self, source: &str) -> Result<Level, E> {
                LevelFilter::from_str(source).map_err(E::custom).map(Level)
            }
        }
        deserializer.deserialize_str(V)
    }
}

/// Deserializes a node that must carry its settings when present: plain
/// `Option` would read a bare node as absent and quietly drop it.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
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

    use std::str::FromStr as _;

    use super::{Config, LevelFilter, Targets, parse};

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
            assert_eq!(config.log, Targets::from_str("warn").unwrap());
            assert!(config.sentry.is_none());
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
    fn log_takes_a_level_with_target_overrides() {
        // Levels and targets parse bare; quoting them stays legal.
        for source in [r#"log "debug" hyper="warn""#, "log debug hyper=warn"] {
            let config = parse(source).unwrap();
            let expected = Targets::new()
                .with_default(LevelFilter::DEBUG)
                .with_target("hyper", LevelFilter::WARN);
            assert_eq!(config.log, expected);
        }
    }

    #[test]
    fn log_level_is_optional() {
        let config = parse("log docket=info").unwrap();
        let expected = Targets::new()
            .with_default(LevelFilter::WARN)
            .with_target("docket", LevelFilter::INFO);
        assert_eq!(config.log, expected);

        // A bare `log` node means the same as no log node at all.
        assert_eq!(parse("log").unwrap().log, parse("").unwrap().log);
    }

    #[test]
    fn bad_values_point_at_the_value() {
        for (source, message) in [
            (r#"bind "3000""#, "invalid socket address syntax"),
            ("bind 3000", "expected socket address"),
            ("database 1", "expected a non-empty path string"),
            (r#"database """#, "database path is empty"),
            (r#"log "verbose""#, "error parsing level filter"),
            (
                r#"log "info" hyper="verbose""#,
                "error parsing level filter",
            ),
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
    fn sentry_takes_a_dsn() {
        let config = parse(r#"sentry dsn="https://key@o0.ingest.sentry.io/0""#).unwrap();
        assert_eq!(
            config.sentry.unwrap().dsn,
            "https://key@o0.ingest.sentry.io/0"
        );
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
            (r#"log "info" "debug""#, "log takes one argument, its level"),
            ("bind \"127.0.0.1:1\"\nbind \"127.0.0.1:2\"", "sequence"),
            (r#"database "a.db" "b.db""#, "sequence"),
            (r#"database path="a.db""#, "map"),
            (r#"database "a.db" { inner }"#, "map"),
            ("sentry", "expected a string"),
            (r#"sentry dns="x""#, "unknown field `dns`"),
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
