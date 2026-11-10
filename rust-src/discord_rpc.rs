//! Discord's local length-prefixed JSON IPC protocol.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};
pub const DEFAULT_CLIENT_ID: &str = "1496646444726354031";
const MAX_FRAME_BYTES: u32 = 64 * 1024;
pub fn socket_paths() -> Vec<PathBuf> {
    let runtime = crate::runtime::runtime_dir();
    let mut bases = vec![runtime.clone()];
    for name in [
        "com.discordapp.Discord",
        "com.discordapp.DiscordCanary",
        "com.discordapp.DiscordPTB",
        "dev.vencord.Vesktop",
    ] {
        bases.push(runtime.join("app").join(name));
    }
    bases.push(runtime.join("snap.discord"));
    if let Ok(entries) = std::fs::read_dir(runtime.join("app")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                bases.push(entry.path());
            }
        }
    }
    bases.push(PathBuf::from("/tmp"));
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for base in bases {
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("discord-ipc-")
                    && seen.insert(entry.path())
                {
                    out.push(entry.path());
                }
            }
        }
        for n in 0..10 {
            let path = base.join(format!("discord-ipc-{n}"));
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }
    out
}
pub struct DiscordRPC {
    pub client_id: String,
    pub timeout: Duration,
    stream: Option<UnixStream>,
}
impl DiscordRPC {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            timeout: Duration::from_secs(2),
            stream: None,
        }
    }
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }
    pub async fn connect(&mut self) -> bool {
        if self.is_connected() {
            return true;
        }
        if self.client_id.is_empty() {
            return false;
        }
        for path in socket_paths() {
            if !path.exists() {
                continue;
            }
            if let Ok(Ok(stream)) = timeout(self.timeout, UnixStream::connect(&path)).await {
                self.stream = Some(stream);
                let result = async {
                    self.send(0, &json!({"v":1,"client_id":self.client_id}))
                        .await?;
                    let (op, payload) = self.recv_response().await?;
                    if op != 1 || payload["evt"] != "READY" {
                        bail!("unexpected Discord handshake");
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if result.is_ok() {
                    tracing::debug!("Discord IPC connected: {}", path.display());
                    return true;
                }
                self.stream = None;
            }
        }
        false
    }
    pub async fn set_activity(&mut self, activity: Option<Value>) -> bool {
        if !self.is_connected() {
            return false;
        }
        let payload = json!({"cmd":"SET_ACTIVITY","args":{"pid":std::process::id(),"activity":activity},"nonce":uuid::Uuid::new_v4().to_string()});
        let result = async {
            self.send(1, &payload).await?;
            let (op, ack) = self.recv_response().await?;
            if op != 1 || ack.get("cmd").is_some_and(|v| v != "SET_ACTIVITY") {
                bail!("unexpected Discord activity acknowledgement");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(%error,"Discord RPC disconnected");
            self.stream = None;
            return false;
        }
        true
    }
    pub async fn close(&mut self) {
        let _ = self.send(2, &json!({})).await;
        self.stream = None;
    }
    async fn send(&mut self, op: u32, payload: &Value) -> Result<()> {
        let stream = self.stream.as_mut().context("Discord is not connected")?;
        let body = serde_json::to_vec(payload)?;
        if body.len() > MAX_FRAME_BYTES as usize {
            bail!("Discord frame too large");
        }
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend(op.to_le_bytes());
        frame.extend((body.len() as u32).to_le_bytes());
        frame.extend(body);
        timeout(self.timeout, stream.write_all(&frame)).await??;
        Ok(())
    }
    async fn recv(&mut self) -> Result<(u32, Value)> {
        let stream = self.stream.as_mut().context("Discord is not connected")?;
        let mut header = [0u8; 8];
        timeout(self.timeout, stream.read_exact(&mut header)).await??;
        let op = u32::from_le_bytes(header[..4].try_into()?);
        let length = u32::from_le_bytes(header[4..].try_into()?);
        if length > MAX_FRAME_BYTES {
            bail!("Discord frame too large: {length}");
        }
        let mut body = vec![0; length as usize];
        timeout(self.timeout, stream.read_exact(&mut body)).await??;
        Ok((
            op,
            serde_json::from_slice(&body).unwrap_or_else(|_| json!({})),
        ))
    }
    async fn recv_response(&mut self) -> Result<(u32, Value)> {
        let mut frame = self.recv().await?;
        while frame.0 == 3 {
            self.send(4, &frame.1).await?;
            frame = self.recv().await?;
        }
        if frame.0 == 2 || frame.1["evt"] == "ERROR" {
            bail!("Discord rejected request: {}", frame.1);
        }
        Ok(frame)
    }
}
/// Keeps Discord presence in sync with game focus and live configuration.
pub async fn presence_loop(state: crate::server::Shared) {
    use crate::{active_window, game_icons};
    let mut shutdown = state.shutdown.subscribe();
    let mut rpc = DiscordRPC::new(DEFAULT_CLIENT_ID);
    let mut last_game: Option<String> = None;
    let mut last_pid = 0;
    let mut started = crate::updates::now() as u64;
    let mut last_activity = Value::Null;
    loop {
        let cfg = state.cfg.read().await.discord.clone();
        let id = cfg
            .client_id_override
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_CLIENT_ID);
        if rpc.client_id != id {
            rpc.close().await;
            rpc = DiscordRPC::new(id);
            last_activity = Value::Null;
        }
        if !cfg.enabled {
            if rpc.is_connected() {
                rpc.set_activity(None).await;
                rpc.close().await;
            }
            last_game = None;
            last_activity = Value::Null;
        } else {
            let focused = active_window::get_active_window().await;
            let mut game = focused
                .as_ref()
                .and_then(|w| active_window::match_game(w, &cfg.custom_games));
            if game.is_none()
                && cfg.persist_while_running
                && last_pid > 0
                && crate::runtime::read_host_proc(last_pid, "comm").is_some()
            {
                game = last_game.clone();
            }
            if game != last_game {
                started = crate::updates::now() as u64;
                if game.is_some() {
                    last_pid = focused.as_ref().map_or(0, |w| w.pid);
                }
            }
            if game.is_some() || rpc.is_connected() {
                let was_connected = rpc.is_connected();
                if rpc.connect().await {
                    let activity = if let Some(game) = &game {
                        let icon = if cfg.game_icons {
                            game_icons::discord_icon_url(game).await
                        } else {
                            None
                        };
                        let assets = if let Some(icon) = icon {
                            json!({"large_image":icon,"large_text":game,"small_image":"vice_logo","small_text":"Vice"})
                        } else {
                            json!({"large_image":"vice_logo","large_text":"Vice, Linux clip recorder"})
                        };
                        json!({"details":format!("Clipping {game} with Vice"),"state":game,"timestamps":{"start":started},"assets":assets})
                    } else {
                        Value::Null
                    };
                    if (!was_connected || activity != last_activity)
                        && rpc
                            .set_activity(if activity.is_null() {
                                None
                            } else {
                                Some(activity.clone())
                            })
                            .await
                    {
                        last_activity = activity;
                    }
                }
            }
            last_game = game;
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(5))=>{},_=shutdown.changed()=>break}
    }
    if rpc.is_connected() {
        rpc.set_activity(None).await;
        rpc.close().await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_frames() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut rpc = DiscordRPC::new("test");
        rpc.stream = Some(a);
        b.write_all(&[1u32.to_le_bytes(), (MAX_FRAME_BYTES + 1).to_le_bytes()].concat())
            .await
            .unwrap();
        assert!(
            rpc.recv()
                .await
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
    }
    #[tokio::test]
    async fn ping_reply_and_ack() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut rpc = DiscordRPC::new("test");
        rpc.stream = Some(a);
        let task = tokio::spawn(async move {
            for (op, payload) in [(3u32, json!({"nonce":"hi"})), (1, json!({"evt":"READY"}))] {
                let body = serde_json::to_vec(&payload).unwrap();
                b.write_all(
                    &[
                        op.to_le_bytes().to_vec(),
                        (body.len() as u32).to_le_bytes().to_vec(),
                        body,
                    ]
                    .concat(),
                )
                .await
                .unwrap();
            }
            let mut header = [0; 8];
            b.read_exact(&mut header).await.unwrap();
            assert_eq!(u32::from_le_bytes(header[..4].try_into().unwrap()), 4);
        });
        assert_eq!(rpc.recv_response().await.unwrap().1["evt"], "READY");
        task.await.unwrap();
    }
}
