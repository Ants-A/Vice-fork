//! Serve one file on loopback for read-ahead throughput/jitter comparisons.
//! Usage: cargo run --example media_stream_bench -- /path/to/clip.mp4
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("supply a media path"))?,
    );
    let app = Router::new().fallback(move |request: Request<Body>| {
        let path = path.clone();
        async move {
            vice::media_stream::serve_as(path, request, Some("video/mp4"))
                .await
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!("{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
