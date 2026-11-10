use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use vice::{config, daemon, runtime};

#[derive(Parser)]
#[command(name="vice",version=vice::VERSION,about="Instant-replay game clipping for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Start the daemon in the foreground.
    Start {
        #[arg(long)]
        debug: bool,
        #[arg(long,default_value_t=true,action=clap::ArgAction::Set, num_args=0..=1,default_missing_value="true")]
        open_ui: bool,
        #[arg(long)]
        no_open_ui: bool,
    },
    /// Open the native desktop interface.
    Ui,
    /// Save the last seconds from the replay buffer.
    Clip {
        #[arg(value_parser=clap::value_parser!(u64).range(1..=1800))]
        seconds: Option<u64>,
        #[arg(short='d',long,value_parser=clap::value_parser!(u64).range(1..=1800))]
        duration: Option<u64>,
    },
    Stop,
    Status,
    Doctor,
    Config,
    OpenConfig,
    ListKeys {
        #[arg(long, default_value = "")]
        filter: String,
    },
    Clips,
    /// Toggle full-session recording.
    Session,
    /// Remove a local user installation, preserving recordings and settings.
    Uninstall {
        #[arg(long, short = 'y')]
        yes: bool,
    },
}
fn main() -> Result<()> {
    runtime::normalize_runtime_environment();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}
async fn run(cli: Cli) -> Result<()> {
    let debug = matches!(cli.command, Some(Command::Start { debug: true, .. }));
    tracing_subscriber::fmt()
        .with_env_filter(if debug { "vice=debug" } else { "vice=info" })
        .with_writer(std::io::stderr)
        .init();
    match cli.command.unwrap_or(Command::Ui) {
        Command::Start {
            open_ui,
            no_open_ui,
            ..
        } => daemon::run(open_ui && !no_open_ui).await?,
        Command::Ui => daemon::open_window()?,
        Command::Clip { seconds, duration } => {
            if seconds.is_some() && duration.is_some() && seconds != duration {
                bail!("Give the duration once, as SECONDS or --duration.");
            }
            let seconds = duration.or(seconds);
            let command = seconds
                .map(|s| format!("clip {s}"))
                .unwrap_or("clip".into());
            let response = daemon::ipc(&command).await?;
            if response.starts_with("ERROR") || response.starts_with("error:") {
                bail!("{response}");
            }
            println!("{response}");
        }
        Command::Stop => println!("{}", daemon::ipc("stop").await?),
        Command::Status => println!("{}", daemon::ipc("status").await?),
        Command::Session => println!("{}", daemon::ipc("session").await?),
        Command::Config => println!("{}", toml::to_string_pretty(&config::load()?)?),
        Command::OpenConfig => {
            if !config::config_path().exists() {
                config::save(&config::load()?)?;
            }
            std::process::Command::new("xdg-open")
                .arg(config::config_path())
                .spawn()?;
        }
        Command::ListKeys { filter } => {
            for key in vice::hotkey::list_available_keys() {
                if key.to_lowercase().contains(&filter.to_lowercase()) {
                    println!("{key}");
                }
            }
        }
        Command::Clips => {
            let cfg = config::load()?;
            let dir = runtime::resolve_path(cfg.output.directory);
            if !dir.is_dir() {
                println!("No clips directory found.");
            } else {
                let mut files: Vec<_> = std::fs::read_dir(dir)?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension().is_some_and(|e| {
                            ["mp4", "mkv", "mov", "webm"].contains(&e.to_string_lossy().as_ref())
                        })
                    })
                    .collect();
                files.sort_by_key(|p| {
                    std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok())
                });
                if files.is_empty() {
                    println!("No clips saved yet.");
                }
                for path in files {
                    let size = path.metadata()?.len() as f64 / (1024. * 1024.);
                    println!(
                        "{}  ({size:.1} MB)",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }
        Command::Uninstall { yes } => uninstall(yes).await?,
        Command::Doctor => {
            println!(
                "Vice {} (Rust)\nConfig: {}\nSocket: {}",
                vice::VERSION,
                config::config_path().display(),
                runtime::socket_path().display()
            );
            for tool in [
                "ffmpeg",
                "ffprobe",
                "gpu-screen-recorder",
                "wf-recorder",
                "cloudflared",
                "wl-copy",
                "xclip",
                "xdotool",
                "xprop",
                "wmctrl",
            ] {
                println!(
                    "{tool}: {}",
                    if runtime::host_which(tool) {
                        "available"
                    } else {
                        "missing"
                    }
                );
            }
            let encoders = vice::encoding::video_candidates().await;
            println!(
                "Video encoding: {}",
                encoders
                    .iter()
                    .map(|encoder| format!(
                        "{} ({})",
                        encoder.name(),
                        if encoder.is_hardware() {
                            "GPU"
                        } else {
                            "CPU fallback"
                        }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("Hotkeys: {}", vice::hotkey::can_access_hotkeys());
            println!(
                "Daemon: {}",
                daemon::ipc("status")
                    .await
                    .unwrap_or_else(|e| e.to_string())
            );
        }
    }
    Ok(())
}

async fn uninstall(yes: bool) -> Result<()> {
    use std::{
        io::{self, Write},
        path::{Component, Path, PathBuf},
        process::{Command, Stdio},
    };
    if runtime::in_vice_flatpak() {
        println!(
            "Vice is installed as a Flatpak.\nRun: flatpak uninstall {}",
            runtime::VICE_FLATPAK_ID
        );
        return Ok(());
    }
    let executable = std::env::current_exe()?;
    if executable.starts_with("/nix/store") {
        println!("Vice is managed by Nix. Remove services.vice.enable or the package and rebuild.");
        return Ok(());
    }
    if runtime::which("pacman").is_some()
        && Command::new("pacman")
            .arg("-Qo")
            .arg(&executable)
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout).contains("vice-clipper")
            })
    {
        println!("Vice is managed by pacman.\nRun: sudo pacman -Rns vice-clipper");
        return Ok(());
    }
    let prefix = std::env::var_os("VICE_INSTALL_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime::actual_home_dir().join(".local"));
    if !prefix.is_absolute()
        || prefix == Path::new("/")
        || prefix
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        || prefix.canonicalize().is_ok_and(|p| p == Path::new("/"))
    {
        bail!("installation prefix must be an absolute directory other than /");
    }
    if !yes {
        print!(
            "Remove Vice's binaries, desktop files and user service from {}? [y/N] ",
            prefix.display()
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Uninstall cancelled.");
            return Ok(());
        }
    }
    // Stop only a daemon from this user's runtime. Never elevate or touch recordings.
    let service = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime::actual_home_dir().join(".config"))
        .join("systemd/user/vice.service");
    if runtime::socket_path().exists() {
        let _ = daemon::ipc("stop").await;
    }
    if service.exists() && runtime::which("systemctl").is_some() {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "vice.service"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mut removed = 0;
    for path in [
        prefix.join("bin/vice"),
        prefix.join("bin/vice-app"),
        prefix.join("share/applications/vice.desktop"),
        prefix.join("share/icons/hicolor/scalable/apps/vice.svg"),
        service.clone(),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    for resource in ["ui", "fonts", "data"] {
        let path = prefix.join("share/vice").join(resource);
        if path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            std::fs::remove_file(path)?;
            removed += 1;
        } else {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    if runtime::which("systemctl").is_some() && service.parent().is_some_and(Path::is_dir) {
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    println!(
        "Removed {removed} installation artifacts. Recordings, settings and library metadata are preserved."
    );
    Ok(())
}
