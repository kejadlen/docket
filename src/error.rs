use miette::Diagnostic;

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum Error {
    #[error("io error: {0}")]
    #[diagnostic(code(docket::io))]
    Io(#[from] std::io::Error),
}
