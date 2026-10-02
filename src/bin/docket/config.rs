use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;

use camino::Utf8Path;
use camino::Utf8PathBuf;
use kdl::KdlDocument;
use kdl::KdlError;
use kdl::KdlNode;
use kdl::KdlValue;
use miette::Diagnostic;
use miette::IntoDiagnostic as _;
use miette::SourceSpan;
use thiserror::Error;

const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000);

#[derive(Debug)]
pub struct Config {
    /// Address to listen on. Keep it on localhost; `tailscale serve`
    /// fronts it and supplies the user identity header.
    pub bind: SocketAddr,

    /// Serve without Tailscale: requests with no identity header act as a
    /// user picked from the sidebar. For local work against fixtures only.
    pub dev: bool,
}

impl Config {
    pub fn load(path: &Utf8Path) -> miette::Result<Self> {
        let source = fs_err::read_to_string(path).into_diagnostic()?;
        match parse(&source) {
            Ok(config) => Ok(config),
            // KDL syntax errors carry their own source and span labels;
            // ours need the file attached to render theirs.
            Err(Error::Kdl(err)) => Err(miette::Report::new(err)),
            Err(err) => Err(miette::Report::new(err).with_source_code(source)),
        }
    }
}

/// Settings come from the KDL file; the CLI only says where to find it.
#[derive(Debug, clap::Parser)]
#[command(version, about = "Shared household email triage over JMAP")]
pub struct Args {
    /// KDL file to read settings from.
    #[arg(long, env = "DOCKET_CONFIG", default_value = "docket.kdl")]
    pub config: Utf8PathBuf,
}

fn parse(source: &str) -> Result<Config, Error> {
    let document = KdlDocument::parse(source)?;
    let mut bind = None;
    let mut dev = None;

    for node in document.nodes() {
        match node.name().value() {
            "bind" => {
                if bind.is_some() {
                    return Err(Error::Duplicate {
                        name: "bind".to_owned(),
                        span: node.name().span(),
                    });
                }
                bind = Some(parse_bind(node)?);
            }
            "dev" => {
                if dev.is_some() {
                    return Err(Error::Duplicate {
                        name: "dev".to_owned(),
                        span: node.name().span(),
                    });
                }
                dev = Some(parse_dev(node)?);
            }
            name => {
                return Err(Error::Unknown {
                    name: name.to_owned(),
                    span: node.name().span(),
                });
            }
        }
    }

    Ok(Config {
        bind: bind.unwrap_or(DEFAULT_BIND),
        dev: dev.unwrap_or(false),
    })
}

fn parse_bind(node: &KdlNode) -> Result<SocketAddr, Error> {
    let raw = single_value(node, "bind")?
        .and_then(KdlValue::as_string)
        .ok_or(Error::BindValue { span: node.span() })?;
    raw.parse().map_err(|_| Error::BindParse {
        value: raw.to_owned(),
        span: node.span(),
    })
}

fn parse_dev(node: &KdlNode) -> Result<bool, Error> {
    match single_value(node, "dev")? {
        None => Ok(true),
        Some(value) => value.as_bool().ok_or(Error::DevValue { span: node.span() }),
    }
}

/// Returns the node's single positional value, or `None` when the node
/// is bare. Everything else a KDL node can carry — properties, type
/// annotations, child blocks, extra values — is rejected rather than
/// silently ignored.
fn single_value<'a>(node: &'a KdlNode, setting: &str) -> Result<Option<&'a KdlValue>, Error> {
    let shaped = node.ty().is_some()
        || node.children().is_some()
        || node.entries().iter().any(|entry| entry.name().is_some())
        || node.entries().len() > 1;
    if shaped {
        return Err(Error::Shape {
            setting: setting.to_owned(),
            span: node.span(),
        });
    }

    Ok(node.entries().first().map(|entry| entry.value()))
}

#[derive(Debug, Diagnostic, Error)]
enum Error {
    #[error(transparent)]
    #[diagnostic(code(docket::config::kdl_syntax))]
    Kdl(#[from] KdlError),

    #[error("unknown setting `{name}`")]
    #[diagnostic(code(docket::config::unknown), help("known settings: `bind`, `dev`"))]
    Unknown {
        name: String,
        #[label]
        span: SourceSpan,
    },

    #[error("duplicate setting `{name}`")]
    #[diagnostic(code(docket::config::duplicate))]
    Duplicate {
        name: String,
        #[label]
        span: SourceSpan,
    },

    #[error(
        "`{setting}` takes one value at most, with no properties, type annotations, or child blocks"
    )]
    #[diagnostic(code(docket::config::shape))]
    Shape {
        setting: String,
        #[label]
        span: SourceSpan,
    },

    #[error(r#"bind takes a quoted address, like bind "127.0.0.1:3000""#)]
    #[diagnostic(code(docket::config::bind_value))]
    BindValue {
        #[label]
        span: SourceSpan,
    },

    #[error("`{value}` is not a valid address")]
    #[diagnostic(code(docket::config::bind_parse))]
    BindParse {
        value: String,
        #[label]
        span: SourceSpan,
    },

    #[error("dev takes #true or #false, or nothing at all (which means true)")]
    #[diagnostic(code(docket::config::dev_value))]
    DevValue {
        #[label]
        span: SourceSpan,
    },
}

#[cfg(test)]
mod tests {
    use super::Error;
    use super::parse;

    #[test]
    fn empty_document_uses_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.bind.to_string(), "127.0.0.1:3000");
        assert!(!config.dev);
    }

    #[test]
    fn comments_only_uses_defaults() {
        let config = parse("// nothing configured\n").unwrap();
        assert_eq!(config.bind.to_string(), "127.0.0.1:3000");
        assert!(!config.dev);
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
    fn bind_with_port_only_is_rejected() {
        assert!(matches!(
            parse(r#"bind "3000""#),
            Err(Error::BindParse { .. })
        ));
    }

    #[test]
    fn bind_without_a_string_is_rejected() {
        assert!(matches!(parse("bind 3000"), Err(Error::BindValue { .. })));
        assert!(matches!(parse("bind"), Err(Error::BindValue { .. })));
    }

    #[test]
    fn bare_dev_means_true() {
        let config = parse("dev").unwrap();
        assert!(config.dev);
    }

    #[test]
    fn dev_accepts_explicit_booleans() {
        assert!(parse("dev #true").unwrap().dev);
        assert!(!parse("dev #false").unwrap().dev);
    }

    #[test]
    fn dev_rejects_non_boolean_values() {
        assert!(matches!(parse(r#"dev "yes""#), Err(Error::DevValue { .. })));
        assert!(matches!(parse("dev 1"), Err(Error::DevValue { .. })));
        assert!(matches!(parse("dev #null"), Err(Error::DevValue { .. })));
        // Bare `true` never reaches us: KDL v2 rejects it at parse time.
        assert!(matches!(parse("dev true"), Err(Error::Kdl(_))));
    }

    #[test]
    fn settings_combine() {
        let config = parse("bind \"0.0.0.0:3000\"\ndev\n").unwrap();
        assert_eq!(config.bind.to_string(), "0.0.0.0:3000");
        assert!(config.dev);
    }

    #[test]
    fn unknown_setting_is_rejected() {
        assert!(matches!(parse("bnd \"x\""), Err(Error::Unknown { .. })));
    }

    #[test]
    fn duplicate_setting_is_rejected() {
        assert!(matches!(
            parse("dev\ndev #false"),
            Err(Error::Duplicate { .. })
        ));
        let source = "bind \"127.0.0.1:1\"\nbind \"127.0.0.1:2\"";
        assert!(matches!(parse(source), Err(Error::Duplicate { .. })));
    }

    #[test]
    fn extra_syntax_is_rejected() {
        assert!(matches!(
            parse("dev #true #false"),
            Err(Error::Shape { .. })
        ));
        assert!(matches!(parse("dev on=#false"), Err(Error::Shape { .. })));
        assert!(matches!(parse("dev { inner }"), Err(Error::Shape { .. })));
        assert!(matches!(
            parse(r#"bind address="127.0.0.1:3000""#),
            Err(Error::Shape { .. })
        ));
    }

    #[test]
    fn malformed_kdl_is_rejected() {
        assert!(matches!(parse(r#"bind "unterminated"#), Err(Error::Kdl(_))));
        assert!(matches!(parse("="), Err(Error::Kdl(_))));
    }
}
