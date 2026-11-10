//! servo-term: a web browser inside the terminal.
//!
//! Servo renders offscreen; each frame goes to the terminal through the Kitty
//! graphics protocol using POSIX shared memory (`t=s`). Mouse, keyboard and
//! paste input from the terminal go back to Servo.
//!
//! Usage: servo-term [URL] [--scale F] [--dpr F]

mod input;
mod pdf;
mod term;

use std::cell::{Cell, RefCell};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    Code, CreateNewWebViewRequest, Cursor, DeviceIntPoint, DeviceIntRect, DevicePoint,
    DeviceVector2D, EmbedderControl, EventLoopWaker, InputEvent, Key, KeyState, KeyboardEvent,
    LoadStatus, Location, Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent,
    MouseMoveEvent, NamedKey, NavigationRequest, Preferences, RenderingContext, Scroll, Servo, ServoBuilder, SimpleDialog,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
};
use servo::protocol_handler::ProtocolRegistry;
use url::Url;

use input::{Ev, Parser};
use term::{Geom, Kitty};

/// Column where the URL starts in the toolbar (after " ←  →  ↻  │ ").
const TOOLBAR_URL_COL: usize = 11;

/// Frame-rate cap: at most 30 frames per second. Full-screen RGBA frames are
/// large, and the terminal can ack a frame before it has drawn it, so the ack
/// alone does not stop us from sending more than the terminal can show.
const MIN_FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / 30);

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

struct Opts {
    url: Url,
    /// Render at this fraction of the terminal's pixel size.
    scale: f32,
    /// Device pixels per CSS pixel; None = guess from the cell size.
    dpr: Option<f32>,
}

fn parse_opts() -> Opts {
    let mut opts = Opts {
        url: Url::parse("https://en.wikipedia.org/wiki/Special:Random").unwrap(),
        scale: 1.0,
        dpr: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut num = |name: &str| -> f32 {
            args.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{name} needs a number"))
        };
        match arg.as_str() {
            "--scale" => opts.scale = num("--scale").clamp(0.1, 1.0),
            "--dpr" => opts.dpr = Some(num("--dpr")),
            other => opts.url = normalize_url(other),
        }
    }
    opts
}

/// Turn URL-bar text into a URL: full URLs as-is, host-like text gets a
/// scheme, anything else becomes a search.
fn normalize_url(text: &str) -> Url {
    let url = normalize_url_inner(text);
    pdf::route(&url).unwrap_or(url)
}

fn normalize_url_inner(text: &str) -> Url {
    let text = text.trim();
    // A local path (absolute, ~/, or relative to the current directory).
    let expanded = match text.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME").map(|h| format!("{h}/{rest}")).unwrap_or_default(),
        None => text.to_string(),
    };
    if !text.contains("://") && std::path::Path::new(&expanded).is_file() {
        if let Ok(abs) = std::fs::canonicalize(&expanded) {
            if let Ok(url) = Url::from_file_path(abs) {
                return url;
            }
        }
    }
    if let Ok(url) = Url::parse(text) {
        if text.contains("://") || ["about", "data", "file"].contains(&url.scheme()) {
            return url;
        }
    }
    let host_like = !text.contains(' ') && (text.contains('.') || text.starts_with("localhost"));
    if host_like {
        let local = text.starts_with("localhost") || text.starts_with("127.");
        let scheme = if local { "http" } else { "https" };
        if let Ok(url) = Url::parse(&format!("{scheme}://{text}")) {
            return url;
        }
    }
    let q: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    Url::parse(&format!("https://html.duckduckgo.com/html/?q={q}")).unwrap()
}

// ---------------------------------------------------------------------------
// Servo glue
// ---------------------------------------------------------------------------

struct Delegate {
    rendering_context: Rc<SoftwareRenderingContext>,
    frame_ready: Cell<bool>,
    chrome_dirty: Cell<bool>,
    new_tabs: RefCell<Vec<WebView>>,
    cursor: RefCell<Option<Cursor>>,
    message: RefCell<Option<(String, Instant)>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.frame_ready.set(true);
    }
    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        self.chrome_dirty.set(true);
        // A PDF without a .pdf URL shows Servo's "Unknown content type" page; the
        // script swaps it for our viewer.
        let is_web = webview.url().is_some_and(|u| matches!(u.scheme(), "http" | "https"));
        if status == LoadStatus::Complete && is_web {
            webview.evaluate_javascript(pdf::DETECT_SCRIPT, |_| {});
        }
    }
    fn request_navigation(&self, webview: WebView, request: NavigationRequest) {
        // Links to .pdf files go straight to the viewer.
        match pdf::route(&request.url) {
            Some(viewer) => {
                request.deny();
                webview.load(viewer);
            }
            None => request.allow(),
        }
    }
    fn notify_url_changed(&self, _: WebView, _: Url) {
        self.chrome_dirty.set(true);
    }
    fn notify_page_title_changed(&self, _: WebView, _: Option<String>) {
        self.chrome_dirty.set(true);
    }
    fn notify_status_text_changed(&self, _: WebView, _: Option<String>) {
        self.chrome_dirty.set(true);
    }
    fn notify_cursor_changed(&self, _: WebView, cursor: Cursor) {
        *self.cursor.borrow_mut() = Some(cursor);
    }
    fn request_create_new(&self, parent: WebView, request: CreateNewWebViewRequest) {
        let webview = request
            .builder(self.rendering_context.clone())
            .hidpi_scale_factor(parent.hidpi_scale_factor())
            .delegate(parent.delegate())
            .build();
        self.new_tabs.borrow_mut().push(webview);
    }
    fn show_embedder_control(&self, _: WebView, control: EmbedderControl) {
        // No native pickers here. Show dialog text in the status line; dropping
        // the control sends Servo the default answer.
        if let EmbedderControl::SimpleDialog(dialog) = &control {
            let kind = match dialog {
                SimpleDialog::Alert(_) => "alert",
                SimpleDialog::Confirm(_) => "confirm",
                SimpleDialog::Prompt(_) => "prompt",
            };
            *self.message.borrow_mut() =
                Some((format!("page {kind}: {}", dialog.message()), Instant::now()));
            self.chrome_dirty.set(true);
        }
    }
}

/// We poll Servo's event loop every couple of milliseconds, so wakeups are not needed.
#[derive(Clone)]
struct Waker;
impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(Waker)
    }
    fn wake(&self) {}
}

/// Where Servo's log goes: $SERVO_TERM_LOG, else ~/.cache/servo-term/servo-term.log.
fn log_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("SERVO_TERM_LOG") {
        return p.into();
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    let dir = std::path::Path::new(&home).join(".cache/servo-term");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("servo-term.log")
}

/// Send Servo's stderr logging to a file so it does not draw over the page.
fn redirect_stderr() {
    // Truncate on each start so the log does not grow forever.
    if let Ok(f) = OpenOptions::new().create(true).write(true).truncate(true).open(log_path()) {
        unsafe { libc::dup2(f.as_raw_fd(), libc::STDERR_FILENO) };
        std::mem::forget(f);
    }
}

/// The URL to show in the URL bar: for the PDF viewer, the PDF's own URL.
fn shown_url(wv: &WebView) -> String {
    wv.url()
        .map(|u| pdf::display_url(&u).unwrap_or_else(|| u.to_string()))
        .unwrap_or_default()
}

/// CSS cursor name for OSC 22 (pointer shape).
fn css_cursor(c: &Cursor) -> &'static str {
    match c {
        Cursor::Pointer => "pointer",
        Cursor::Text | Cursor::VerticalText => "text",
        Cursor::Wait => "wait",
        Cursor::Progress => "progress",
        Cursor::Help => "help",
        Cursor::Crosshair => "crosshair",
        Cursor::Move | Cursor::AllScroll => "move",
        Cursor::NotAllowed | Cursor::NoDrop => "not-allowed",
        Cursor::Grab => "grab",
        Cursor::Grabbing => "grabbing",
        Cursor::EwResize | Cursor::EResize | Cursor::WResize | Cursor::ColResize => "ew-resize",
        Cursor::NsResize | Cursor::NResize | Cursor::SResize | Cursor::RowResize => "ns-resize",
        Cursor::NeswResize | Cursor::NeResize | Cursor::SwResize => "nesw-resize",
        Cursor::NwseResize | Cursor::NwResize | Cursor::SeResize => "nwse-resize",
        Cursor::Cell => "cell",
        Cursor::Copy => "copy",
        Cursor::Alias => "alias",
        Cursor::ZoomIn => "zoom-in",
        Cursor::ZoomOut => "zoom-out",
        Cursor::ContextMenu => "context-menu",
        Cursor::None => "none",
        Cursor::Default => "default",
    }
}

// ---------------------------------------------------------------------------
// URL bar
// ---------------------------------------------------------------------------

struct UrlEdit {
    text: Vec<char>,
    caret: usize,
    /// All text is selected; the next typed character replaces it.
    all_selected: bool,
}

impl UrlEdit {
    fn insert(&mut self, s: &str) {
        if self.all_selected {
            self.text.clear();
            self.caret = 0;
            self.all_selected = false;
        }
        for c in s.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.caret, c);
            self.caret += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// Device pixels per CSS pixel. Retina terminals have tall cells; use 2x there
/// so CSS pixels look normal.
fn dpr_for(geom: Geom, dpr_override: Option<f32>) -> f32 {
    dpr_override.unwrap_or(if geom.cell_h >= 26.0 { 2.0 } else { 1.0 })
}

struct App {
    dpr_override: Option<f32>,
    servo: Servo,
    ctx: Rc<SoftwareRenderingContext>,
    delegate: Rc<Delegate>,
    tabs: Vec<WebView>,
    active: usize,
    geom: Geom,
    scale: f32,
    pixel_mouse: bool,
    render_size: (u32, u32),
    kitty: Kitty,
    url_edit: Option<UrlEdit>,
    dragging: bool,
    quit: bool,
    frames: u32,
    fps: f32,
    stats_at: Instant,
    /// When the last frame went to the terminal (for the frame-rate cap).
    last_paint: Instant,
    snapshot_path: Option<String>,
}

impl App {
    fn webview(&self) -> &WebView {
        &self.tabs[self.active]
    }

    fn render_size_for(geom: Geom, scale: f32) -> (u32, u32) {
        let (w, h) = geom.view_px();
        (((w * scale) as u32).max(1), ((h * scale) as u32).max(1))
    }

    fn apply_geometry(&mut self, geom: Geom) {
        if geom == self.geom {
            return;
        }
        self.geom = geom;
        let (w, h) = Self::render_size_for(geom, self.scale);
        if (w, h) != self.render_size {
            self.render_size = (w, h);
            // WebView::resize resizes the shared RenderingContext and every WebView's
            // viewport. Do not call ctx.resize() first: Servo then sees no size change
            // and skips the viewport update.
            self.webview().resize(PhysicalSize::new(w, h));
        }
        let hidpi = Scale::new(dpr_for(geom, self.dpr_override) * self.scale);
        for tab in &self.tabs {
            tab.set_hidpi_scale_factor(hidpi);
        }
        self.kitty.clear();
        self.delegate.frame_ready.set(true);
        self.delegate.chrome_dirty.set(true);
    }

    fn switch_to(&mut self, index: usize) {
        for (i, tab) in self.tabs.iter().enumerate() {
            if i == index {
                tab.show();
                tab.set_focused(true);
            } else {
                tab.hide();
                tab.set_focused(false);
            }
        }
        self.active = index;
        self.delegate.frame_ready.set(true);
        self.delegate.chrome_dirty.set(true);
    }

    fn close_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.tabs.remove(self.active);
            self.switch_to(self.active.min(self.tabs.len() - 1));
        }
    }

    fn navigate(&mut self, text: &str) {
        let url = normalize_url(text);
        self.webview().load(url);
        self.webview().set_focused(true);
    }

    fn start_url_edit(&mut self) {
        let text: Vec<char> = shown_url(self.webview()).chars().collect();
        let caret = text.len();
        self.url_edit = Some(UrlEdit { text, caret, all_selected: true });
        self.delegate.chrome_dirty.set(true);
    }

    fn set_message(&self, msg: impl Into<String>) {
        *self.delegate.message.borrow_mut() = Some((msg.into(), Instant::now()));
        self.delegate.chrome_dirty.set(true);
    }

    // -- input --------------------------------------------------------------

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Key { key, mods } => self.on_key(key, mods),
            Ev::Paste(text) => {
                if let Some(edit) = &mut self.url_edit {
                    edit.insert(&text.replace(['\n', '\r'], " "));
                    self.delegate.chrome_dirty.set(true);
                } else {
                    for c in text.chars() {
                        let key = match c {
                            '\n' | '\r' => Key::Named(NamedKey::Enter),
                            '\t' => Key::Named(NamedKey::Tab),
                            c => Key::Character(c.to_string()),
                        };
                        self.send_key(key, Modifiers::empty());
                    }
                }
            }
            Ev::Mouse { btn, x, y, press } => self.on_mouse(btn, x, y, press),
            Ev::Focus(focused) => self.webview().set_focused(focused),
            Ev::PixelMouse(on) => self.pixel_mouse = on,
            Ev::CellPx { w, h } if w > 0 && h > 0 => {
                let g = Geom { cell_w: w as f32, cell_h: h as f32, ..self.geom };
                self.apply_geometry(g);
            }
            Ev::TextAreaPx { .. } | Ev::CellPx { .. } => {}
        }
    }

    fn on_key(&mut self, key: Key, mods: Modifiers) {
        let ctrl = mods.contains(Modifiers::CONTROL);
        let alt = mods.contains(Modifiers::ALT);
        let ch = match &key {
            Key::Character(s) => s.to_lowercase(),
            _ => String::new(),
        };
        let left = matches!(key, Key::Named(NamedKey::ArrowLeft)) || ch == "b";
        let right = matches!(key, Key::Named(NamedKey::ArrowRight)) || ch == "f";

        // Browser shortcuts.
        if ctrl {
            match ch.as_str() {
                "q" | "c" => return self.quit = true,
                "l" | "t" => return self.start_url_edit(),
                "r" => return self.webview().reload(),
                "w" => return self.close_tab(),
                "=" | "+" => return self.zoom(1.1),
                "-" => return self.zoom(1.0 / 1.1),
                "0" => return self.webview().set_page_zoom(1.0),
                _ => {}
            }
        }
        if matches!(key, Key::Named(NamedKey::F5)) {
            return self.webview().reload();
        }
        if alt && self.url_edit.is_none() && (left || right) {
            return self.go(if left { -1 } else { 1 });
        }

        if self.url_edit.is_some() {
            self.url_key(key, mods);
        } else {
            self.send_key(key, mods);
        }
    }

    fn zoom(&self, factor: f32) {
        let wv = self.webview();
        wv.set_page_zoom((wv.page_zoom() * factor).clamp(0.25, 5.0));
    }

    fn go(&self, dir: i32) {
        let wv = self.webview();
        if dir < 0 && wv.can_go_back() {
            wv.go_back(1);
        } else if dir > 0 && wv.can_go_forward() {
            wv.go_forward(1);
        }
    }

    fn url_key(&mut self, key: Key, mods: Modifiers) {
        let ctrl = mods.contains(Modifiers::CONTROL);
        self.delegate.chrome_dirty.set(true);
        let edit = self.url_edit.as_mut().unwrap();
        match key {
            Key::Named(NamedKey::Enter) => {
                let text: String = edit.text.iter().collect();
                self.url_edit = None;
                if !text.trim().is_empty() {
                    self.navigate(&text);
                }
            }
            Key::Named(NamedKey::Escape) => self.url_edit = None,
            Key::Named(NamedKey::Backspace) => {
                if edit.all_selected {
                    edit.insert("");
                } else if edit.caret > 0 {
                    edit.caret -= 1;
                    edit.text.remove(edit.caret);
                }
            }
            Key::Named(NamedKey::Delete) => {
                if edit.all_selected {
                    edit.insert("");
                } else if edit.caret < edit.text.len() {
                    edit.text.remove(edit.caret);
                }
            }
            Key::Named(NamedKey::ArrowLeft) => {
                edit.all_selected = false;
                edit.caret = edit.caret.saturating_sub(1);
            }
            Key::Named(NamedKey::ArrowRight) => {
                edit.all_selected = false;
                edit.caret = (edit.caret + 1).min(edit.text.len());
            }
            Key::Named(NamedKey::Home) => {
                edit.all_selected = false;
                edit.caret = 0;
            }
            Key::Named(NamedKey::End) => {
                edit.all_selected = false;
                edit.caret = edit.text.len();
            }
            Key::Character(s) if ctrl => match s.as_str() {
                "a" => {
                    edit.all_selected = false;
                    edit.caret = 0;
                }
                "e" => {
                    edit.all_selected = false;
                    edit.caret = edit.text.len();
                }
                "u" => {
                    edit.text.clear();
                    edit.caret = 0;
                    edit.all_selected = false;
                }
                _ => {}
            },
            Key::Character(s) if !mods.contains(Modifiers::ALT) => edit.insert(&s),
            _ => {}
        }
    }

    fn send_key(&self, key: Key, mods: Modifiers) {
        let wv = self.webview();
        for state in [KeyState::Down, KeyState::Up] {
            wv.notify_input_event(InputEvent::Keyboard(KeyboardEvent::new_without_event(
                state,
                key.clone(),
                Code::Unidentified,
                Location::Standard,
                mods,
                false,
                false,
            )));
        }
    }

    fn on_mouse(&mut self, btn: u16, x: i32, y: i32, press: bool) {
        let g = self.geom;
        // Terminal pixel position (0-based).
        let (px, py) = if self.pixel_mouse {
            ((x - 1).max(0) as f32, (y - 1).max(0) as f32)
        } else {
            (((x - 1) as f32 + 0.5) * g.cell_w, ((y - 1) as f32 + 0.5) * g.cell_h)
        };
        let row = (py / g.cell_h) as u16;
        let col = (px / g.cell_w) as usize;
        let motion = btn & 32 != 0;
        let wheel = btn & 64 != 0;
        let button = btn & 3;
        let in_page = row >= 1 && row <= g.view_rows();

        // Toolbar clicks.
        if !motion && !wheel && press && row == 0 && button == 0 {
            match col {
                0..=2 => self.go(-1),
                3..=5 => self.go(1),
                6..=8 => self.webview().reload(),
                _ => self.start_url_edit(),
            }
            return;
        }
        if !in_page && !self.dragging {
            return;
        }

        let (vw, vh) = g.view_px();
        let point = DevicePoint::new(
            px.clamp(0.0, vw - 1.0) * self.scale,
            (py - g.cell_h).clamp(0.0, vh - 1.0) * self.scale,
        );
        let wv = self.webview().clone();

        if wheel {
            let step = 3.0 * g.cell_h * self.scale;
            let (dx, dy) = match button {
                0 => (0.0, -step),
                1 => (0.0, step),
                2 => (-step, 0.0),
                _ => (step, 0.0),
            };
            wv.notify_scroll_event(Scroll::Delta(DeviceVector2D::new(dx, dy).into()), point.into());
            return;
        }

        if motion {
            wv.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.into())));
            return;
        }

        if press && self.url_edit.is_some() {
            self.url_edit = None;
            self.delegate.chrome_dirty.set(true);
        }
        let mouse_button = match button {
            0 => MouseButton::Primary,
            1 => MouseButton::Auxiliary,
            _ => MouseButton::Secondary,
        };
        let action = if press { MouseButtonAction::Down } else { MouseButtonAction::Up };
        self.dragging = press;
        if press {
            wv.set_focused(true);
            wv.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.into())));
        }
        wv.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
            action,
            mouse_button,
            point.into(),
        )));
    }

    // -- output -------------------------------------------------------------

    fn paint_frame(&mut self) {
        // While the terminal is still reading the last frame, or the frame-rate
        // cap is not done, keep `frame_ready` set and paint the newest frame
        // later. Frames in between drop.
        if !self.delegate.frame_ready.get()
            || self.last_paint.elapsed() < MIN_FRAME_INTERVAL
            || !self.kitty.ready()
        {
            return;
        }
        self.delegate.frame_ready.set(false);
        self.last_paint = Instant::now();
        let (w, h) = self.render_size;
        self.webview().paint();
        let rect =
            DeviceIntRect::new(DeviceIntPoint::origin(), DeviceIntPoint::new(w as i32, h as i32));
        if let Some(img) = self.ctx.read_to_image(rect) {
            self.ctx.present();
            if let Err(e) = self.kitty.show(&img, self.geom) {
                self.set_message(format!("kitty output error: {e}"));
            }
            self.frames += 1;
            // Debug aid: SERVO_TERM_SNAPSHOT=path.png saves every frame (slow).
            if let Some(path) = &self.snapshot_path {
                let _ = img.save(path);
            }
        }
    }

    fn draw_chrome(&mut self) {
        if self.stats_at.elapsed() >= Duration::from_millis(500) {
            self.fps = self.frames as f32 / self.stats_at.elapsed().as_secs_f32();
            self.frames = 0;
            self.stats_at = Instant::now();
            self.delegate.chrome_dirty.set(true);
        }
        let mut out = String::new();
        if let Some(cursor) = self.delegate.cursor.borrow_mut().take() {
            out.push_str(&format!("\x1b]22;{}\x1b\\", css_cursor(&cursor)));
        }
        if self.delegate.chrome_dirty.replace(false) {
            self.chrome_text(&mut out);
        }
        if !out.is_empty() {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
        }
    }

    fn chrome_text(&self, out: &mut String) {
        let cols = self.geom.cols as usize;
        let wv = self.webview();
        const BAR: &str = "\x1b[48;5;236m";
        const ON: &str = "\x1b[38;5;255m";
        const OFF: &str = "\x1b[38;5;242m";

        // Toolbar: " ←  →  ↻  │ url"
        let back = if wv.can_go_back() { ON } else { OFF };
        let fwd = if wv.can_go_forward() { ON } else { OFF };
        out.push_str(&format!("\x1b[1;1H{BAR}\x1b[2K {back}←  {fwd}→  {ON}↻  {OFF}│ \x1b[38;5;252m"));
        let avail = cols.saturating_sub(TOOLBAR_URL_COL + 1);
        match &self.url_edit {
            Some(edit) => {
                let start = edit.caret.saturating_sub(avail.saturating_sub(1));
                let shown: Vec<char> = edit.text.iter().skip(start).take(avail).copied().collect();
                out.push_str("\x1b[48;5;238m\x1b[38;5;255m\x1b[K");
                if edit.all_selected {
                    out.push_str("\x1b[7m");
                    out.extend(shown.iter());
                    out.push_str("\x1b[27m");
                } else {
                    let caret = edit.caret - start;
                    for (i, c) in shown.iter().enumerate() {
                        if i == caret {
                            out.push_str(&format!("\x1b[7m{c}\x1b[27m"));
                        } else {
                            out.push(*c);
                        }
                    }
                    if caret >= shown.len() {
                        out.push_str("\x1b[7m \x1b[27m");
                    }
                }
            }
            None => {
                let url = shown_url(wv);
                out.extend(url.chars().take(avail));
            }
        }
        out.push_str("\x1b[0m");

        // Status line: message / hovered link / title on the left, hints on the right.
        let msg = self
            .delegate
            .message
            .borrow()
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
            .map(|(m, _)| m.clone());
        let load = match wv.load_status() {
            LoadStatus::Complete => "",
            LoadStatus::HeadParsed => "loading… ",
            _ => "connecting… ",
        };
        let left = msg
            .or_else(|| wv.status_text())
            .unwrap_or_else(|| format!("{load}{}", wv.page_title().unwrap_or_default()));
        let tabs = if self.tabs.len() > 1 {
            format!("tab {}/{} ^W close │ ", self.active + 1, self.tabs.len())
        } else {
            String::new()
        };
        let right = format!(" {tabs}{:3.0}fps │ ^L url  ⌥←→ nav  ^R reload  ^Q quit ", self.fps);
        let right: String = right.chars().take(cols.saturating_sub(1)).collect();
        let right_len = right.chars().count();
        let left_room = cols.saturating_sub(right_len + 1);
        let left: String = left.chars().filter(|c| !c.is_control()).take(left_room).collect();
        let pad = left_room - left.chars().count();
        out.push_str(&format!(
            "\x1b[{};1H{BAR}\x1b[38;5;250m\x1b[2K {left}{}{right}\x1b[0m",
            self.geom.rows,
            " ".repeat(pad)
        ));
    }
}

fn main() {
    let opts = parse_opts();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    redirect_stderr();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::leave();
        default_hook(info);
    }));

    term::enter();
    let (geom, pixel_mouse) = term::query(term::ioctl_geom(None));
    let dpr = dpr_for(geom, opts.dpr);
    eprintln!("geometry {geom:?} pixel_mouse={pixel_mouse} dpr={dpr}");

    let winch = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGWINCH, winch.clone()).unwrap();
    let term_signal = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term_signal.clone()).unwrap();
    signal_hook::flag::register(signal_hook::consts::SIGHUP, term_signal.clone()).unwrap();

    // Input thread: read stdin, parse, forward events to the main thread.
    let (tx, rx) = mpsc::channel::<Ev>();
    std::thread::spawn(move || {
        let mut parser = Parser::default();
        let mut buf = [0u8; 8192];
        let mut events = Vec::new();
        loop {
            let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
            let timeout = if parser.pending() { 25 } else { 500 };
            let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if r > 0 {
                let n =
                    unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    return;
                }
                parser.feed(&buf[..n as usize], &mut events);
            } else if r == 0 && parser.pending() {
                parser.flush(&mut events);
            }
            for ev in events.drain(..) {
                if tx.send(ev).is_err() {
                    return;
                }
            }
        }
    });

    let scale = opts.scale;
    let (w, h) = App::render_size_for(geom, scale);
    let ctx = Rc::new(
        SoftwareRenderingContext::new(PhysicalSize::new(w, h))
            .expect("could not create SoftwareRenderingContext"),
    );
    ctx.make_current().expect("make_current failed");

    // WebGL 1 is on whenever the `webgl` feature is compiled in; WebGL 2 and
    // WebGPU are off by default in Servo's preferences.
    let mut prefs = Preferences::default();
    prefs.dom_webgl2_enabled = true;
    prefs.dom_webgpu_enabled = true;
    let servo = ServoBuilder::default()
        .preferences(prefs)
        .protocol_registry({
            let mut protocols = ProtocolRegistry::default();
            protocols.register("pdf", pdf::PdfProtocol).expect("register pdf: scheme");
            protocols
        })
        .event_loop_waker(Box::new(Waker))
        .build();
    let delegate = Rc::new(Delegate {
        rendering_context: ctx.clone(),
        frame_ready: Cell::new(false),
        chrome_dirty: Cell::new(true),
        new_tabs: RefCell::new(Vec::new()),
        cursor: RefCell::new(None),
        message: RefCell::new(None),
    });
    let webview = WebViewBuilder::new(&servo, ctx.clone())
        .url(opts.url.clone())
        .hidpi_scale_factor(Scale::new(dpr * scale))
        .delegate(delegate.clone())
        .build();
    webview.set_focused(true);

    let mut app = App {
        dpr_override: opts.dpr,
        servo,
        ctx,
        delegate,
        tabs: vec![webview],
        active: 0,
        geom,
        scale,
        pixel_mouse,
        render_size: (w, h),
        kitty: Kitty::default(),
        url_edit: None,
        dragging: false,
        quit: false,
        frames: 0,
        fps: 0.0,
        stats_at: Instant::now(),
        last_paint: Instant::now() - MIN_FRAME_INTERVAL,
        snapshot_path: std::env::var("SERVO_TERM_SNAPSHOT").ok(),
    };

    while !app.quit && !term_signal.load(Ordering::Relaxed) {
        if winch.swap(false, Ordering::Relaxed) {
            let g = term::ioctl_geom(Some(app.geom));
            app.apply_geometry(g);
            term::request_cell_size();
        }

        // Wait briefly for input so typing feels instant without busy-waiting.
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(2)) {
            app.handle(ev);
            while let Ok(ev) = rx.try_recv() {
                app.handle(ev);
            }
        }

        app.servo.spin_event_loop();

        let new_tabs: Vec<WebView> = app.delegate.new_tabs.borrow_mut().drain(..).collect();
        for tab in new_tabs {
            app.tabs.push(tab);
            let last = app.tabs.len() - 1;
            app.switch_to(last);
        }

        app.paint_frame();
        app.draw_chrome();
    }

    app.kitty.discard_in_flight();
    term::leave();
}
