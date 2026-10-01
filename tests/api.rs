use std::net::SocketAddr;

use reqwest::StatusCode;
use tokio::net::TcpListener;

async fn spawn() -> SocketAddr {
    let app = docket::routes::router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn health_returns_ok() {
    let addr = spawn().await;
    let res = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "ok");
}
