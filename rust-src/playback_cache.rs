//! A player's original bytes survive EOF and seeking, until its session closes.
use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use futures_util::stream;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{Semaphore, mpsc};

const BLOCK: u64 = 1024 * 1024;
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
type BlockState = Arc<Mutex<bool>>;

#[derive(Clone)]
pub struct Sessions {
    root: Arc<CacheRoot>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
}
struct CacheRoot(PathBuf);
impl Drop for CacheRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// A process owns only its UUID directory. Never delete another live registry's
// files, including registries in the same process; recover files after a crash.
fn prepare_root(root: &std::path::Path) -> io::Result<()> {
    let parent = root
        .parent()
        .ok_or_else(|| io::Error::other("missing cache parent"))?;
    std::fs::create_dir_all(parent)?;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(|s| s.strip_suffix(".session")) else {
            continue;
        };
        let Some((pid, key)) = name.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if uuid::Uuid::parse_str(key).is_err() || !entry.file_type()?.is_dir() {
            continue;
        }
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    std::fs::create_dir_all(root)
}

struct Session {
    source: File,
    cache: File,
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    etag: String,
    mime: &'static str,
    blocks: Mutex<HashMap<u64, BlockState>>,
    permits: Arc<Semaphore>,
    touched: Mutex<Instant>,
    closed: AtomicBool,
    source_bytes: AtomicU64,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Sessions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(CacheRoot(root.join(format!(
                "{}-{}.session",
                std::process::id(),
                uuid::Uuid::new_v4()
            )))),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub async fn create(&self, source: PathBuf) -> Result<String> {
        self.reap_expired();
        let root = self.root.clone();
        let id = uuid::Uuid::new_v4().to_string();
        let key = id.clone();
        let session = tokio::task::spawn_blocking(move || -> Result<Session> {
            let file = File::open(&source).context("open playback source")?;
            let metadata = file.metadata()?;
            anyhow::ensure!(metadata.is_file(), "playback source must be a file");
            prepare_root(&root.0)?;
            let path = root.0.join(format!("{key}.cache"));
            let cache = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            // Sparse allocation; this does not read or copy the recording.
            if let Err(error) = cache.set_len(metadata.len()) {
                let _ = std::fs::remove_file(&path);
                return Err(error.into());
            }
            let mime = match source
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str()
            {
                "mp4" | "m4v" | "mov" => "video/mp4",
                "mkv" => "video/x-matroska",
                "webm" => "video/webm",
                _ => "application/octet-stream",
            };
            Ok(Session {
                source: file,
                cache,
                path,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                etag: format!("\"{key}\""),
                mime,
                blocks: Mutex::new(HashMap::new()),
                permits: Arc::new(Semaphore::new(4)),
                touched: Mutex::new(Instant::now()),
                closed: AtomicBool::new(false),
                source_bytes: AtomicU64::new(0),
            })
        })
        .await??;
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::new(session));
        Ok(id)
    }
    pub fn touch(&self, id: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(id) {
            *session.touched.lock().unwrap() = Instant::now();
            true
        } else {
            false
        }
    }
    /// Unlink immediately; any already-running read retains safe open handles.
    pub fn close(&self, id: &str) -> bool {
        if let Some(session) = self.sessions.lock().unwrap().remove(id) {
            session.close();
            true
        } else {
            false
        }
    }
    /// Call periodically while running. Heartbeats keep an open, paused player alive.
    pub fn reap_expired(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|_, session| {
            let keep = session.touched.lock().unwrap().elapsed() < TTL;
            if !keep {
                session.close();
            }
            keep
        });
        before - sessions.len()
    }
    pub async fn serve(
        &self,
        id: &str,
        req: Request<Body>,
        mime: Option<&'static str>,
    ) -> Result<Response> {
        let session = self.sessions.lock().unwrap().get(id).cloned();
        let Some(session) = session else {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())?);
        };
        *session.touched.lock().unwrap() = Instant::now();
        let headers = req.headers();
        let modified: chrono::DateTime<chrono::Utc> = session.modified.into();
        let mut response = Response::builder()
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CACHE_CONTROL, "private, no-store")
            .header(header::CONTENT_TYPE, mime.unwrap_or(session.mime))
            .header(header::ETAG, &session.etag)
            .header(
                header::LAST_MODIFIED,
                modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            );
        if !matches!(*req.method(), Method::GET | Method::HEAD) {
            return Ok(response
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header(header::ALLOW, "GET, HEAD")
                .body(Body::empty())?);
        }
        let value = |name| headers.get(name).and_then(|v| v.to_str().ok());
        let not_modified = match value(header::IF_NONE_MATCH) {
            Some(tags) => tags.split(',').any(|tag| {
                let tag = tag.trim().trim_start_matches("W/");
                tag == "*" || tag == session.etag
            }),
            None => value(header::IF_MODIFIED_SINCE)
                .and_then(parse_date)
                .is_some_and(|date| modified.timestamp() <= date),
        };
        if not_modified {
            return Ok(response
                .status(StatusCode::NOT_MODIFIED)
                .body(Body::empty())?);
        }
        let allow_range = value(header::IF_RANGE).is_none_or(|v| {
            v == session.etag || parse_date(v).is_some_and(|date| modified.timestamp() <= date)
        });
        let range = if allow_range {
            value(header::RANGE)
        } else {
            None
        };
        let (offset, length, status) = if let Some(range) = range {
            match parse_range(range, session.size) {
                Some((start, end)) => {
                    response = response.header(
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{}", session.size),
                    );
                    (start, end - start + 1, StatusCode::PARTIAL_CONTENT)
                }
                None => {
                    return Ok(response
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{}", session.size))
                        .header(header::CONTENT_LENGTH, 0)
                        .body(Body::empty())?);
                }
            }
        } else {
            (0, session.size, StatusCode::OK)
        };
        response = response
            .status(status)
            .header(header::CONTENT_LENGTH, length);
        if req.method() == Method::HEAD || length == 0 {
            return Ok(response.body(Body::empty())?);
        }
        let chunks = stream::unfold(None::<mpsc::Receiver<io::Result<Bytes>>>, move |receiver| {
            let session = session.clone();
            async move {
                let mut receiver = match receiver {
                    Some(receiver) => receiver,
                    None => {
                        let (send, receive) = mpsc::channel(16);
                        // At most four producers allocate/read blocks for this player.
                        // Queued bytes allow the source to run ahead of HTTP writes.
                        let permit = session.permits.clone().acquire_owned().await.ok()?;
                        tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            let mut offset = offset;
                            let mut remaining = length;
                            while remaining > 0 && !send.is_closed() {
                                let count = remaining.min(BLOCK - offset % BLOCK) as usize;
                                match session.read(offset, count) {
                                    Ok(bytes) => {
                                        if send.blocking_send(Ok(Bytes::from(bytes))).is_err() {
                                            break;
                                        }
                                        offset += count as u64;
                                        remaining -= count as u64;
                                    }
                                    Err(error) => {
                                        let _ = send.blocking_send(Err(error));
                                        break;
                                    }
                                }
                            }
                        });
                        receive
                    }
                };
                receiver.recv().await.map(|chunk| (chunk, Some(receiver)))
            }
        });
        Ok(response.body(Body::from_stream(chunks))?)
    }
}
fn parse_date(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.timestamp())
}
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
    if size == 0 {
        return None;
    }
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        return (suffix > 0).then(|| (size.saturating_sub(suffix), size - 1));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end && start < size).then_some((start, end))
}
impl Session {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = std::fs::remove_file(&self.path);
    }
    fn read(&self, offset: u64, count: usize) -> io::Result<Vec<u8>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "player closed"));
        }
        let block = offset / BLOCK;
        let state = self
            .blocks
            .lock()
            .unwrap()
            .entry(block)
            .or_insert_with(|| Arc::new(Mutex::new(false)))
            .clone();
        let mut ready = state.lock().unwrap();
        if !*ready {
            let start = block * BLOCK;
            let mut buffer = vec![0; (self.size - start).min(BLOCK) as usize];
            self.source.read_exact_at(&mut buffer, start)?;
            self.source_bytes
                .fetch_add(buffer.len() as u64, Ordering::Relaxed);
            // A full cache disk must not make the original recording unplayable.
            // Only mark a block reusable after every byte was written.
            *ready = self.cache.write_all_at(&buffer, start).is_ok();
            let from = (offset - start) as usize;
            buffer.copy_within(from..from + count, 0);
            buffer.truncate(count);
            return Ok(buffer);
        }
        drop(ready);
        let mut buffer = vec![0; count];
        self.cache.read_exact_at(&mut buffer, offset)?;
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    async fn get(sessions: &Sessions, id: &str, range: Option<&str>, method: Method) -> Response {
        let mut req = Request::builder().method(method);
        if let Some(range) = range {
            req = req.header(header::RANGE, range);
        }
        sessions
            .serve(id, req.body(Body::empty()).unwrap(), None)
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn lazy_fill_rewind_concurrent_ranges_and_close() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("clip.mp4");
        let data: Vec<u8> = (0..BLOCK as usize * 3 + 17)
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&source, &data).unwrap();
        let sessions = Sessions::new(temp.path().join("cache"));
        let id = sessions.create(source).await.unwrap();
        let session = sessions.sessions.lock().unwrap()[&id].clone();
        let head = get(&sessions, &id, None, Method::HEAD).await;
        assert_eq!(
            head.headers()[header::CONTENT_LENGTH],
            data.len().to_string()
        );
        assert_eq!(session.source_bytes.load(Ordering::Relaxed), 0);
        let (a, b) = tokio::join!(
            async {
                to_bytes(
                    get(&sessions, &id, Some("bytes=0-127"), Method::GET)
                        .await
                        .into_body(),
                    128,
                )
                .await
                .unwrap()
            },
            async {
                to_bytes(
                    get(&sessions, &id, Some("bytes=10-137"), Method::GET)
                        .await
                        .into_body(),
                    128,
                )
                .await
                .unwrap()
            }
        );
        assert_eq!(&a[..], &data[..128]);
        assert_eq!(&b[..], &data[10..138]);
        assert_eq!(session.source_bytes.load(Ordering::Relaxed), BLOCK);
        let complete = to_bytes(
            get(&sessions, &id, None, Method::GET).await.into_body(),
            data.len(),
        )
        .await
        .unwrap();
        assert_eq!(&complete[..], data);
        assert_eq!(
            session.source_bytes.load(Ordering::Relaxed),
            data.len() as u64
        );
        for range in ["bytes=0-127", "bytes=-17", "bytes=1048570-1048580"] {
            to_bytes(
                get(&sessions, &id, Some(range), Method::GET)
                    .await
                    .into_body(),
                data.len(),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            session.source_bytes.load(Ordering::Relaxed),
            data.len() as u64
        );
        assert!(session.path.exists());
        assert!(sessions.close(&id));
        assert!(!session.path.exists());
        assert_eq!(
            get(&sessions, &id, None, Method::GET).await.status(),
            StatusCode::NOT_FOUND
        );
        assert!(!sessions.touch(&id));
    }
    #[tokio::test]
    async fn closing_unpolled_or_inflight_responses_does_not_read_the_whole_file() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("clip.mp4");
        let file = File::create(&source).unwrap();
        file.set_len(128 * BLOCK).unwrap();
        let sessions = Sessions::new(temp.path().join("cache"));
        let id = sessions.create(source).await.unwrap();
        let session = sessions.sessions.lock().unwrap()[&id].clone();
        drop(get(&sessions, &id, None, Method::GET).await);
        assert_eq!(session.source_bytes.load(Ordering::Relaxed), 0);
        use futures_util::StreamExt;
        let mut body = get(&sessions, &id, None, Method::GET)
            .await
            .into_body()
            .into_data_stream();
        assert!(body.next().await.unwrap().is_ok());
        sessions.close(&id);
        drop(body);
        // Wait for the bounded producer to observe cancellation.
        for _ in 0..100 {
            if session.permits.available_permits() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(session.permits.available_permits(), 4);
        assert!(session.source_bytes.load(Ordering::Relaxed) <= 18 * BLOCK);
        assert!(!session.path.exists());
    }

    #[tokio::test]
    async fn source_is_pinned_and_abandoned_session_expires() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("clip.mp4");
        std::fs::write(&source, b"original recording").unwrap();
        let sessions = Sessions::new(temp.path().join("cache"));
        let id = sessions.create(source.clone()).await.unwrap();
        let replacement = temp.path().join("new.mp4");
        std::fs::write(&replacement, b"new").unwrap();
        std::fs::rename(replacement, source).unwrap();
        let response = get(&sessions, &id, None, Method::GET).await;
        let etag = response.headers()[header::ETAG].clone();
        assert_eq!(
            &to_bytes(response.into_body(), 100).await.unwrap()[..],
            b"original recording"
        );
        let req = Request::builder()
            .header(header::IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            sessions.serve(&id, req, None).await.unwrap().status(),
            StatusCode::NOT_MODIFIED
        );
        for range in ["bytes=999-", "bytes=-0", "bytes=3-1", "bytes=0-1,3-4"] {
            assert_eq!(
                get(&sessions, &id, Some(range), Method::GET).await.status(),
                StatusCode::RANGE_NOT_SATISFIABLE
            );
        }
        let session = sessions.sessions.lock().unwrap()[&id].clone();
        *session.touched.lock().unwrap() = Instant::now() - TTL - Duration::from_secs(1);
        assert_eq!(sessions.reap_expired(), 1);
        assert!(!session.path.exists());
    }
}
