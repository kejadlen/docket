use std::net::SocketAddr;

#[derive(Debug, clap::Parser)]
#[command(version)]
pub struct Config {
    /// Address to listen on. Keep it on localhost; `tailscale serve`
    /// fronts it and supplies the user identity header.
    #[arg(long, env = "DOCKET_BIND", default_value = "127.0.0.1:3000")]
    pub bind: SocketAddr,

    /// Serve without Tailscale: requests with no identity header act as a
    /// user picked from the sidebar. For local work against fixtures only.
    #[arg(long, env = "DOCKET_DEV")]
    pub dev: bool,
}
