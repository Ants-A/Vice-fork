//! Daemon ownership, Unix IPC and recording supervision.
use crate::{
    config,
    hotkey::{HotkeyEvent, HotkeyListener},
    runtime,
    server::{self, AppState, Shared},
};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, UnixListener, UnixStream},
    sync::mpsc,
};

pub async fn ipc(command: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut socket = UnixStream::connect(runtime::socket_path())
            .await
            .context("Vice is not running. Start it with vice start.")?;
        socket.write_all(format!("{command}\n").as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).await?;
        Ok(line.trim().into())
    })
    .await
    .context("daemon command timed out")?
}
struct Owner;
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(runtime::socket_path());
        let _ = std::fs::remove_file(runtime::pid_path());
    }
}
pub async fn run(open_ui: bool) -> Result<()> {
    let _lock = runtime::claim_daemon_lock(&runtime::socket_path())?;
    if runtime::daemon_is_running(&runtime::socket_path(), &runtime::pid_path()) {
        bail!("Vice is already running");
    }
    let cfg = config::load()?;
    cfg.validate()?;
    crate::recorder::reap_orphaned_captures();
    for dir in [&cfg.output.directory, &cfg.output.image_directory] {
        std::fs::create_dir_all(runtime::resolve_path(dir))
            .with_context(|| format!("cannot use output folder {dir}"))?;
    }
    let socket_path = runtime::socket_path();
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let socket = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(runtime::pid_path(), std::process::id().to_string())?;
    let _owner = Owner;
    let state = AppState::new(cfg.clone());
    state.rescan().await;
    let recover = state.clone();
    tokio::spawn(async move {
        if let Err(error) = server::recover_library(recover).await {
            tracing::warn!(%error,"library recovery failed");
        }
    });
    state.overlay.lock().await.set_events(state.events.clone());
    if let Err(error) = state.overlay.lock().await.configure(&cfg).await {
        tracing::warn!(%error,"keyboard overlay failed to start");
    }
    let local = TcpListener::bind(("127.0.0.1", cfg.sharing.port))
        .await
        .context("cannot bind local UI port")?;
    let mut tasks = tokio::task::JoinSet::new();
    let stop = state.shutdown.clone();
    let s = state.clone();
    let mut shutdown = stop.subscribe();
    tasks.spawn(async move {
        axum::serve(local, server::local_router(s))
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
            })
            .await
    });
    if cfg.sharing.enabled {
        let public = TcpListener::bind((
            "0.0.0.0",
            cfg.sharing
                .public_port
                .unwrap_or(cfg.sharing.port.saturating_add(1)),
        ))
        .await
        .context("cannot bind public share port")?;
        let s = state.clone();
        let mut shutdown = stop.subscribe();
        tasks.spawn(async move {
            axum::serve(public, server::public_router(s))
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await
        });
    }
    let ipc_state = state.clone();
    let ipc_task = tokio::spawn(async move {
        while let Ok((stream, _)) = socket.accept().await {
            let state = ipc_state.clone();
            tokio::spawn(async move {
                let _ = handle_ipc(state, stream).await;
            });
        }
    });
    let recording_state = state.clone();
    let recorder_task = tokio::spawn(async move {
        match recording_state.recorder.lock().await.start().await {
            Ok(()) => *recording_state.recorder_error.write().await = None,
            Err(e) => {
                tracing::error!(%e,"recording failed to start");
                *recording_state.recorder_error.write().await = Some(e.to_string());
            }
        }
    });
    if open_ui {
        open_window()?;
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut listener = HotkeyListener::start(cfg.clone(), tx.clone());
    let start = Instant::now();
    let mut keys = crate::keys::KeyRing::new(cfg.recording.buffer_duration as f64 + 10.0);
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    let mut shutdown = stop.subscribe();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut config_events = state.events.subscribe();
    let mut signatures = serde_json::to_string(&cfg.hotkeys)?;
    let presence_state = state.clone();
    let presence_task = tokio::spawn(crate::discord_rpc::presence_loop(presence_state));
    let update_task = if cfg.updates.check_on_start {
        let update_state = state.clone();
        Some(tokio::spawn(async move {
            if let Some(update) = crate::updates::check(false).await {
                let mut event = update;
                event["type"] = json!("update_available");
                update_state.emit(event);
            }
        }))
    } else {
        None
    };
    let public_enabled = cfg.sharing.enabled;
    let mut tunnel_task = if cfg.sharing.enabled && cfg.sharing.cloudflare_tunnel {
        let s = state.clone();
        Some(tokio::spawn(tunnel(s)))
    } else {
        None
    };
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            _=terminate.recv()=>break,
            _=interrupt.recv()=>break,
            event=config_events.recv()=>{if let Ok(event)=event {if event["type"]=="key_overlay_moved" {let mut cfg=state.cfg.write().await;cfg.overlay.keys_position="custom".into();cfg.overlay.keys_x=event["x"].as_i64().unwrap_or(32).max(0);cfg.overlay.keys_y=event["y"].as_i64().unwrap_or(32).max(0);let _=config::save(&cfg);}else if event["type"]=="config_changed" {let cfg=state.cfg.read().await.clone();let _=state.overlay.lock().await.configure(&cfg).await;
            if public_enabled && cfg.sharing.enabled && cfg.sharing.cloudflare_tunnel {if tunnel_task.is_none(){let s=state.clone();tunnel_task=Some(tokio::spawn(tunnel(s)));}} else {if let Some(task)=tunnel_task.take(){task.abort();}*state.tunnel_url.write().await=None;state.broadcast_share_links().await;}
            keys.horizon=cfg.recording.buffer_duration as f64+10.;let new=serde_json::to_string(&cfg.hotkeys)?;if new!=signatures{listener.stop().await;listener=HotkeyListener::start(cfg,tx.clone());signatures=new;}}}},
            event=rx.recv()=>if let Some(event)=event {match event {
                HotkeyEvent::Availability(available)=>{state.hotkeys_available.store(available,std::sync::atomic::Ordering::Relaxed);},
                HotkeyEvent::Key{name,down}=>{keys.record(&name,down,start.elapsed().as_secs_f64());state.overlay.lock().await.key(&name,down).await;},
                HotkeyEvent::Clip(duration)=>{let s=state.clone();let end=start.elapsed().as_secs_f64();let intervals=keys.intervals(end-duration as f64,end);tokio::spawn(async move {
                    if hotkeys_suppressed(&s).await {return;}
                    let elapsed={let rec=s.recorder.lock().await;if rec.session_active(){Some(rec.session_elapsed())}else{None}};
                    if let Some(elapsed)=elapsed {
                        let cfg=s.cfg.read().await.clone();let mut highlights=s.session_highlights.lock().await;
                        let label=if highlights.is_empty(){"Highlight".into()}else{format!("Highlight {}",highlights.len()+1)};
                        let time=(elapsed*1000.).round()/1000.;highlights.push(json!({"time":time,"label":label,"color":"#f59e0b"}));
                        s.emit(json!({"type":"session_highlight","time":time,"label":label,"color":"#f59e0b"}));
                        crate::audio::play("highlight",cfg.notifications.sound_volume,cfg.notifications.highlight_sound.as_deref());return;
                    }
                    if let Ok(path)=s.save_clip(Some(duration as u64)).await {let cfg=s.cfg.read().await.clone();if cfg.overlay.keys_in_clips{let _=crate::keys::save_sidecar(&s.media_id(&path, false).await,&intervals);}}
                });},
                HotkeyEvent::SessionToggle=>{let s=state.clone();tokio::spawn(async move {if !hotkeys_suppressed(&s).await {let _=toggle_session(&s).await;}});},
                HotkeyEvent::RecordingToggle=>{let s=state.clone();tokio::spawn(async move {
                    if hotkeys_suppressed(&s).await{return;}
                    let session=s.recorder.lock().await.session_active();if session{let _=toggle_session(&s).await;}
                    let mut rec=s.recorder.lock().await;
                    if rec.is_healthy(){s.paused.store(true,std::sync::atomic::Ordering::Relaxed);let _=rec.stop().await;}
                    else{s.paused.store(false,std::sync::atomic::Ordering::Relaxed);if let Err(e)=rec.start().await{*s.recorder_error.write().await=Some(e.to_string());}}
                });},
                HotkeyEvent::Screenshot=>{let s=state.clone();tokio::spawn(async move {if hotkeys_suppressed(&s).await{return;}let cfg=s.cfg.read().await.clone();match crate::recorder::capture_screenshot(&cfg).await{Ok(p)=>{let _=s.saved(&p,true).await;},Err(e)=>{crate::audio::play("clip_failed",cfg.notifications.sound_volume,cfg.notifications.clip_failed_sound.as_deref());s.emit(json!({"type":"image_error","error":e.to_string()}));}}});},
                HotkeyEvent::OverlayToggle=>{if hotkeys_suppressed(&state).await{continue;}let mut cfg=state.cfg.write().await;cfg.overlay.keys = !cfg.overlay.keys;let _=config::save(&cfg);let result=state.overlay.lock().await.configure(&cfg).await;state.emit(json!({"type":"key_overlay","running":cfg.overlay.keys&&result.is_ok(),"error":result.err().map(|e|e.to_string()),"moving":false}));}
            }},
            _=interval.tick()=>{
                let cfg=state.cfg.read().await.clone();
                if let Ok(mut rec)=state.recorder.try_lock(){
                    let paused=state.paused.load(std::sync::atomic::Ordering::Relaxed);
                    if !paused && !rec.is_healthy() {match rec.start().await{Ok(())=>*state.recorder_error.write().await=None,Err(e)=>*state.recorder_error.write().await=Some(e.to_string())}}
                    let win=crate::active_window::get_active_window().await;
                    rec.game_tag=win.as_ref().and_then(|win|crate::active_window::match_game(win,&cfg.discord.custom_games));
                    let mut retarget=false;
                    if !rec.session_active() {
                    if cfg.recording.follow_mouse_display&& let Some(display)=crate::active_window::pointer_display().await&& rec.display_override.as_ref()!=Some(&display){rec.display_override=Some(display);retarget=true;}
                    if cfg.recording.window_capture && crate::recorder::gsr_window_capture_supported() {
                        // Keep the game pinned while an unrelated app has focus.
                        if rec.game_tag.is_some()&& let Some(id)=win.and_then(|w|w.window_id)&& rec.window_override.as_ref()!=Some(&id){rec.window_override=Some(id);retarget=true;}
                    } else if rec.window_override.take().is_some(){retarget=true;}
                    }
                    if retarget && !paused && !rec.session_active(){let _=rec.stop().await;if let Err(e)=rec.start().await{*state.recorder_error.write().await=Some(e.to_string());}}
                }
                let added=state.rescan().await;if added>0{state.emit(json!({"type":"library_changed","added":added}));let recover=state.clone();tokio::spawn(async move {let _=server::recover_library(recover).await;});}
                let mut status=state.status().await;status["type"]=json!("status");state.emit(status);
            }
        }
    }
    let _ = stop.send(true);
    state.overlay.lock().await.stop().await;
    listener.stop().await;
    presence_task.abort();
    if let Some(task) = update_task {
        task.abort();
    }
    ipc_task.abort();
    recorder_task.abort();
    if let Some(task) = tunnel_task {
        task.abort();
    }
    if let Some((_, cancel)) = state.export.lock().await.as_ref() {
        let _ = cancel.send(true);
    }
    server::wait_for_export(&state).await;
    let session = state.recorder.lock().await.session_active();
    if session {
        let _ = toggle_session(&state).await;
    }
    let _ = state.recorder.lock().await.stop().await;
    state.flush_metadata(true).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.abort_all();
    Ok(())
}
pub fn open_window() -> Result<()> {
    let exe = std::env::current_exe()?;
    let app = exe.with_file_name("vice-app");
    std::process::Command::new(app)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}
async fn toggle_session(state: &Shared) -> Result<()> {
    let cfg = state.cfg.read().await.clone();
    let mut rec = state.recorder.lock().await;
    if rec.session_active() {
        let p = rec.stop_session().await?;
        drop(rec);
        let mut highlights = state.session_highlights.lock().await;
        let entries = std::mem::take(&mut *highlights);
        drop(highlights);
        state.saved(&p, false).await?;
        if !entries.is_empty() {
            let entries: Vec<_> = entries
                .into_iter()
                .enumerate()
                .map(|(i, mut value)| {
                    value["id"] = json!((i + 1).to_string());
                    value
                })
                .collect();
            let slug = state.media_id(&p, false).await;
            server::write_json(
                &server::data_dir()
                    .join("highlights")
                    .join(format!("{slug}.json")),
                &json!(entries),
            )?;
        }
        crate::audio::play(
            "session_end",
            cfg.notifications.sound_volume,
            cfg.notifications.session_end_sound.as_deref(),
        );
        state.emit(json!({"type":"session_stop"}));
    } else {
        let path = rec.start_session().await?;
        state.session_highlights.lock().await.clear();
        crate::audio::play(
            "session_start",
            cfg.notifications.sound_volume,
            cfg.notifications.session_start_sound.as_deref(),
        );
        state.emit(json!({"type":"session_start","path":path}));
    }
    Ok(())
}
async fn hotkeys_suppressed(state: &Shared) -> bool {
    let cfg = state.cfg.read().await;
    let patterns = cfg.hotkeys.disable_while_focused.clone();
    drop(cfg);
    if patterns.is_empty() {
        return false;
    }
    crate::active_window::get_active_window()
        .await
        .is_some_and(|win| crate::active_window::focus_blocked(&win, &patterns))
}
async fn handle_ipc(state: Shared, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(reader).take(4096).read_line(&mut line),
    )
    .await??;
    let mut args = line.split_whitespace();
    let cmd = args.next().unwrap_or("");
    let response = match cmd {
        "status" => state.status().await.to_string(),
        "stop" | "quit" => {
            let _ = state.shutdown.send(true);
            "OK stopping".into()
        }
        "clip" => {
            let duration = match args.next() {
                Some(v) => Some(v.parse::<u64>().context("invalid duration")?),
                None => None,
            };
            if duration == Some(0) || duration.is_some_and(|v| v > 1800) {
                "ERROR duration must be 1..1800 seconds".into()
            } else {
                match state.save_clip(duration).await {
                    Ok(p) => format!("OK {}", p.display()),
                    Err(e) => format!("ERROR {e}"),
                }
            }
        }
        "toggle" | "session" => match toggle_session(&state).await {
            Ok(()) => "OK".into(),
            Err(e) => format!("ERROR {e}"),
        },
        "ui" => {
            open_window()?;
            "OK".into()
        }
        _ => "ERROR unknown command".into(),
    };
    writer.write_all(format!("{response}\n").as_bytes()).await?;
    Ok(())
}
async fn tunnel(state: Shared) {
    let re = regex::Regex::new(r"https://([a-zA-Z0-9-]+)\.trycloudflare\.com")
        .expect("constant tunnel URL expression");
    loop {
        let port = state.active_public_port;
        let result = async {
            let mut child = tokio::process::Command::new("cloudflared")
                .args([
                    "tunnel",
                    "--url",
                    &format!("http://127.0.0.1:{port}"),
                    "--no-autoupdate",
                ])
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut lines = BufReader::new(child.stderr.take().context("tunnel stderr")?).lines();
            while let Some(line) = lines.next_line().await? {
                if let Some(c) = re.captures(&line)
                    && !["api", "www", "dash", "developers", "blog"].contains(&&c[1])
                {
                    let url = c[0].to_owned();
                    *state.tunnel_url.write().await = Some(url.clone());
                    state.emit(json!({"type":"tunnel_url","url":url}));
                    state.broadcast_share_links().await;
                }
            }
            child.wait().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        *state.tunnel_url.write().await = None;
        state.broadcast_share_links().await;
        state.emit(json!({"type":"tunnel_error","error":result.err().map(|e|e.to_string()).unwrap_or("tunnel exited".into())}));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
