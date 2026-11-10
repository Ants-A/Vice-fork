pub mod active_window;
pub mod audio;
pub mod config;
pub mod daemon;
pub mod discord_rpc;
pub mod editor;
pub mod game_icons;
pub mod hotkey;
pub mod keys;
pub mod media;
pub mod platform_info;
pub mod playlist_icons;
pub mod playlists;
pub mod recorder;
pub mod runtime;
pub mod server;
pub mod updates;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod key_overlay;
pub mod tray;

pub mod encoding;

pub mod stream_preview;

pub mod media_stream;

pub mod native_video;

pub mod media_names;
