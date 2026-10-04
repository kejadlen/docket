pub mod dates;
#[cfg(feature = "dev")]
pub mod dev;
pub mod error;
pub mod fixtures;
pub mod jmap;
pub mod lists;
pub mod model;
pub mod routes;
pub mod store;
pub mod views;

pub use error::Error;

/// Which build this is: the release's image tag, or a local `-dev` build.
pub const VERSION: &str = env!("DOCKET_VERSION");
