//! Native GTK/WebKit desktop shell. The daemon remains alive after closing it.
use anyhow::{Context, Result, bail};
use std::{
    io::{BufRead, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};
use vice::{config, runtime};
fn probe() -> Option<serde_json::Value> {
    let mut socket = UnixStream::connect(runtime::socket_path()).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    socket.write_all(b"status\n").ok()?;
    let mut line = String::new();
    std::io::BufReader::new(socket).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}
fn ensure_server() -> Result<String> {
    if let Some(status) = probe() {
        return Ok(status["local_url"]
            .as_str()
            .context("daemon did not return a UI address")?
            .into());
    }
    let cfg = config::load()?;
    if runtime::systemd_unit_loaded("vice.service") {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "start", "vice.service"])
            .status();
    } else {
        let exe = std::env::current_exe()?.with_file_name("vice");
        std::fs::create_dir_all(runtime::data_dir())?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(runtime::data_dir().join("vice.log"))?;
        use std::os::unix::process::CommandExt;
        std::process::Command::new(exe)
            .args(["start", "--no-open-ui"])
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()?;
    }
    let deadline = Instant::now() + Duration::from_secs(25);
    let url = format!("http://127.0.0.1:{}", cfg.sharing.port);
    while Instant::now() < deadline {
        if let Some(status) = probe() {
            return Ok(status["local_url"].as_str().unwrap_or(&url).into());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!(
        "Vice did not start. See {} or run vice doctor.",
        runtime::data_dir().join("vice.log").display()
    )
}
fn main() -> Result<()> {
    runtime::normalize_runtime_environment();
    if std::env::args().any(|arg| arg == "--key-overlay") {
        return vice::key_overlay::run();
    }
    // WebKitGTK's download buffer otherwise waits for up to five seconds of
    // media after an underflow. Keep native startup/rebuffering responsive,
    // independently of the server's larger read-ahead window. This shared
    // WebKit GStreamer setting applies to GTK as well as WPE:
    // Source/WebCore/platform/graphics/gstreamer/MediaPlayerPrivateGStreamer.cpp
    // (configureDownloadBuffer). Preserve an explicit environment override.
    if std::env::var_os("WPE_SHELL_MEDIA_DISK_CACHE_SIZE_NSEC").is_none() {
        // The application is still single-threaded, before GTK/WebKit starts.
        unsafe {
            std::env::set_var("WPE_SHELL_MEDIA_DISK_CACHE_SIZE_NSEC", "1000000000");
        }
    }
    let nvidia = vice::native_video::nvidia_gpu();
    let ranks = std::env::var("GST_PLUGIN_FEATURE_RANK").unwrap_or_default();
    for (key, value) in
        vice::native_video::environment(config::load()?.ui.hardware_video_decode, nvidia, &ranks)
    {
        // Apply before GTK/WebKit or any video subprocess starts.
        unsafe {
            std::env::set_var(key, value);
        }
    }
    if nvidia {
        eprintln!(
            "[vice-app] NVIDIA compatibility playback: system-memory video frames; FFmpeg GPU encoding remains available"
        );
    }
    let url = ensure_server()?;
    #[cfg(feature = "desktop")]
    {
        native(url)
    }
    #[cfg(not(feature = "desktop"))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }
}
#[cfg(feature = "desktop")]
fn native(url: String) -> Result<()> {
    use tao::platform::unix::WindowExtUnix;
    use tao::{
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder},
        window::WindowBuilder,
    };
    use wry::WebViewBuilderExtUnix;
    #[derive(Debug)]
    enum Action {
        Show,
        KeepRunning,
        Quit,
        Labels(String, String),
        Scale(f64),
    }
    let lock_path = runtime::runtime_dir().join("vice-app.sock");
    let lock = runtime::claim_daemon_lock(&lock_path);
    if lock.is_err() {
        let _ = UnixStream::connect(&lock_path);
        return Ok(());
    }
    let _lock = lock?;
    let _ = std::fs::remove_file(&lock_path);
    let socket = std::os::unix::net::UnixListener::bind(&lock_path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;
    let event_loop = EventLoopBuilder::<Action>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let tray_proxy = proxy.clone();
    let show = proxy.clone();
    std::thread::spawn(move || {
        while socket.accept().is_ok() {
            let _ = show.send_event(Action::Show);
        }
    });
    let window = WindowBuilder::new()
        .with_title("Vice")
        .with_decorations(false)
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 820.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(800.0, 560.0))
        .build(&event_loop)?;
    let base = url.clone();
    let nav_base = url.clone();
    let script = r#"(()=>{const send=(method,args=[])=>window.ipc.postMessage(JSON.stringify({method,args}));window.pywebview={api:{keep_running:()=>send('keep_running'),quit_app:()=>send('quit_app'),open_url:url=>send('open_url',[url]),log_debug:msg=>send('log_debug',[msg]),set_tray_labels:(a,b)=>send('set_tray_labels',[a,b]),set_ui_scale:scale=>send('set_ui_scale',[scale])}};window.dispatchEvent(new Event('pywebviewready'));})();"#;
    let view = wry::WebViewBuilder::new()
        .with_url(format!("{url}/?native=1"))
        .with_initialization_script(script)
        .with_navigation_handler(move |url| {
            let allowed = url == nav_base || url.starts_with(&format!("{nav_base}/"));
            if !allowed && (url.starts_with("https://") || url.starts_with("http://")) {
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            allowed
        })
        .with_new_window_req_handler(|url, _| {
            if url.starts_with("https://") || url.starts_with("http://") {
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            wry::NewWindowResponse::Deny
        })
        .with_ipc_handler(move |req| {
            if !req.uri().to_string().starts_with(&base) {
                return;
            }
            if let Ok(body) = serde_json::from_str::<serde_json::Value>(req.body()) {
                match body["method"].as_str().unwrap_or("") {
                    "keep_running" => {
                        let _ = proxy.send_event(Action::KeepRunning);
                    }
                    "quit_app" => {
                        let _ = proxy.send_event(Action::Quit);
                    }
                    "set_ui_scale" => {
                        if let Some(scale) = body["args"][0].as_f64().filter(|s| s.is_finite()) {
                            let _ = proxy.send_event(Action::Scale(scale.clamp(0.75, 2.0)));
                        }
                    }
                    "set_tray_labels" => {
                        let _ = proxy.send_event(Action::Labels(
                            body["args"][0].as_str().unwrap_or("Open Vice").into(),
                            body["args"][1].as_str().unwrap_or("Quit Vice").into(),
                        ));
                    }
                    "open_url" => {
                        if let Some(url) = body["args"][0].as_str()
                            && (url.starts_with("https://") || url.starts_with("http://"))
                        {
                            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                        }
                    }
                    "log_debug" => {
                        eprintln!("[webview] {}", body["args"][0].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
        })
        .build_gtk(window.default_vbox().context("GTK content box")?)?;
    view.zoom(config::load()?.ui.scale)?;
    let show_tray = tray_proxy.clone();
    let tray = vice::tray::Tray::new(
        "vice",
        move || {
            let _ = show_tray.send_event(Action::Show);
        },
        move || {
            let _ = tray_proxy.send_event(Action::Quit);
        },
    );
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Action::Scale(scale)) => {
                let _ = view.zoom(scale);
            }
            Event::UserEvent(Action::Show) => {
                window.set_visible(true);
                window.set_focus();
            }
            Event::UserEvent(Action::KeepRunning) => {
                if tray.is_some() {
                    window.set_visible(false);
                } else {
                    let _ = std::fs::remove_file(&lock_path);
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(Action::Labels(open, quit)) => {
                if let Some(tray) = &tray {
                    tray.set_labels(&open, &quit);
                }
            }
            Event::UserEvent(Action::Quit) => {
                if let Ok(mut s) = UnixStream::connect(runtime::socket_path()) {
                    let _ = s.write_all(b"stop\n");
                }
                let _ = std::fs::remove_file(&lock_path);
                *flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                let _ = std::fs::remove_file(&lock_path);
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
