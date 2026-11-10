//! Progressive previews: begin playback before an entire remote clip is encoded.
use crate::server::{self, Shared};
use anyhow::{Context, Result, bail};
use axum::{
    body::{Body, Bytes},
    http::{Request, Response, header},
};
use futures_util::stream;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::watch};

#[derive(Clone, Debug)]
enum Phase {
    Running(usize),
    Complete(usize),
    Failed(usize, String),
}
impl Phase {
    fn epoch(&self) -> usize {
        match self {
            Self::Running(n) | Self::Complete(n) | Self::Failed(n, _) => *n,
        }
    }
}
pub struct PreviewJob {
    temporary: PathBuf,
    target: PathBuf,
    phase: watch::Sender<Phase>,
    cancel: watch::Sender<bool>,
    readers: AtomicUsize,
    last_reader: Mutex<Instant>,
    created: Instant,
}
struct Reader(Arc<PreviewJob>);
impl Drop for Reader {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut at) = self.0.last_reader.lock() {
            *at = Instant::now();
        }
    }
}
struct Activity(Shared);
impl Drop for Activity {
    fn drop(&mut self) {
        self.0.active_video_streams.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut at) = self.0.last_video_request.lock() {
            *at = Some(Instant::now());
        }
    }
}

pub async fn serve(
    state: Shared,
    source: PathBuf,
    mut args: Vec<String>,
    ext: &str,
    req: Request<Body>,
) -> Result<Response<Body>> {
    let key = server::indexed_cache_key(&state, &source).await;
    let target = server::cache_dir()
        .join("proxies")
        .join(format!("{key}-stream720-v4.{ext}"));
    if target.metadata().is_ok_and(|m| m.len() > 0) {
        return server::video_file(state, target, req)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"));
    }
    // Each fragment/cluster carries a keyframe. Both formats can play while the
    // file grows; completed files retain normal HTTP range support.
    if ext == "mp4" {
        if let Some(i) = args.iter().position(|a| a == "-movflags") {
            args[i + 1] = "+empty_moov+frag_keyframe+default_base_moof".into();
        }
        args.extend([
            "-frag_duration".into(),
            "1000000".into(),
            "-g".into(),
            "30".into(),
        ]);
    } else {
        args.extend([
            "-cluster_time_limit".into(),
            "1000".into(),
            "-cluster_size_limit".into(),
            "256000".into(),
        ]);
    }
    args.extend(["-flush_packets".into(), "1".into()]);
    let job = {
        let mut jobs = state.preview_jobs.lock().await;
        jobs.retain(|_, j| {
            matches!(*j.phase.borrow(), Phase::Running(_))
                || j.created.elapsed() < Duration::from_secs(5)
        });
        if let Some(job) = jobs.get(&target) {
            job.clone()
        } else {
            tokio::fs::create_dir_all(target.parent().context("preview directory")?).await?;
            let (phase, _) = watch::channel(Phase::Running(0));
            let (cancel, _) = watch::channel(false);
            let job = Arc::new(PreviewJob {
                temporary: target.with_file_name(format!(
                    ".vice-stream-{}.{}",
                    uuid::Uuid::new_v4(),
                    ext
                )),
                target: target.clone(),
                phase,
                cancel,
                readers: AtomicUsize::new(0),
                last_reader: Mutex::new(Instant::now()),
                created: Instant::now(),
            });
            jobs.insert(target, job.clone());
            args.extend(["-y".into(), job.temporary.to_string_lossy().into_owned()]);
            spawn(state.clone(), source, args, job.clone());
            job
        }
    };
    body(job, ext)
}
fn spawn(state: Shared, source: PathBuf, args: Vec<String>, job: Arc<PreviewJob>) {
    let monitor = job.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !matches!(*monitor.phase.borrow(), Phase::Running(_)) {
                break;
            }
            if monitor.readers.load(Ordering::Relaxed) == 0
                && monitor
                    .last_reader
                    .lock()
                    .is_ok_and(|at| at.elapsed() > Duration::from_secs(5))
            {
                monitor.cancel.send_replace(true);
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut canceled = job.cancel.subscribe();
        let mut shutdown = state.shutdown.subscribe();
        state.active_video_streams.fetch_add(1, Ordering::Relaxed);
        let _activity = Activity(state.clone());
        let source_key = server::indexed_cache_key(&state, &source).await;
        let result:Result<()>=async {
            let _permit=tokio::select!{slot=state.transcodes.acquire()=>slot?,_=canceled.changed()=>bail!("preview closed"),_=shutdown.changed()=>bail!("preview stopped")};
            let mut failure=None;
            for (i,command) in crate::encoding::command_candidates(&args).await.into_iter().enumerate(){
                if *canceled.borrow()||*shutdown.borrow(){bail!("preview stopped");}
                let _=tokio::fs::remove_file(&job.temporary).await;
                job.phase.send_replace(Phase::Running(i+1));
                let attempt=tokio::select!{
                    result=server::run_preview_attempt(&state,&source,command,600)=>result,
                    _=canceled.changed()=>bail!("preview closed"),
                    _=shutdown.changed()=>bail!("preview stopped"),
                };
                match attempt {Ok(())=>{
                    if !job.temporary.metadata().is_ok_and(|m|m.len()>0){bail!("preview encoder produced no video");}
                    let owned = source.clone();
                    let current = tokio::task::spawn_blocking(move || server::cache_key(&owned)).await?;
                    if source_key != current { bail!("clip changed during preview preparation; reopen it"); }
                    tokio::fs::rename(&job.temporary,&job.target).await?;
                    job.phase.send_replace(Phase::Complete(i+1));
                    state.emit(serde_json::json!({"type":"preview_ready","slug":source.file_stem().unwrap_or_default().to_string_lossy()}));
                    return Ok(());
                },Err(error)=>{tracing::warn!(%error,"preview encoder failed; retrying");failure=Some(error);}}
            }
            Err(failure.unwrap_or_else(||anyhow::anyhow!("no preview encoder")))
        }.await;
        if let Err(error) = result {
            tracing::warn!(%error,"preview stopped");
            let epoch = job.phase.borrow().epoch();
            job.phase
                .send_replace(Phase::Failed(epoch, error.to_string()));
        }
        let _ = tokio::fs::remove_file(&job.temporary).await;
    });
}
fn body(job: Arc<PreviewJob>, ext: &str) -> Result<Response<Body>> {
    job.readers.fetch_add(1, Ordering::Relaxed);
    struct Cursor {
        reader: Reader,
        file: Option<tokio::fs::File>,
        epoch: Option<usize>,
        position: u64,
    }
    let cursor = Cursor {
        reader: Reader(job),
        file: None,
        epoch: None,
        position: 0,
    };
    let chunks = stream::try_unfold(cursor, |mut cursor| async move {
        loop {
            let job = &cursor.reader.0;
            let phase = job.phase.borrow().clone();
            if let Phase::Failed(_, ref error) = phase {
                return Err(std::io::Error::other(error.clone()));
            }
            if cursor.epoch.is_some_and(|n| n != phase.epoch()) {
                if cursor.position > 0 {
                    return Err(std::io::Error::other(
                        "preview encoder restarted; reload preview",
                    ));
                }
                cursor.file = None;
            }
            if cursor.file.is_none() {
                let path = if matches!(phase, Phase::Complete(_)) {
                    &job.target
                } else {
                    &job.temporary
                };
                match tokio::fs::File::open(path).await {
                    Ok(file) => {
                        cursor.file = Some(file);
                        cursor.epoch = Some(phase.epoch());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let mut bytes = vec![0u8; 64 * 1024];
            let count = cursor.file.as_mut().unwrap().read(&mut bytes).await?;
            if count > 0 {
                bytes.truncate(count);
                cursor.position += count as u64;
                return Ok(Some((Bytes::from(bytes), cursor)));
            }
            if matches!(phase, Phase::Complete(_)) {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    Ok(Response::builder()
        .header(
            header::CONTENT_TYPE,
            if ext == "mp4" {
                "video/mp4"
            } else {
                "video/webm"
            },
        )
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCEPT_RANGES, "none")
        .body(Body::from_stream(chunks))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn delivers_fragments_before_encode_finishes_and_keeps_completed_cache() -> Result<()> {
        if crate::runtime::which("ffmpeg").is_none() {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.mp4");
        let args = [
            "ffmpeg",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x90:r=15",
            "-t",
            "4",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-g",
            "15",
            "-y",
        ]
        .map(str::to_owned);
        let mut make = args.to_vec();
        make.push(source.to_string_lossy().into_owned());
        server::run_command(make, 15).await?;
        let state = crate::server::AppState::new(crate::config::Config::default());
        let target = directory.path().join("preview.mp4");
        let (phase, _) = watch::channel(Phase::Running(0));
        let (cancel, _) = watch::channel(false);
        let job = Arc::new(PreviewJob {
            temporary: directory.path().join("working.mp4"),
            target: target.clone(),
            phase,
            cancel,
            readers: AtomicUsize::new(0),
            last_reader: Mutex::new(Instant::now()),
            created: Instant::now(),
        });
        let args = vec![
            "ffmpeg".into(),
            "-v".into(),
            "error".into(),
            "-re".into(),
            "-i".into(),
            source.to_string_lossy().into_owned(),
            "-c:v".into(),
            "copy".into(),
            "-an".into(),
            "-movflags".into(),
            "+empty_moov+frag_keyframe+default_base_moof".into(),
            "-frag_duration".into(),
            "1000000".into(),
            "-flush_packets".into(),
            "1".into(),
            "-y".into(),
            job.temporary.to_string_lossy().into_owned(),
        ];
        spawn(state, source, args, job.clone());
        let mut stream = body(job.clone(), "mp4")?.into_body().into_data_stream();
        use futures_util::StreamExt;
        let first = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await?
            .context("missing first fragment")??;
        assert!(!first.is_empty());
        assert!(
            matches!(*job.phase.borrow(), Phase::Running(_)),
            "should begin before conversion completes"
        );
        while let Some(chunk) = stream.next().await {
            chunk?;
        }
        assert!(target.exists());
        assert!(matches!(*job.phase.borrow(), Phase::Complete(_)));
        assert!(crate::media::probe_media(&target).await?.duration > 3.5);
        Ok(())
    }
}
