//! Native keyboard overlay and daemon-side subprocess transport.
use crate::config::Config;
use anyhow::{Context, Result};
use serde_json::{Value, json};
#[cfg(feature = "desktop")]
use std::path::PathBuf;
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
};
pub fn panel_size(scale: f64) -> (i32, i32) {
    ((290. * scale).ceil() as i32, (218. * scale).ceil() as i32)
}
pub fn key_rects(scale: f64) -> Vec<(&'static str, &'static str, f64, f64, f64, f64)> {
    let rows: Vec<Vec<(&str, &str, f64)>> = vec![
        vec![
            ("", "", 0.5),
            ("1", "1", 1.),
            ("2", "2", 1.),
            ("3", "3", 1.),
            ("4", "4", 1.),
            ("5", "5", 1.),
        ],
        vec![
            ("TAB", "Tab", 1.5),
            ("Q", "Q", 1.),
            ("W", "W", 1.),
            ("E", "E", 1.),
            ("R", "R", 1.),
        ],
        vec![
            ("SHIFT", "Shift", 1.5),
            ("A", "A", 1.),
            ("S", "S", 1.),
            ("D", "D", 1.),
            ("F", "F", 1.),
        ],
        vec![("CTRL", "Ctrl", 1.5), ("SPACE", "", 4.)],
    ];
    let mut rects = vec![];
    let mut y = 12. * scale;
    for row in rows {
        let mut x = 12. * scale;
        for (key, label, span) in row {
            let w = if span >= 1. {
                span * 44. * scale + (span - 1.) * 6. * scale
            } else {
                span * 44. * scale
            };
            if !key.is_empty() {
                rects.push((key, label, x, y, w, 44. * scale));
            }
            x += w + 6. * scale;
        }
        y += 50. * scale;
    }
    rects
}
pub struct OverlayHost {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<tokio::task::JoinHandle<()>>,
    state: std::sync::Arc<std::sync::Mutex<Value>>,
    events: Option<tokio::sync::broadcast::Sender<Value>>,
}
impl Default for OverlayHost {
    fn default() -> Self {
        Self {
            child: None,
            stdin: None,
            reader: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                json!({"running":false,"moving":false,"error":null}),
            )),
            events: None,
        }
    }
}
impl OverlayHost {
    pub fn set_events(&mut self, events: tokio::sync::broadcast::Sender<Value>) {
        self.events = Some(events);
    }
    pub fn status(&self) -> Value {
        self.state
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|_| json!({"running":false,"moving":false}))
    }
    fn emit_state(&self, value: Value) {
        if let Ok(mut state) = self.state.lock() {
            *state = value.clone();
        }
        if let Some(events) = &self.events {
            let mut value = value;
            value["type"] = json!("key_overlay");
            let _ = events.send(value);
        }
    }
    pub async fn configure(&mut self, cfg: &Config) -> Result<()> {
        if !cfg.overlay.keys {
            self.stop().await;
            return Ok(());
        }
        if self
            .child
            .as_mut()
            .is_none_or(|c| c.try_wait().ok().flatten().is_some())
        {
            self.stop().await;
            let exe = std::env::current_exe()?;
            let path = if exe.file_name().is_some_and(|s| s == "vice-app") {
                exe
            } else {
                exe.parent().context("binary directory")?.join("vice-app")
            };
            let mut child = Command::new(path)
                .arg("--key-overlay")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .context("start key overlay")?;
            self.stdin = child.stdin.take();
            let output = child.stdout.take().context("overlay stdout")?;
            let state = self.state.clone();
            let events = self.events.clone();
            self.reader = Some(tokio::spawn(async move {
                let mut lines = BufReader::new(output).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(message) = serde_json::from_str::<Value>(&line) {
                        match message["t"].as_str() {
                            Some("ready") => {
                                let value = json!({"running":true,"moving":false,"error":null});
                                if let Ok(mut state) = state.lock() {
                                    *state = value.clone();
                                }
                                if let Some(events) = &events {
                                    let mut value = value;
                                    value["type"] = json!("key_overlay");
                                    let _ = events.send(value);
                                }
                            }
                            Some("moved") => {
                                if let Some(events) = &events {
                                    let _=events.send(json!({"type":"key_overlay_moved","x":message["x"],"y":message["y"]}));
                                }
                            }
                            Some("error") => {
                                let value = json!({"running":false,"moving":false,"error":message["reason"]});
                                if let Ok(mut state) = state.lock() {
                                    *state = value.clone();
                                }
                                if let Some(events) = &events {
                                    let mut value = value;
                                    value["type"] = json!("key_overlay");
                                    let _ = events.send(value);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if let Ok(mut state) = state.lock() {
                    state["running"] = json!(false);
                    state["moving"] = json!(false);
                }
            }));
            self.child = Some(child);
        }
        self.send(json!({"t":"style","accent":cfg.sharing.embed_color}))
            .await?;
        self.send(json!({"t":"place","position":cfg.overlay.keys_position,"x":cfg.overlay.keys_x,"y":cfg.overlay.keys_y,"scale":cfg.overlay.keys_scale})).await
    }
    pub async fn key(&mut self, name: &str, down: bool) {
        if let Some(key) = crate::keys::layout_key(name) {
            let _ = self.send(json!({"t":"key","k":key,"d":down})).await;
        }
    }
    pub async fn moving(&mut self, on: bool) -> Result<()> {
        if self.stdin.is_none() {
            anyhow::bail!("Enable the keyboard overlay first");
        }
        self.send(json!({"t":"move","on":on})).await?;
        let mut value = self.status();
        value["moving"] = json!(on);
        self.emit_state(value);
        Ok(())
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        if let Some(stdin) = &mut self.stdin {
            stdin.write_all(format!("{value}\n").as_bytes()).await?;
        }
        Ok(())
    }
    pub async fn stop(&mut self) {
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if let Some(task) = self.reader.take() {
            task.abort();
        }
        self.emit_state(json!({"running":false,"moving":false,"error":null}));
    }
}
#[cfg(feature = "desktop")]
mod native {
    use super::*;
    use gtk::{
        cairo, gdk,
        glib::{self, translate::ToGlibPtr},
        prelude::*,
    };
    use std::{
        cell::RefCell,
        collections::BTreeSet,
        ffi::{c_char, c_void},
        io::{BufRead, Write},
        rc::Rc,
        time::Duration,
    };
    #[derive(Clone, Copy)]
    struct Layer {
        _library: *mut c_void,
        anchor: unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32, i32),
        margin: unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32, i32),
    }
    impl Layer {
        unsafe fn setup(window: &gtk::Window) -> Option<Self> {
            unsafe {
                let library = libc::dlopen(
                    c"libgtk-layer-shell.so.0".as_ptr(),
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                );
                if library.is_null() {
                    return None;
                }
                macro_rules! sym {
                    ($name:literal,$ty:ty) => {{
                        let ptr = libc::dlsym(library, $name.as_ptr());
                        if ptr.is_null() {
                            return None;
                        }
                        std::mem::transmute::<*mut c_void, $ty>(ptr)
                    }};
                }
                let supported = sym!(c"gtk_layer_is_supported", unsafe extern "C" fn() -> i32);
                if supported() == 0 {
                    return None;
                }
                let init = sym!(
                    c"gtk_layer_init_for_window",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow)
                );
                let set_layer = sym!(
                    c"gtk_layer_set_layer",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32)
                );
                let keyboard = sym!(
                    c"gtk_layer_set_keyboard_mode",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32)
                );
                let zone = sym!(
                    c"gtk_layer_set_exclusive_zone",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32)
                );
                let namespace = sym!(
                    c"gtk_layer_set_namespace",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, *const c_char)
                );
                let anchor = sym!(
                    c"gtk_layer_set_anchor",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32, i32)
                );
                let margin = sym!(
                    c"gtk_layer_set_margin",
                    unsafe extern "C" fn(*mut gtk::ffi::GtkWindow, i32, i32)
                );
                let ptr = window.to_glib_none().0;
                init(ptr);
                set_layer(ptr, 3);
                keyboard(ptr, 0);
                zone(ptr, -1);
                namespace(ptr, c"vice-keys".as_ptr());
                Some(Self {
                    _library: library,
                    anchor,
                    margin,
                })
            }
        }
        fn place(&self, window: &gtk::Window, position: &str, x: i32, y: i32) {
            unsafe {
                let ptr = window.to_glib_none().0;
                for edge in 0..4 {
                    (self.anchor)(ptr, edge, 0);
                    (self.margin)(ptr, edge, 0);
                }
                let left = position.contains("left") || position == "custom";
                let right = position.contains("right");
                let top = position.contains("top") || position == "custom";
                (self.anchor)(ptr, if top { 2 } else { 3 }, 1);
                (self.margin)(
                    ptr,
                    if top { 2 } else { 3 },
                    if position == "custom" { y } else { 32 },
                );
                if left {
                    (self.anchor)(ptr, 0, 1);
                    (self.margin)(ptr, 0, if position == "custom" { x } else { 32 });
                }
                if right {
                    (self.anchor)(ptr, 1, 1);
                    (self.margin)(ptr, 1, 32);
                }
            }
        }
    }
    #[derive(Default)]
    struct State {
        down: BTreeSet<String>,
        accent: String,
        scale: f64,
        position: String,
        x: i32,
        y: i32,
        moving: bool,
        drag: Option<(f64, f64, i32, i32)>,
    }
    fn color(hex: &str) -> (f64, f64, f64) {
        let h = hex.trim_start_matches('#');
        if h.len() != 6 {
            return (0.54, 0.72, 0.95);
        }
        let n = u32::from_str_radix(h, 16).unwrap_or(0x8ab9f3);
        (
            ((n >> 16) & 255) as f64 / 255.,
            ((n >> 8) & 255) as f64 / 255.,
            (n & 255) as f64 / 255.,
        )
    }
    fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
        let pi = std::f64::consts::PI;
        cr.new_sub_path();
        cr.arc(x + w - r, y + r, r, -pi / 2., 0.);
        cr.arc(x + w - r, y + h - r, r, 0., pi / 2.);
        cr.arc(x + r, y + h - r, r, pi / 2., pi);
        cr.arc(x + r, y + r, r, pi, 3. * pi / 2.);
        cr.close_path();
    }
    pub fn draw(
        cr: &cairo::Context,
        accent: &str,
        scale: f64,
        down: &BTreeSet<String>,
        moving: bool,
    ) {
        let (w, h) = panel_size(scale);
        cr.set_operator(cairo::Operator::Source);
        cr.set_source_rgba(0., 0., 0., 0.);
        let _ = cr.paint();
        cr.set_operator(cairo::Operator::Over);
        rounded(cr, 0., 0., w as f64, h as f64, 28. * scale);
        cr.set_source_rgba(0.098, 0.106, 0.137, 0.9);
        let _ = cr.fill_preserve();
        let (r, g, b) = color(accent);
        if moving {
            cr.set_source_rgb(r, g, b);
            cr.set_line_width(2. * scale);
            let _ = cr.stroke();
        } else {
            cr.new_path();
        }
        for (key, label, x, y, w, h) in key_rects(scale) {
            let held = down.contains(key);
            rounded(cr, x, y, w, h, if held { h * 0.16 } else { h * 0.32 });
            if held {
                cr.set_source_rgb(r, g, b);
            } else {
                cr.set_source_rgb(r * 0.24 + 0.08, g * 0.24 + 0.08, b * 0.24 + 0.08);
            }
            let _ = cr.fill();
            if !label.is_empty() {
                cr.select_font_face(
                    "sans-serif",
                    cairo::FontSlant::Normal,
                    cairo::FontWeight::Bold,
                );
                cr.set_font_size(if label.len() > 1 {
                    13. * scale
                } else {
                    16. * scale
                });
                if let Ok(ext) = cr.text_extents(label) {
                    if held {
                        cr.set_source_rgb(0.02, 0.08, 0.12);
                    } else {
                        cr.set_source_rgb(0.87, 0.9, 0.98);
                    }
                    cr.move_to(
                        x + (w - ext.width()) / 2. - ext.x_bearing(),
                        y + (h - ext.height()) / 2. - ext.y_bearing(),
                    );
                    let _ = cr.show_text(label);
                }
            }
        }
    }
    fn place(window: &gtk::Window, state: &State, layer: Option<Layer>) {
        let (w, h) = panel_size(state.scale);
        window.resize(w, h);
        if let Some(layer) = layer {
            layer.place(window, &state.position, state.x, state.y);
        } else {
            let display = gdk::Display::default();
            let monitor = display
                .as_ref()
                .and_then(|d| d.primary_monitor().or_else(|| d.monitor(0)));
            let geometry = monitor
                .map(|m| m.geometry())
                .unwrap_or(gdk::Rectangle::new(0, 0, 1920, 1080));
            let x = if state.position == "custom" {
                state.x
            } else if state.position.contains("right") {
                geometry.x() + geometry.width() - w - 32
            } else if state.position.contains("center") {
                geometry.x() + (geometry.width() - w) / 2
            } else {
                geometry.x() + 32
            };
            let y = if state.position == "custom" {
                state.y
            } else if state.position.contains("top") {
                geometry.y() + 32
            } else {
                geometry.y() + geometry.height() - h - 32
            };
            window.move_(x, y);
        }
        if let Some(native) = window.window() {
            if state.moving {
                native.input_shape_combine_region(
                    &cairo::Region::create_rectangle(&cairo::RectangleInt::new(0, 0, w, h)),
                    0,
                    0,
                );
            } else {
                native.input_shape_combine_region(&cairo::Region::create(), 0, 0);
            }
        }
    }
    pub fn run() -> Result<()> {
        gtk::init().context("initialize key overlay display")?;
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let window = gtk::Window::new(if wayland {
            gtk::WindowType::Toplevel
        } else {
            gtk::WindowType::Popup
        });
        window.set_title("Vice keys");
        window.set_decorated(false);
        window.set_accept_focus(false);
        window.set_skip_taskbar_hint(true);
        window.set_skip_pager_hint(true);
        window.set_app_paintable(true);
        if let Some(visual) = gtk::prelude::WidgetExt::screen(&window).and_then(|s| s.rgba_visual())
        {
            window.set_visual(Some(&visual));
        }
        let layer = if wayland {
            unsafe { Layer::setup(&window) }
        } else {
            None
        };
        if wayland && layer.is_none() {
            println!("{}", json!({"t":"error","reason":"no-layer-shell"}));
            anyhow::bail!("Keyboard overlay requires gtk-layer-shell and a supported compositor");
        }
        if !wayland {
            window.set_keep_above(true);
        }
        let state = Rc::new(RefCell::new(State {
            scale: 1.,
            position: "bottom-left".into(),
            x: 32,
            y: 32,
            accent: "#8ab9f3".into(),
            ..State::default()
        }));
        let area = gtk::DrawingArea::new();
        let draw_state = state.clone();
        area.connect_draw(move |_, cr| {
            let s = draw_state.borrow();
            draw(cr, &s.accent, s.scale, &s.down, s.moving);
            glib::Propagation::Proceed
        });
        window.add(&area);
        window.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::BUTTON_RELEASE_MASK
                | gdk::EventMask::POINTER_MOTION_MASK,
        );
        let s = state.clone();
        window.connect_button_press_event(move |w, event| {
            let mut s = s.borrow_mut();
            if s.moving && event.button() == 1 {
                let (x, y) = event.root();
                let (wx, wy) = w.position();
                s.drag = Some((x, y, wx, wy));
            }
            glib::Propagation::Proceed
        });
        let s = state.clone();
        window.connect_motion_notify_event(move |w, event| {
            let mut s = s.borrow_mut();
            if let Some((x, y, wx, wy)) = s.drag {
                let (px, py) = event.root();
                s.x = wx + (px - x) as i32;
                s.y = wy + (py - y) as i32;
                s.position = "custom".into();
                place(w, &s, layer);
            }
            glib::Propagation::Proceed
        });
        let s = state.clone();
        window.connect_button_release_event(move |_, _| {
            let mut s = s.borrow_mut();
            if s.drag.take().is_some() {
                println!("{}", json!({"t":"moved","x":s.x,"y":s.y}));
                let _ = std::io::stdout().flush();
            }
            glib::Propagation::Proceed
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(line) => {
                        if let Ok(value) = serde_json::from_str::<Value>(&line) {
                            let _ = tx.send(Some(value));
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(None);
        });
        let s = state.clone();
        let win = window.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            while let Ok(message) = rx.try_recv() {
                let Some(message) = message else {
                    gtk::main_quit();
                    return glib::ControlFlow::Break;
                };
                let mut s = s.borrow_mut();
                match message["t"].as_str() {
                    Some("key") => {
                        if let Some(key) = message["k"].as_str()
                            && crate::keys::LAYOUT_KEYS.contains(&key)
                        {
                            if message["d"].as_bool() == Some(true) {
                                s.down.insert(key.into());
                            } else {
                                s.down.remove(key);
                            }
                        }
                    }
                    Some("style") => {
                        s.accent = message["accent"].as_str().unwrap_or("#8ab9f3").into();
                    }
                    Some("place") => {
                        s.position = message["position"].as_str().unwrap_or("bottom-left").into();
                        s.x = message["x"].as_i64().unwrap_or(32) as i32;
                        s.y = message["y"].as_i64().unwrap_or(32) as i32;
                        s.scale = message["scale"].as_f64().unwrap_or(1.).clamp(0.5, 3.);
                    }
                    Some("move") => s.moving = message["on"].as_bool().unwrap_or(false),
                    _ => {}
                }
                place(&win, &s, layer);
                win.queue_draw();
            }
            glib::ControlFlow::Continue
        });
        window.connect_destroy(|_| gtk::main_quit());
        window.show_all();
        place(&window, &state.borrow(), layer);
        println!("{}", json!({"t":"ready"}));
        gtk::main();
        Ok(())
    }
    /// Raster assets used when burning recorded keys into a video export.
    pub fn render_burn_assets(out: &std::path::Path, accent: &str, scale: f64) -> Result<Value> {
        std::fs::create_dir_all(out)?;
        let (w, h) = panel_size(scale);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h)?;
        let cr = cairo::Context::new(&surface)?;
        draw(&cr, accent, scale, &BTreeSet::new(), false);
        let panel = out.join("panel.png");
        let mut file = std::fs::File::create(&panel)?;
        surface.write_to_png(&mut file)?;
        let mut keys = serde_json::Map::new();
        for (key, _, x, y, kw, kh) in key_rects(scale) {
            let x0 = x.floor() as i32;
            let y0 = y.floor() as i32;
            let cap = cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                (x + kw).ceil() as i32 - x0,
                (y + kh).ceil() as i32 - y0,
            )?;
            let cr = cairo::Context::new(&cap)?;
            cr.translate(-(x0 as f64), -(y0 as f64));
            let mut down = BTreeSet::new();
            down.insert(key.into());
            draw(&cr, accent, scale, &down, false);
            let path = out.join(format!("key-{}.png", key.to_lowercase()));
            let mut file = std::fs::File::create(&path)?;
            cap.write_to_png(&mut file)?;
            keys.insert(key.into(), json!([path, x0, y0]));
        }
        Ok(json!({"panel":panel,"size":[w,h],"keys":keys}))
    }
}
#[cfg(feature = "desktop")]
pub use native::{render_burn_assets, run};
#[cfg(not(feature = "desktop"))]
pub fn run() -> Result<()> {
    anyhow::bail!("Key overlay requires a desktop build")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlay_geometry() {
        assert_eq!(panel_size(1.), (290, 218));
        assert_eq!(key_rects(1.).len(), 17);
    }
}
#[cfg(feature = "desktop")]
pub fn build_burn(
    project: &Value,
    sources: &std::collections::HashMap<String, crate::editor::Source>,
    work: &std::path::Path,
    accent: &str,
) -> Result<crate::editor::KeyBurn> {
    use std::collections::HashMap;
    let mut intervals = HashMap::new();
    for cid in sources.keys() {
        if let Some(values) = crate::keys::load_sidecar(cid) {
            let spans: Vec<_> = values
                .iter()
                .filter_map(|v| {
                    let a = v.get(0)?.as_f64()?;
                    let b = v.get(1)?.as_f64()?;
                    let key = v.get(2)?.as_str()?;
                    if a.is_finite() && b.is_finite() && b > a {
                        Some((a, b, key.to_owned()))
                    } else {
                        None
                    }
                })
                .collect();
            intervals.insert(cid.clone(), spans);
        }
    }
    let (_, height, _) = crate::editor::canvas_for(project, sources);
    let assets = render_burn_assets(work, accent, (height as f64 / 1080.).max(0.5))?;
    let mut keys = HashMap::new();
    if let Some(entries) = assets["keys"].as_object() {
        for (key, value) in entries {
            let path = value[0].as_str().context("key asset path")?;
            let x = value[1].as_i64().context("key asset x")? as i32;
            let y = value[2].as_i64().context("key asset y")? as i32;
            keys.insert(key.clone(), (PathBuf::from(path), x, y));
        }
    }
    Ok(crate::editor::KeyBurn {
        panel: PathBuf::from(assets["panel"].as_str().context("panel asset path")?),
        size: (
            assets["size"][0].as_u64().unwrap_or(290) as u32,
            assets["size"][1].as_u64().unwrap_or(218) as u32,
        ),
        keys,
        intervals,
    })
}
#[cfg(all(test, feature = "desktop"))]
mod rendering_tests {
    use super::*;
    #[test]
    fn render_assets_without_a_display() {
        let dir = tempfile::tempdir().unwrap();
        let assets = render_burn_assets(dir.path(), "#8ab9f3", 1.).unwrap();
        assert_eq!(assets["size"], json!([290, 218]));
        assert_eq!(assets["keys"].as_object().unwrap().len(), 17);
        let panel = std::fs::read(dir.path().join("panel.png")).unwrap();
        assert_eq!(&panel[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(panel[16..20].try_into().unwrap()), 290);
        assert_eq!(u32::from_be_bytes(panel[20..24].try_into().unwrap()), 218);
        for entry in assets["keys"].as_object().unwrap().values() {
            assert!(std::path::Path::new(entry[0].as_str().unwrap()).exists());
        }
    }
}
