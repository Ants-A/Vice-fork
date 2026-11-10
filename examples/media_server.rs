//! Read-only original-media benchmark server; does not start the recorder.
//! cargo run --example media_server -- /path/to/clip.mp4 18765
use axum::{
    Router,
    body::Body,
    http::Request,
    response::{Html, IntoResponse},
    routing::{any, get},
};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = std::path::PathBuf::from(std::env::args().nth(1).expect("clip path"));
    let port = std::env::args().nth(2).unwrap_or_else(|| "18765".into());
    let app = Router::new()
        .route(
            "/api/status",
            get(|| async { Html("<html><body>Playback benchmark</body></html>") }),
        )
        .route(
            "/v/{slug}",
            any(move |request: Request<Body>| {
                let source = source.clone();
                async move {
                    match vice::media_stream::serve(source, request).await {
                        Ok(response) => response,
                        Err(error) => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            error.to_string(),
                        )
                            .into_response(),
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    eprintln!(
        "Read-only playback benchmark: http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}
