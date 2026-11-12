//! Timeline validation and deterministic FFmpeg composition graphs.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub const FPS: u32 = 60;
pub const DISCORD_MAX_BYTES: u64 = 20_000_000;
pub const DISCORD_TARGET_BYTES: u64 = 19_600_000;
pub const DISCORD_AUDIO_KBPS: i64 = 128;
pub const DISCORD_MIN_VIDEO_KBPS: i64 = 150;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub path: PathBuf,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub audio_streams: usize,
}
#[derive(Clone, Debug)]
pub struct KeyBurn {
    pub panel: PathBuf,
    pub size: (u32, u32),
    pub keys: HashMap<String, (PathBuf, i32, i32)>,
    pub intervals: HashMap<String, Vec<(f64, f64, String)>>,
}
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn f(v: &Value, k: &str) -> f64 {
    v[k].as_f64()
        .or_else(|| v[k].as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}
fn fallback(v: &Value, k: &str, default: f64) -> f64 {
    if v.get(k).is_none() {
        default
    } else {
        v[k].as_f64()
            .or_else(|| v[k].as_str().and_then(|s| s.parse().ok()))
            .filter(|n| n.is_finite())
            .unwrap_or(default)
    }
}
fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
fn n(x: f64) -> String {
    let st = format!("{x:.3}");
    let st = st.trim_end_matches('0').trim_end_matches('.');
    if st.is_empty() || st == "-0" {
        "0".into()
    } else {
        st.into()
    }
}
fn array<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v[k].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
fn valid_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|c| c.is_ascii_hexdigit())
}
fn valid_fx(s: &str) -> bool {
    [
        "crossfade",
        "fadeblack",
        "fadewhite",
        "blurdis",
        "slide",
        "dipaccent",
    ]
    .contains(&s)
}
fn xfade(s: &str) -> &str {
    match s {
        "crossfade" => "fade",
        "blurdis" => "hblur",
        "slide" => "slideleft",
        _ => s,
    }
}
fn hex(s: &str) -> String {
    format!("0x{}", s.trim_start_matches('#').to_lowercase())
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn validate_project(raw: &Value, sources: &HashMap<String, Source>) -> (Value, Vec<String>) {
    let mut errors = vec![];
    let mut tracks = vec![];
    let mut seen = HashSet::new();
    for t in array(raw, "tracks") {
        if !t.is_object() {
            continue;
        }
        let id = s(t, "id");
        let kind = s(t, "type");
        if !valid_id(id) || !["text", "video", "audio"].contains(&kind) {
            errors.push(format!(
                "track {}: invalid id or type",
                if id.is_empty() { "?" } else { id }
            ));
            continue;
        }
        if !seen.insert(id) {
            errors.push(format!("track {id}: duplicate id"));
            continue;
        }
        let label = t["label"].as_str().unwrap_or(id);
        tracks.push(json!({"id":id,"type":kind,"label":if label.is_empty(){id.to_owned()}else{label.chars().take(8).collect()}}));
    }
    if tracks.iter().filter(|t| t["type"] == "text").count() != 1 {
        errors.push("project needs exactly one text track".into());
    }
    if !tracks.iter().any(|t| t["type"] == "video") {
        errors.push("project needs at least one video track".into());
    }
    let track_types: HashMap<_, _> = tracks.iter().map(|t| (s(t, "id"), s(t, "type"))).collect();
    let mut items = vec![];
    let mut seen = HashSet::new();
    for it in array(raw, "items") {
        if !it.is_object() {
            continue;
        }
        let id = s(it, "id");
        let kind = s(it, "kind");
        let tid = s(it, "trackId");
        if !valid_id(id) || !seen.insert(id) {
            errors.push(format!(
                "item {}: invalid or duplicate id",
                if id.is_empty() { "?" } else { id }
            ));
            continue;
        }
        let expected = match kind {
            "clip" => "video",
            "audio" => "audio",
            "text" => "text",
            _ => "",
        };
        let Some(track) = track_types.get(tid).filter(|_| !expected.is_empty()) else {
            errors.push(format!("item {id}: unknown kind or track"));
            continue;
        };
        if *track != expected {
            errors.push(format!(
                "item {id}: {kind} does not belong on a {track} track"
            ));
            continue;
        }
        if ["start", "dur"].iter().any(|key| {
            it.get(*key).is_some_and(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .filter(|n| n.is_finite())
                    .is_none()
            })
        }) {
            errors.push(format!("item {id}: bad timing values"));
            continue;
        }
        let start = r3(f(it, "start").max(0.0));
        let dur = r3(f(it, "dur"));
        if dur <= 0.0 {
            errors.push(format!("item {id}: duration must be positive"));
            continue;
        }
        let mut out = json!({"id":id,"kind":kind,"trackId":tid,"start":start,"dur":dur});
        if kind == "clip" || kind == "audio" {
            let cid = s(it, "clipId");
            let Some(src) = sources.get(cid) else {
                errors.push(format!(
                    "item {id}: clip {} is missing",
                    if cid.is_empty() { "?" } else { cid }
                ));
                continue;
            };
            let offset = r3(f(it, "offset").max(0.0));
            if src.duration > 0.0 && offset + dur > src.duration + 0.05 {
                errors.push(format!("item {id}: uses more of {cid} than it has"));
                continue;
            }
            out["clipId"] = json!(cid);
            out["offset"] = json!(offset);
            if kind == "audio" {
                let stream = it.get("audioStream").cloned().unwrap_or(json!(0));
                if stream
                    .as_u64()
                    .filter(|v| (*v as usize) < src.audio_streams)
                    .is_none()
                {
                    errors.push(format!("item {id}: audio stream is missing or invalid"));
                    continue;
                }
                let vol = it.get("volume").cloned().unwrap_or(json!(1));
                if vol
                    .as_f64()
                    .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                    .is_none()
                {
                    errors.push(format!("item {id}: volume must be between 0 and 1"));
                    continue;
                }
                if it.get("muted").filter(|v| !v.is_boolean()).is_some() {
                    errors.push(format!("item {id}: muted must be a boolean"));
                    continue;
                }
                for key in ["audioStream", "volume", "muted"] {
                    if let Some(v) = it.get(key) {
                        out[key] = v.clone();
                    }
                }
            } else {
                out["muted"] = json!(it["muted"].as_bool().unwrap_or(false));
                if valid_fx(s(&it["trans"], "fx")) {
                    let cap = 3.0_f64.min(r3(dur * 0.8));
                    out["trans"] = json!({"fx":it["trans"]["fx"],"len":r3(fallback(&it["trans"],"len",0.2).max(0.2).min(cap))});
                }
            }
        } else {
            out["text"] = json!(s(it, "text").chars().take(200).collect::<String>());
            out["font"] = json!(if ["display", "body", "mono"].contains(&s(it, "font")) {
                s(it, "font")
            } else {
                "display"
            });
            out["color"] = json!(if valid_color(s(it, "color")) {
                s(it, "color")
            } else {
                "#f2f5fa"
            });
            out["size"] = json!((fallback(it, "size", 64.0) as i64).clamp(8, 1000));
            out["weight"] = json!((fallback(it, "weight", 600.0) as i64).clamp(100, 800));
            out["x"] = json!(r3(fallback(it, "x", 50.0).clamp(0.0, 100.0)));
            out["y"] = json!(r3(fallback(it, "y", 50.0).clamp(0.0, 100.0)));
        }
        items.push(out);
    }
    let order: HashMap<_, _> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (s(t, "id"), i))
        .collect();
    items.sort_by(|a, b| {
        order
            .get(s(a, "trackId"))
            .cmp(&order.get(s(b, "trackId")))
            .then(f(a, "start").total_cmp(&f(b, "start")))
            .then(s(a, "id").cmp(s(b, "id")))
    });
    for pair in items.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        if a["trackId"] == b["trackId"] && f(b, "start") < f(a, "start") + f(a, "dur") - 1e-6 {
            errors.push(format!(
                "item {}: overlaps {} on {}",
                s(b, "id"),
                s(a, "id"),
                s(a, "trackId")
            ));
        }
    }
    let project = json!({"version":1,"tracks":tracks,"items":items});
    if array(&project, "items").is_empty() {
        errors.push("timeline is empty".into());
    } else if project_extent(&project) > 3600.0 {
        errors.push("timeline is longer than an hour".into());
    }
    (project, errors)
}
pub fn project_extent(p: &Value) -> f64 {
    r3(array(p, "items")
        .iter()
        .map(|i| f(i, "start") + f(i, "dur"))
        .fold(0.0, f64::max))
}
pub fn canvas_for(p: &Value, sources: &HashMap<String, Source>) -> (u32, u32, u32) {
    let main = array(p, "tracks")
        .iter()
        .rfind(|t| t["type"] == "video")
        .map(|t| s(t, "id"))
        .unwrap_or("");
    let (mut w, mut h) = (1920, 1080);
    if let Some(it) = array(p, "items")
        .iter()
        .find(|i| s(i, "trackId") == main && i["kind"] == "clip")
        && let Some(src) = sources.get(s(it, "clipId"))
        && src.width > 0
        && src.height > 0
    {
        w = src.width;
        h = src.height;
    }
    (w - w % 2, h - h % 2, FPS)
}
pub fn font_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("VICE_RESOURCE_DIR")
        .map(PathBuf::from)
        .map(|p| p.join("fonts"))
        .filter(|p| p.is_dir())
    {
        return path;
    }
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("../share/vice/fonts")))
        .unwrap_or_default();
    for path in [
        beside_exe,
        PathBuf::from("/usr/share/vice/fonts"),
        PathBuf::from("/usr/local/share/vice/fonts"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vice/fonts"),
    ] {
        if path.is_dir() {
            return path;
        }
    }
    PathBuf::from("vice/fonts")
}
pub fn font_file(fonts: &Path, font: &str, weight: i64) -> PathBuf {
    let stem = match font {
        "body" => "Inter",
        "mono" => "JetBrainsMono",
        _ => "Geist",
    };
    fonts.join(format!(
        "{stem}-{}.ttf",
        if weight >= 600 { "Bold" } else { "Regular" }
    ))
}
pub fn text_file_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("text_{id}.txt"))
}
pub fn text_file_contents(p: &Value, dir: &Path) -> HashMap<PathBuf, String> {
    array(p, "items")
        .iter()
        .filter(|i| i["kind"] == "text")
        .map(|it| (text_file_path(dir, s(it, "id")), s(it, "text").to_owned()))
        .collect()
}

#[derive(Clone)]
struct Element {
    it: Option<Value>,
    dur: f64,
    ext: f64,
    pre: String,
    suf: String,
}
impl Element {
    fn gap(dur: f64) -> Self {
        Self {
            it: None,
            dur,
            ext: 0.0,
            pre: String::new(),
            suf: String::new(),
        }
    }
    fn clip(it: &Value) -> Self {
        Self {
            it: Some(it.clone()),
            dur: f(it, "dur"),
            ext: 0.0,
            pre: String::new(),
            suf: String::new(),
        }
    }
}
struct Graph<'a> {
    w: u32,
    h: u32,
    accent: &'a str,
    sources: &'a HashMap<String, Source>,
    inputs: HashMap<String, usize>,
    lines: Vec<String>,
    seq: usize,
}
impl Graph<'_> {
    fn label(&mut self, prefix: &str) -> String {
        let s = format!("{prefix}{}", self.seq);
        self.seq += 1;
        s
    }
    fn gap(&self, dur: f64, transparent: bool) -> String {
        format!(
            "color=c={}:s={}x{}:r={FPS}:d={},settb=AVTB,format={}",
            if transparent { "black@0" } else { "black" },
            self.w,
            self.h,
            n(dur),
            if transparent { "yuva420p" } else { "yuv420p" }
        )
    }
}
struct Chain<'a, 'b> {
    ctx: &'a mut Graph<'b>,
    transparent: bool,
    cur: Option<String>,
    pending: Vec<Element>,
    end: f64,
    tail_clip: bool,
}
impl<'a, 'b> Chain<'a, 'b> {
    fn new(ctx: &'a mut Graph<'b>, transparent: bool) -> Self {
        Self {
            ctx,
            transparent,
            cur: None,
            pending: vec![],
            end: 0.0,
            tail_clip: false,
        }
    }
    fn emit(&mut self, el: &Element) -> String {
        let lbl = self.ctx.label("s");
        let expression = if let Some(it) = &el.it {
            let src = &self.ctx.sources[s(it, "clipId")];
            let media_ext = el
                .ext
                .min(r3(src.duration - f(it, "offset") - el.dur).max(0.0));
            let mut extra = format!("{}{}", el.pre, el.suf);
            if el.ext - media_ext > 1e-3 {
                extra.push_str(&format!(
                    ",tpad=stop_mode=clone:stop_duration={}",
                    n(el.ext - media_ext)
                ));
            }
            format!(
                "[{}:v]trim=start={}:end={},setpts=PTS-STARTPTS,fps={FPS},scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2,setsar=1,settb=AVTB,format={}{}",
                self.ctx.inputs[s(it, "clipId")],
                n(f(it, "offset")),
                n(f(it, "offset") + el.dur + media_ext),
                self.ctx.w,
                self.ctx.h,
                self.ctx.w,
                self.ctx.h,
                if self.transparent {
                    "yuva420p"
                } else {
                    "yuv420p"
                },
                extra
            )
        } else {
            format!(
                "{}{}",
                self.ctx.gap(el.dur + el.ext, self.transparent),
                el.suf
            )
        };
        self.ctx.lines.push(format!("{expression}[{lbl}]"));
        lbl
    }
    fn pipe(&mut self, expr: &str) {
        let lbl = self.ctx.label("s");
        self.ctx
            .lines
            .push(format!("[{}]{expr}[{lbl}]", self.cur.as_deref().unwrap()));
        self.cur = Some(lbl);
    }
    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let mut parts = vec![];
        if let Some(cur) = &self.cur {
            parts.push(cur.clone());
        }
        for el in pending {
            parts.push(self.emit(&el));
        }
        if parts.is_empty() {
            return;
        }
        if parts.len() == 1 {
            self.cur = parts.pop();
            return;
        }
        let lbl = self.ctx.label("c");
        self.ctx.lines.push(format!(
            "{}concat=n={}:v=1:a=0[{lbl}]",
            parts.iter().map(|p| format!("[{p}]")).collect::<String>(),
            parts.len()
        ));
        self.cur = Some(lbl);
    }
    fn xfade(&mut self, left: &str, right: &str, fx: &str, len: f64, offset: f64) {
        let mut expr = format!(
            "[{left}][{right}]xfade=transition={}:duration={}:offset={}",
            xfade(fx),
            n(len),
            n(offset)
        );
        if self.transparent && ["fadeblack", "fadewhite"].contains(&fx) && !self.tail_clip {
            expr.push_str(&format!(",fade=t=in:st={}:d={}:alpha=1", n(offset), n(len)));
        }
        let lbl = self.ctx.label("x");
        self.ctx.lines.push(format!("{expr}[{lbl}]"));
        self.cur = Some(lbl);
    }
    fn junction(&mut self, mut el: Element, trans: &Value) {
        let fx = s(trans, "fx");
        let length = f(trans, "len");
        let start = f(el.it.as_ref().unwrap(), "start");
        if fx == "dipaccent" {
            let half = r3(length / 2.0);
            let color = hex(self.ctx.accent);
            if let Some(left) = self.pending.last_mut() {
                left.suf.push_str(&format!(
                    ",fade=t=out:st={}:d={}:color={color}",
                    n(left.dur - half),
                    n(half)
                ));
            } else {
                self.pipe(&format!(
                    "fade=t=out:st={}:d={}:color={color}",
                    n(start - half),
                    n(half)
                ));
            }
            el.pre = format!(",fade=t=in:st=0:d={}:color={color}", n(half));
            self.pending.push(el);
            return;
        }
        if let Some(left) = self.pending.last_mut() {
            left.ext = length;
        } else {
            self.pipe(&format!("tpad=stop_mode=clone:stop_duration={}", n(length)));
        }
        let tail = self.tail_clip;
        self.flush();
        let right = self.emit(&el);
        self.tail_clip = tail;
        let left = self.cur.clone().unwrap();
        self.xfade(&left, &right, fx, length, start);
    }
    fn lead(&mut self, mut el: Element, trans: &Value) {
        let fx = s(trans, "fx");
        let len = f(trans, "len");
        if fx == "dipaccent" {
            el.pre = format!(
                ",fade=t=in:st=0:d={}:color={}",
                n(len),
                hex(self.ctx.accent)
            );
            self.pending.push(el);
            return;
        }
        let lead = self.ctx.label("s");
        self.ctx
            .lines
            .push(format!("{}[{lead}]", self.ctx.gap(len, self.transparent)));
        let right = self.emit(&el);
        self.tail_clip = false;
        self.xfade(&lead, &right, fx, len, 0.0);
    }
    fn build(mut self, items: &[&Value], extent: f64, pad_tail: bool) -> Option<String> {
        let mut els = vec![];
        let mut cursor = 0.0;
        for it in items {
            if f(it, "start") - cursor > 0.05 {
                els.push(Element::gap(r3(f(it, "start") - cursor)));
            }
            els.push(Element::clip(it));
            cursor = f(it, "start") + f(it, "dur");
        }
        if els.is_empty() {
            return None;
        }
        for el in els {
            let end = el
                .it
                .as_ref()
                .map(|it| f(it, "start") + f(it, "dur"))
                .unwrap_or(self.end + el.dur);
            let tail_clip = el.it.is_some();
            let trans = el.it.as_ref().and_then(|it| it.get("trans")).cloned();
            if let Some(trans) = trans {
                if !self.pending.is_empty() || self.cur.is_some() {
                    self.junction(el, &trans);
                } else {
                    self.lead(el, &trans);
                }
            } else {
                self.pending.push(el);
            }
            self.end = end;
            self.tail_clip = tail_clip;
        }
        self.flush();
        if pad_tail && extent - self.end > 1e-3 {
            self.pipe(&format!(
                "tpad=stop_mode=add:stop_duration={}",
                n(extent - self.end)
            ));
        }
        self.cur
    }
}
/// Keep the encoder's burst allowance within the file-size headroom. Larger
/// VBV windows can overshoot an otherwise correct average bitrate on VAAPI.
pub fn discord_buffer_kbps(video_kbps: i64) -> i64 {
    (video_kbps / 4).clamp(1, 1600)
}

pub fn discord_bitrates(duration: f64, audio_streams: usize) -> Result<(i64, i64)> {
    let audio = DISCORD_AUDIO_KBPS * audio_streams.max(1) as i64;
    let video = (DISCORD_TARGET_BYTES as f64 * 8.0 / 1000.0 / duration.max(1.0) - audio as f64)
        .floor() as i64;
    if video < DISCORD_MIN_VIDEO_KBPS {
        bail!(
            "{duration:.0}s with {} audio track(s) cannot fit 20 MB. Trim it shorter first.",
            audio_streams.max(1)
        );
    }
    Ok((video, DISCORD_AUDIO_KBPS))
}
pub fn build_export_cmd(
    project: &Value,
    sources: &HashMap<String, Source>,
    out: &Path,
    accent: &str,
    fonts: &Path,
    text_dir: &Path,
    discord: bool,
) -> Result<Vec<String>> {
    build_export_cmd_with_keys(
        project, sources, out, accent, fonts, text_dir, discord, None,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn build_export_cmd_with_keys(
    project: &Value,
    sources: &HashMap<String, Source>,
    out: &Path,
    accent: &str,
    fonts: &Path,
    text_dir: &Path,
    discord: bool,
    key_burn: Option<&KeyBurn>,
) -> Result<Vec<String>> {
    let (w, h, _) = canvas_for(project, sources);
    let extent = project_extent(project);
    if extent <= 0.0 {
        bail!("timeline is empty");
    }
    let video: Vec<_> = array(project, "tracks")
        .iter()
        .filter(|t| t["type"] == "video")
        .map(|t| s(t, "id"))
        .collect();
    let Some(main) = video.last() else {
        bail!("project needs at least one video track");
    };
    let lane = |tid: &str| {
        array(project, "items")
            .iter()
            .filter(|i| s(i, "trackId") == tid)
            .collect::<Vec<_>>()
    };
    let mut order: Vec<String> = vec![];
    for it in array(project, "items") {
        let cid = s(it, "clipId");
        if !cid.is_empty() && !order.iter().any(|s| s == cid) {
            if !sources.contains_key(cid) {
                bail!("missing clip {cid}");
            }
            order.push(cid.into());
        }
    }
    let mut ctx = Graph {
        w,
        h,
        accent: if valid_color(accent) {
            accent
        } else {
            "#0099ff"
        },
        sources,
        inputs: order
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i))
            .collect(),
        lines: vec![],
        seq: 0,
    };
    let mut cur = Chain::new(&mut ctx, false)
        .build(&lane(main), extent, true)
        .unwrap_or_else(|| {
            let lbl = ctx.label("s");
            ctx.lines.push(format!("{}[{lbl}]", ctx.gap(extent, false)));
            lbl
        });
    for tid in video[..video.len() - 1].iter().rev() {
        if let Some(upper) = Chain::new(&mut ctx, true).build(&lane(tid), extent, false) {
            let lbl = ctx.label("o");
            ctx.lines
                .push(format!("[{cur}][{upper}]overlay=eof_action=pass[{lbl}]"));
            cur = lbl;
        }
    }
    for it in array(project, "items")
        .iter()
        .filter(|i| i["kind"] == "text")
    {
        let font = font_file(fonts, s(it, "font"), f(it, "weight") as i64);
        let size = (f(it, "size") * h as f64 / 1080.0).round().max(8.0) as u32;
        let lbl = ctx.label("t");
        ctx.lines.push(format!("[{cur}]drawtext=expansion=none:fontfile={}:textfile={}:fontsize={size}:fontcolor={}:shadowcolor=black@0.5:shadowx=2:shadowy=2:x=(w*{}/100)-(text_w/2):y=(h*{}/100)-(text_h/2):enable='between(t,{},{})'[{lbl}]",quote(&font.to_string_lossy()),quote(&text_file_path(text_dir,s(it,"id")).to_string_lossy()),hex(s(it,"color")),n(f(it,"x")),n(f(it,"y")),n(f(it,"start")),n(f(it,"start")+f(it,"dur"))));
        cur = lbl;
    }
    let mut images = vec![];
    if let Some(burn) = key_burn {
        let mut panels = vec![];
        let mut held: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
        for it in lane(main) {
            if let Some(intervals) = burn.intervals.get(s(it, "clipId")) {
                let start = f(it, "start");
                let end = start + f(it, "dur");
                panels.push((start, end));
                for (a, b, key) in intervals {
                    let ta = start.max(start + a - f(it, "offset"));
                    let tb = end.min(start + b - f(it, "offset"));
                    if tb > ta && burn.keys.contains_key(key) {
                        held.entry(key.clone()).or_default().push((r3(ta), r3(tb)));
                    }
                }
            }
        }
        if !panels.is_empty() {
            let px = (32.0 * h as f64 / 1080.0).round() as i64;
            let py = h as i64 - burn.size.1 as i64 - px;
            images.push(burn.panel.clone());
            let lbl = ctx.label("k");
            ctx.lines.push(format!(
                "[{cur}][{}:v]overlay=x={px}:y={py}:enable='{}'[{lbl}]",
                order.len(),
                windows(&panels)
            ));
            cur = lbl;
            let mut keys: Vec<_> = held.keys().collect();
            keys.sort();
            for key in keys {
                let (path, x, y) = &burn.keys[key];
                images.push(path.clone());
                let lbl = ctx.label("k");
                ctx.lines.push(format!(
                    "[{cur}][{}:v]overlay=x={}:y={}:enable='{}'[{lbl}]",
                    order.len() + images.len() - 1,
                    px + *x as i64,
                    py + *y as i64,
                    windows(&held[key])
                ));
                cur = lbl;
            }
        }
    }
    let mut audio: Vec<_> = array(project, "items")
        .iter()
        .filter(|it| {
            !s(it, "clipId").is_empty()
                && sources
                    .get(s(it, "clipId"))
                    .map(|src| src.has_audio)
                    .unwrap_or(false)
                && ["audio", "clip"].contains(&s(it, "kind"))
                && it["muted"] != true
        })
        .collect();
    audio.sort_by(|a, b| {
        f(a, "start")
            .total_cmp(&f(b, "start"))
            .then(s(a, "id").cmp(s(b, "id")))
    });
    ctx.lines.push(format!(
        "anullsrc=r=48000:cl=stereo,atrim=0:{}[ab]",
        n(extent)
    ));
    let mut labels = vec!["ab".to_owned()];
    for (i, it) in audio.iter().enumerate() {
        let mut expr = format!(
            "[{}:a:{}]",
            ctx.inputs[s(it, "clipId")],
            it["audioStream"].as_u64().unwrap_or(0)
        );
        if it["kind"] == "audio" {
            expr.push_str("aresample=48000:async=1:first_pts=0,");
        }
        expr.push_str(&format!("atrim=start={}:end={},asetpts=PTS-STARTPTS,aformat=sample_rates=48000:channel_layouts=stereo,",n(f(it,"offset")),n(f(it,"offset")+f(it,"dur"))));
        let volume = fallback(it, "volume", 1.0);
        if volume != 1.0 {
            expr.push_str(&format!("volume={},", n(volume)));
        }
        expr.push_str(&format!(
            "adelay={}:all=1[a{i}]",
            (f(it, "start") * 1000.0).round() as i64
        ));
        ctx.lines.push(expr);
        labels.push(format!("a{i}"));
    }
    if labels.len() == 1 {
        ctx.lines.push("[ab]anull[aout]".into());
    } else {
        ctx.lines.push(format!(
            "{}amix=inputs={}:duration=longest:normalize=0,atrim=0:{},asetpts=PTS-STARTPTS[aout]",
            labels.iter().map(|a| format!("[{a}]")).collect::<String>(),
            labels.len(),
            n(extent)
        ));
    }
    let mut cmd: Vec<String> = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
    ]
    .map(str::to_owned)
    .to_vec();
    for cid in &order {
        cmd.extend([
            "-i".into(),
            sources[cid].path.to_string_lossy().into_owned(),
        ]);
    }
    for image in images {
        cmd.extend([
            "-loop".into(),
            "1".into(),
            "-t".into(),
            n(extent),
            "-i".into(),
            image.to_string_lossy().into_owned(),
        ]);
    }
    cmd.extend([
        "-filter_complex".into(),
        ctx.lines.join(";"),
        "-map".into(),
        format!("[{cur}]"),
        "-map".into(),
        "[aout]".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
    ]);
    let ab;
    if discord {
        let (vb, a) = discord_bitrates(extent, 1)?;
        ab = format!("{a}k");
        cmd.extend([
            "medium".into(),
            "-b:v".into(),
            format!("{vb}k"),
            "-maxrate".into(),
            format!("{vb}k"),
            "-bufsize".into(),
            format!("{}k", discord_buffer_kbps(vb)),
        ]);
    } else {
        ab = "192k".into();
        cmd.extend(["veryfast".into(), "-crf".into(), "20".into()]);
    }
    cmd.extend([
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        ab,
        "-movflags".into(),
        "+faststart".into(),
        "-t".into(),
        n(extent),
        "-f".into(),
        "mp4".into(),
        "-y".into(),
        out.to_string_lossy().into_owned(),
    ]);
    Ok(cmd)
}
fn windows(spans: &[(f64, f64)]) -> String {
    spans
        .iter()
        .map(|(a, b)| format!("between(t,{},{})", n(*a), n(*b)))
        .collect::<Vec<_>>()
        .join("+")
}
pub fn sanitize_export_name(name: &str) -> Option<String> {
    crate::media_names::valid_filename(&format!("{name}.mp4")).then(|| format!("{name}.mp4"))
}

pub fn default_export_name(dir: &Path) -> String {
    let mut i = 1;
    loop {
        let stem = format!("Vice_Edit_{i}");
        if !dir.join(format!("{stem}.mp4")).exists() && !dir.join(format!("{stem}.mkv")).exists() {
            return format!("{stem}.mp4");
        }
        i += 1;
    }
}
#[derive(Clone, Debug)]
pub struct EditorProjectStore {
    pub path: PathBuf,
}
impl EditorProjectStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn load(&self) -> Option<Value> {
        let v: Value = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        if v.is_object() { Some(v) } else { None }
    }
    pub fn save(&self, project: &Value) -> Result<()> {
        crate::playlists::atomic_json(&self.path, project)
    }
    pub fn on_clip_renamed(&self, old: &str, new: &str) -> Result<bool> {
        let Some(mut p) = self.load() else {
            return Ok(false);
        };
        let Some(items) = p["items"].as_array_mut() else {
            return Ok(false);
        };
        let mut changed = false;
        for i in items {
            if i["clipId"] == old {
                i["clipId"] = json!(new);
                changed = true;
            }
        }
        if changed {
            self.save(&p)?;
        }
        Ok(changed)
    }
    pub fn on_clip_deleted(&self, slug: &str) -> Result<bool> {
        let Some(mut p) = self.load() else {
            return Ok(false);
        };
        let Some(items) = p["items"].as_array_mut() else {
            return Ok(false);
        };
        let len = items.len();
        items.retain(|i| i["clipId"] != slug);
        let changed = len != items.len();
        if changed {
            self.save(&p)?;
        }
        Ok(changed)
    }
}
pub fn parse_progress(line: &str, total: f64) -> Option<f64> {
    if line == "progress=end" {
        return Some(1.0);
    }
    if total <= 0.0 {
        return None;
    }
    let micros = line.strip_prefix("out_time_us=")?.parse::<u64>().ok()?;
    Some((micros as f64 / 1_000_000.0 / total).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sources() -> HashMap<String, Source> {
        [
            (
                "A".into(),
                Source {
                    path: PathBuf::from("/a.mp4"),
                    duration: 30.0,
                    width: 1920,
                    height: 1080,
                    has_audio: true,
                    audio_streams: 2,
                },
            ),
            (
                "B".into(),
                Source {
                    path: PathBuf::from("/b.mp4"),
                    duration: 10.0,
                    width: 1280,
                    height: 720,
                    has_audio: false,
                    audio_streams: 0,
                },
            ),
        ]
        .into_iter()
        .collect()
    }
    fn project(items: Value) -> Value {
        json!({"version":1,"tracks":[{"id":"T1","type":"text"},{"id":"V2","type":"video"},{"id":"V1","type":"video"},{"id":"A1","type":"audio"}],"items":items})
    }
    fn clip(id: &str, track: &str, cid: &str, start: f64, dur: f64) -> Value {
        json!({"id":id,"kind":"clip","trackId":track,"clipId":cid,"start":start,"dur":dur,"offset":0})
    }
    fn graph(p: &Value) -> String {
        let (p, err) = validate_project(p, &sources());
        assert!(err.is_empty(), "{err:?}");
        let cmd = build_export_cmd(
            &p,
            &sources(),
            Path::new("/out.mp4"),
            "#33adff",
            Path::new("/fonts"),
            Path::new("/texts"),
            false,
        )
        .unwrap();
        cmd[cmd.iter().position(|s| s == "-filter_complex").unwrap() + 1].clone()
    }
    #[test]
    fn rejects_overlap_missing_source_and_track_mismatch() {
        let p = project(json!([
            clip("i1", "V1", "A", 0., 10.),
            clip("i2", "V1", "A", 5., 10.),
            clip("i3", "A1", "A", 0., 10.),
            clip("i4", "V2", "Missing", 0., 10.)
        ]));
        let (_, err) = validate_project(&p, &sources());
        assert!(err.iter().any(|e| e.contains("overlaps")));
        assert!(err.iter().any(|e| e.contains("missing")));
        assert!(err.iter().any(|e| e.contains("does not belong")));
    }
    #[test]
    fn audio_volume_and_stream_validate() {
        let (_, err) = validate_project(
            &project(
                json!([{"id":"a","kind":"audio","trackId":"A1","clipId":"A","dur":4,"volume":1.5,"audioStream":0}]),
            ),
            &sources(),
        );
        assert!(err.iter().any(|e| e.contains("volume")));
    }
    #[test]
    fn simple_graph_golden() {
        let g = graph(&project(json!([clip("i1", "V1", "A", 0., 10.)])));
        assert_eq!(
            g,
            "[0:v]trim=start=0:end=10,setpts=PTS-STARTPTS,fps=60,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,setsar=1,settb=AVTB,format=yuv420p[s0];anullsrc=r=48000:cl=stereo,atrim=0:10[ab];[0:a:0]atrim=start=0:end=10,asetpts=PTS-STARTPTS,aformat=sample_rates=48000:channel_layouts=stereo,adelay=0:all=1[a0];[ab][a0]amix=inputs=2:duration=longest:normalize=0,atrim=0:10,asetpts=PTS-STARTPTS[aout]"
        );
    }
    #[test]
    fn transitions_extend_source_and_freeze_tail() {
        let mut right = clip("i2", "V1", "A", 10., 5.);
        right["trans"] = json!({"fx":"slide","len":1});
        let g = graph(&project(json!([clip("i1", "V1", "B", 0., 10.), right])));
        assert!(g.contains("tpad=stop_mode=clone:stop_duration=1[s0]"));
        assert!(g.contains("xfade=transition=slideleft:duration=1:offset=10"));
    }
    #[test]
    fn upper_tracks_transparent_main_black() {
        let g = graph(&project(json!([
            clip("i1", "V1", "A", 2., 18.),
            clip("i2", "V2", "B", 8., 6.)
        ])));
        assert!(g.contains("color=c=black:s=1920x1080:r=60:d=2"));
        assert!(g.contains("color=c=black@0:s=1920x1080:r=60:d=8"));
        assert!(g.contains("overlay=eof_action=pass"));
    }
    #[test]
    fn lead_dip_repairs_alpha() {
        let mut upper = clip("i2", "V2", "B", 0., 6.);
        upper["trans"] = json!({"fx":"fadeblack","len":0.8});
        let g = graph(&project(json!([clip("i1", "V1", "A", 0., 20.), upper])));
        assert!(g.contains(
            "xfade=transition=fadeblack:duration=0.8:offset=0,fade=t=in:st=0:d=0.8:alpha=1"
        ));
    }
    #[test]
    fn discord_budget_refuses_impossible_length() {
        assert!(discord_bitrates(1800.0, 1).is_err());
        assert!(discord_bitrates(450.0, 1).is_ok());
        assert!(discord_bitrates(450.0, 2).is_err());
        let (a, _) = discord_bitrates(10.0, 1).unwrap();
        let (b, _) = discord_bitrates(10.0, 2).unwrap();
        assert_eq!(a - b, 128);
    }
    #[test]
    fn discord_budget_uses_nearly_twenty_decimal_megabytes_in_one_pass() {
        for duration in [10.0, 60.0, 120.0] {
            for tracks in [1, 2] {
                let (video, audio) = discord_bitrates(duration, tracks).unwrap();
                let estimated = (video + audio * tracks as i64) as f64 * 1000.0 / 8.0 * duration;
                assert!(estimated <= DISCORD_TARGET_BYTES as f64);
                assert!(estimated >= DISCORD_TARGET_BYTES as f64 - 15_000.0);
                assert!(estimated < DISCORD_MAX_BYTES as f64);
            }
        }
    }
    #[test]
    fn names_and_progress() {
        assert_eq!(sanitize_export_name("../../evil.MP4"), None);
        assert_eq!(sanitize_export_name("..."), Some("....mp4".into()));
        assert_eq!(
            sanitize_export_name(" My export! #1.MP4 "),
            Some(" My export! #1.MP4 .mp4".into())
        );
        assert_eq!(parse_progress("out_time_us=5000000", 10.0), Some(0.5));
        assert_eq!(parse_progress("progress=end", 10.0), Some(1.0));
        assert_eq!(parse_progress("out_time_us=nan", 10.0), None);
    }
}
