//! Serve original bytes with read-ahead independent of HTTP backpressure.
use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use futures_util::stream;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::fd::AsRawFd,
    path::PathBuf,
};
use tokio::sync::mpsc;
use tower::ServiceExt;

const FIRST_READ: usize = 128 * 1024;
const READ_SIZE: usize = 4 * 1024 * 1024;
const QUEUED_READS: usize = 4;

pub async fn serve(path: PathBuf, req: Request<Body>) -> Result<Response> {
    let head = req.method() == Method::HEAD;
    let versioned = req
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|part| part.starts_with("v=")));
    // Keep tower's conditional requests, exact byte ranges, suffix ranges, HEAD,
    // and error responses. Replace only successful response bodies.
    let response = tower_http::services::ServeFile::new(&path)
        .oneshot(req)
        .await?;
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        header::CACHE_CONTROL,
        if versioned {
            "private, max-age=3600".parse()?
        } else {
            "no-cache".parse()?
        },
    );
    if head || !matches!(parts.status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        return Ok(Response::from_parts(parts, Body::new(body)));
    }
    let length: u64 = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .context("file content length")?
        .to_str()?
        .parse()?;
    let offset: u64 = parts
        .headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "))
        .and_then(|value| value.split_once('-'))
        .map(|(start, _)| start.parse())
        .transpose()?
        .unwrap_or(0);
    drop(body);
    // Start on the first body poll. HEAD and abandoned, unpolled responses do
    // no file reads. Dropping the receiver stops the worker at the next chunk.
    let chunks = stream::unfold(None::<mpsc::Receiver<io::Result<Bytes>>>, move |receiver| {
        let path = path.clone();
        async move {
            let mut receiver = receiver.unwrap_or_else(|| {
                let (send, receive) = mpsc::channel(QUEUED_READS);
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let mut file = File::open(path)?;
                        file.seek(SeekFrom::Start(offset))?;
                        // A hint on this handle only; no mount or system settings.
                        unsafe {
                            libc::posix_fadvise(
                                file.as_raw_fd(),
                                offset as i64,
                                length as i64,
                                libc::POSIX_FADV_SEQUENTIAL,
                            );
                        }
                        read_ahead(&mut file, length, &send)
                    })();
                    if let Err(error) = result {
                        let _ = send.blocking_send(Err(error));
                    }
                });
                receive
            });
            receiver.recv().await.map(|chunk| (chunk, Some(receiver)))
        }
    });
    Ok(Response::from_parts(parts, Body::from_stream(chunks)))
}

fn read_ahead(
    reader: &mut impl Read,
    mut remaining: u64,
    send: &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    let mut read_size = FIRST_READ;
    while remaining > 0 && !send.is_closed() {
        let count = remaining.min(read_size as u64) as usize;
        read_size = (read_size * 2).min(READ_SIZE);
        let mut buffer = vec![0; count];
        reader.read_exact(&mut buffer)?;
        remaining -= count as u64;
        if send.blocking_send(Ok(Bytes::from(buffer))).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    async fn request(path: &std::path::Path, range: Option<&str>, method: Method) -> Response {
        let mut request = Request::builder().method(method).uri("/video?v=1");
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        serve(path.to_owned(), request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn original_bytes_ranges_and_conditions_survive_read_ahead() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("original.mp4");
        let data: Vec<u8> = (0..READ_SIZE * 2 + 113).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        for (range, start, end) in [
            (None, 0, data.len()),
            (Some("bytes=0-127"), 0, 128),
            (Some("bytes=131000-4321000"), 131000, 4321001),
            (Some("bytes=-113"), data.len() - 113, data.len()),
            (Some("bytes=4194304-"), 4194304, data.len()),
        ] {
            let response = request(&path, range, Method::GET).await;
            assert_eq!(
                response.status(),
                if range.is_some() {
                    StatusCode::PARTIAL_CONTENT
                } else {
                    StatusCode::OK
                }
            );
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH],
                (end - start).to_string()
            );
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, max-age=3600"
            );
            let bytes = to_bytes(response.into_body(), data.len()).await.unwrap();
            assert_eq!(&bytes[..], &data[start..end]);
        }
        let head = request(&path, None, Method::HEAD).await;
        let modified = head.headers()[header::LAST_MODIFIED].clone();
        assert!(to_bytes(head.into_body(), 1).await.unwrap().is_empty());
        let conditional = Request::builder()
            .header(header::IF_MODIFIED_SINCE, modified)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            serve(path.clone(), conditional).await.unwrap().status(),
            StatusCode::NOT_MODIFIED
        );
        let bad = request(&path, Some("bytes=999999999-"), Method::GET).await;
        assert_eq!(bad.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }
    #[tokio::test]
    async fn startup_does_not_wait_for_the_read_ahead_window_and_close_stops_reads() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Source(Arc<AtomicUsize>);
        impl Read for Source {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.0.fetch_add(buffer.len(), Ordering::Relaxed);
                buffer.fill(7);
                Ok(buffer.len())
            }
        }
        let read = Arc::new(AtomicUsize::new(0));
        let (send, mut receive) = mpsc::channel(QUEUED_READS);
        let count = read.clone();
        let worker = tokio::task::spawn_blocking(move || {
            read_ahead(&mut Source(count), 1024 * 1024 * 1024, &send)
        });
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), receive.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.len(), FIRST_READ);
        drop(receive);
        worker.await.unwrap().unwrap();
        assert!(read.load(Ordering::Relaxed) <= FIRST_READ + READ_SIZE * (QUEUED_READS + 2));
    }
}
