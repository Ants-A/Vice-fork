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
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::PathBuf,
    sync::Arc,
    time::SystemTime,
};
use tokio::sync::mpsc;
use tower::ServiceExt;

const FIRST_READ: usize = 128 * 1024;
const READ_SIZE: usize = 4 * 1024 * 1024;
const QUEUED_READS: usize = 4;

pub async fn serve(path: PathBuf, req: Request<Body>) -> Result<Response> {
    serve_as(path, req, None).await
}
pub async fn serve_as(
    path: PathBuf,
    req: Request<Body>,
    mime: Option<&'static str>,
) -> Result<Response> {
    let head = req.method() == Method::HEAD;
    let requested_version = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("v=")))
        .map(str::to_owned);
    let opened = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            return Ok(tower_http::services::ServeFile::new(&path)
                .oneshot(req)
                .await?
                .map(Body::new));
        }
    };
    let metadata = opened.metadata().await?;
    let revision = format!(
        "{}-{}",
        metadata.len(),
        metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let file = Arc::new(opened.into_std().await);
    // Pin headers and reads to the same inode, even if trimming, tagging or
    // recording atomically replaces the pathname before the body is polled.
    let response =
        tower_http::services::ServeFile::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .oneshot(req)
            .await?;
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        header::CONTENT_TYPE,
        match mime {
            Some(value) => value,
            None => content_type(&path, &file)?,
        }
        .parse()?,
    );
    parts.headers.insert(
        header::CACHE_CONTROL,
        match requested_version.as_deref() {
            Some(version) if version == revision => "private, max-age=3600",
            Some(_) => "no-store", // Never cache a response under a stale content revision.
            None => "no-cache",
        }
        .parse()?,
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
        let file = file.clone();
        async move {
            let mut receiver = receiver.unwrap_or_else(|| {
                let (send, receive) = mpsc::channel(QUEUED_READS);
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let mut file = file.try_clone()?;
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

fn content_type(path: &std::path::Path, file: &File) -> io::Result<&'static str> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let known = match ext.as_str() {
        "mp4" | "mov" | "m4v" => Some("video/mp4"),
        "mkv" => Some("video/x-matroska"),
        "webm" => Some("video/webm"),
        "m4a" => Some("audio/mp4"),
        "aac" => Some("audio/aac"),
        "ogg" => Some("audio/ogg"),
        _ => None,
    };
    if let Some(mime) = known {
        return Ok(mime);
    }
    let mut header = [0; 4096];
    let count = file.read_at(&mut header, 0)?;
    let header = &header[..count];
    Ok(if header.get(4..8) == Some(b"ftyp") {
        "video/mp4"
    } else if header.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        if header.windows(4).any(|w| w == b"webm") {
            "video/webm"
        } else {
            "video/x-matroska"
        }
    } else {
        "application/octet-stream"
    })
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
        let stat = crate::media::FileIdentity::read(path).unwrap();
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/video?v={}-{}", stat.size, stat.mtime_ns));
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
    async fn an_atomic_replace_cannot_mix_old_headers_with_new_media() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("clip.mp4");
        std::fs::write(&path, b"old complete video").unwrap();
        let response = request(&path, None, Method::GET).await;
        let next = temp.path().join("next.mp4");
        std::fs::write(&next, b"shorter new clip").unwrap();
        std::fs::rename(next, &path).unwrap();
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"old complete video"
        );
        let stale = Request::builder()
            .uri("/video?v=old-version")
            .body(Body::empty())
            .unwrap();
        let response = serve(path, stale).await.unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"shorter new clip"
        );
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
