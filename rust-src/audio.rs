//! Nonblocking notification playback and 48 kHz stereo tone synthesis.
use std::{f64::consts::TAU, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;

fn specs(name: &str) -> &'static [(f64, f64)] {
    match name {
        "clip_failed" => &[(440., 0.10), (330., 0.18)],
        "session_start" => &[(523., 0.09), (659., 0.09), (784., 0.13)],
        "session_end" => &[(784., 0.09), (659., 0.09), (523., 0.14)],
        "highlight" => &[(988., 0.06)],
        "screenshot" => &[(1318., 0.04), (1760., 0.05)],
        _ => &[(880., 0.07), (1109., 0.11)],
    }
}

pub fn synthesize(name: &str, volume: f64) -> Vec<u8> {
    const RATE: u32 = 48000;
    let level = if volume.is_finite() {
        volume.clamp(0., 1.)
    } else {
        1.
    };
    let mut data = Vec::new();
    for (index, &(freq, duration)) in specs(name).iter().enumerate() {
        if index > 0 {
            data.resize(data.len() + (RATE as f64 * 0.012) as usize * 4, 0);
        }
        let count = (RATE as f64 * duration) as usize;
        let attack = 480.min(count / 4).max(1);
        let release = 1200.min(count / 3).max(1);
        for i in 0..count {
            let envelope = if i < attack {
                i as f64 / attack as f64
            } else if i >= count - release {
                (count - i) as f64 / release as f64
            } else {
                1.
            };
            let sample =
                (0.30 * level * envelope * (TAU * freq * i as f64 / RATE as f64).sin() * 32767.)
                    as i16;
            data.extend_from_slice(&sample.to_le_bytes());
            data.extend_from_slice(&sample.to_le_bytes());
        }
    }
    let mut wav = Vec::with_capacity(data.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(data.len() as u32 + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

pub fn play(name: &str, volume: f64, custom: Option<&str>) {
    if volume <= 0. {
        return;
    }
    let name = name.to_string();
    let custom = custom.map(str::to_owned);
    tokio::spawn(async move {
        let Some(player) = ["paplay", "aplay", "ffplay"]
            .into_iter()
            .find(|p| crate::recorder::has_program(p))
        else {
            return;
        };
        let custom = custom
            .map(crate::runtime::resolve_path)
            .filter(|p| p.is_file() && p.metadata().is_ok_and(|m| m.len() > 0));
        let path: PathBuf = if let Some(path) = custom {
            path
        } else {
            let directory =
                std::env::temp_dir().join(format!("vice-{}", unsafe { libc::getuid() }));
            if tokio::fs::create_dir_all(&directory).await.is_err() {
                return;
            }
            let path = directory.join(format!(
                "snd_{}_{}.wav",
                name,
                (volume.clamp(0., 1.) * 100.).round() as u32
            ));
            if !path.exists()
                && tokio::fs::write(&path, synthesize(&name, volume))
                    .await
                    .is_err()
            {
                return;
            }
            path
        };
        let mut cmd = Command::new(player);
        if player == "ffplay" {
            cmd.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]);
        }
        cmd.arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(mut child) = cmd.spawn() {
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stereo_wav_header_and_silence() {
        let wav = synthesize("clip", 0.);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 2);
        assert!(wav[44..].iter().all(|b| *b == 0));
        assert!(synthesize("clip", 1.)[44..].iter().any(|b| *b != 0));
    }
}
