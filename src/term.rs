//! Terminal setup: raw mode, mode switches, geometry queries, and Kitty image output.

use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::input::{Ev, Parser};

static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

const ENTER_MODES: &str = concat!(
    "\x1b[?1049h", // alternate screen
    "\x1b[?25l",   // hide cursor
    "\x1b[2J",
    "\x1b[?1000h", // mouse press/release
    "\x1b[?1002h", // drag motion
    "\x1b[?1003h", // all motion (hover)
    "\x1b[?1006h", // SGR encoding
    "\x1b[?1016h", // SGR pixel coordinates (if supported)
    "\x1b[?1004h", // focus in/out
    "\x1b[?2004h", // bracketed paste
    "\x1b[>1u",    // Kitty keyboard: disambiguate escape codes
);

const LEAVE_MODES: &str = concat!(
    "\x1b[<u",
    "\x1b[?2004l",
    "\x1b[?1004l",
    "\x1b[?1016l",
    "\x1b[?1006l",
    "\x1b[?1003l",
    "\x1b[?1002l",
    "\x1b[?1000l",
    "\x1b_Ga=d,d=A,q=2\x1b\\",
    "\x1b]22;default\x1b\\",
    "\x1b[0m\x1b[?25h",
    "\x1b[?1049l",
);

pub fn enter() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0 {
            let _ = SAVED_TERMIOS.set(t);
            let mut raw = t;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        }
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(ENTER_MODES.as_bytes());
    let _ = out.flush();
}

pub fn leave() {
    let mut out = io::stdout().lock();
    let _ = out.write_all(LEAVE_MODES.as_bytes());
    let _ = out.flush();
    if let Some(t) = SAVED_TERMIOS.get() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Geom {
    /// Rows used by the page (all but toolbar and status line).
    pub fn view_rows(&self) -> u16 {
        self.rows.saturating_sub(2).max(1)
    }
    pub fn view_px(&self) -> (f32, f32) {
        (self.cols as f32 * self.cell_w, self.view_rows() as f32 * self.cell_h)
    }
}

/// Read size from the kernel; keep `prev` cell size when the pixel size is unknown.
pub fn ioctl_geom(prev: Option<Geom>) -> Geom {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    let cols = ws.ws_col.max(1);
    let rows = ws.ws_row.max(3);
    let (cell_w, cell_h) = if ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
        (ws.ws_xpixel as f32 / cols as f32, ws.ws_ypixel as f32 / rows as f32)
    } else if let Some(p) = prev {
        (p.cell_w, p.cell_h)
    } else {
        (16.0, 32.0)
    };
    Geom { cols, rows, cell_w, cell_h }
}

/// Ask the terminal for cell size and pixel-mouse support. Waits up to 400 ms.
pub fn query(mut geom: Geom) -> (Geom, bool) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(b"\x1b[16t\x1b[14t\x1b[?1016$p");
    let _ = out.flush();
    drop(out);

    let mut parser = Parser::default();
    let mut events = Vec::new();
    let mut pixel_mouse = false;
    let (mut got_cell, mut got_mode) = (false, false);
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline && !(got_cell && got_mode) {
        let ms = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, ms.max(1)) } <= 0 {
            break;
        }
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        parser.feed(&buf[..n as usize], &mut events);
        for ev in events.drain(..) {
            match ev {
                Ev::CellPx { w, h } if w > 0 && h > 0 => {
                    geom.cell_w = w as f32;
                    geom.cell_h = h as f32;
                    got_cell = true;
                }
                Ev::TextAreaPx { w, h } if w > 0 && h > 0 && !got_cell => {
                    geom.cell_w = w as f32 / geom.cols as f32;
                    geom.cell_h = h as f32 / geom.rows as f32;
                }
                Ev::PixelMouse(on) => {
                    pixel_mouse = on;
                    got_mode = true;
                }
                _ => {}
            }
        }
    }
    (geom, pixel_mouse)
}

/// Ask for the cell size again (after a resize); the reply arrives as an input event.
pub fn request_cell_size() {
    let mut out = io::stdout().lock();
    let _ = out.write_all(b"\x1b[16t");
    let _ = out.flush();
}

/// Write `pixels` to a new POSIX shared memory object. The terminal unlinks it after reading.
fn write_shm(name: &str, pixels: &[u8]) -> io::Result<()> {
    let cname = std::ffi::CString::new(name).unwrap();
    unsafe {
        let fd = libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
            0o600 as libc::c_uint,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let len = pixels.len();
        if libc::ftruncate(fd, len as libc::off_t) != 0 {
            let err = io::Error::last_os_error();
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
            return Err(err);
        }
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        libc::close(fd);
        if ptr == libc::MAP_FAILED {
            libc::shm_unlink(cname.as_ptr());
            return Err(io::Error::last_os_error());
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr as *mut u8, len);
        libc::munmap(ptr, len);
    }
    Ok(())
}

/// True while the shared memory object `name` exists (the terminal has not read it yet).
fn shm_exists(name: &str) -> bool {
    let cname = std::ffi::CString::new(name).unwrap();
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint) };
    if fd >= 0 {
        unsafe { libc::close(fd) };
        return true;
    }
    // Any error other than "not found" (for example EACCES) counts as "still there".
    io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
}

fn shm_unlink(name: &str) {
    let cname = std::ffi::CString::new(name).unwrap();
    unsafe { libc::shm_unlink(cname.as_ptr()) };
}

/// If the terminal does not unlink a frame in this time, we unlink it and go on.
const FRAME_ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Kitty {
    frame_no: u64,
    /// Two image ids in turn, so the old frame stays until the new one is placed.
    current_id: Option<u32>,
    /// The last frame sent, until the terminal unlinks it (its "ack").
    in_flight: Option<(String, Instant)>,
}

impl Kitty {
    /// Backpressure: true when the terminal has read the last frame, so a new
    /// frame can go. The terminal unlinks each shared memory object after it
    /// reads it; while the object exists, the terminal is behind.
    pub fn ready(&mut self) -> bool {
        let Some((name, sent)) = &self.in_flight else { return true };
        if shm_exists(name) {
            if sent.elapsed() < FRAME_ACK_TIMEOUT {
                return false;
            }
            // The terminal could not read it (or never will). Do not leak it.
            eprintln!("kitty: frame {name} not read after {FRAME_ACK_TIMEOUT:?}; unlinking");
            shm_unlink(name);
        }
        self.in_flight = None;
        true
    }

    /// Place the frame on row 2, scaled to cover the page area.
    pub fn show(&mut self, img: &servo::RgbaImage, geom: Geom) -> io::Result<()> {
        self.frame_no += 1;
        let name = format!("/svt{}-{}", std::process::id(), self.frame_no);
        let bytes = img.as_raw();
        write_shm(&name, bytes)?;
        self.in_flight = Some((name.clone(), Instant::now()));

        let id = if self.current_id == Some(1) { 2 } else { 1 };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&name);
        let mut out = io::stdout().lock();
        write!(
            out,
            "\x1b[2;1H\x1b_Ga=T,t=s,f=32,s={},v={},S={},i={id},p=1,c={},r={},C=1,q=2;{b64}\x1b\\",
            img.width(),
            img.height(),
            bytes.len(),
            geom.cols,
            geom.view_rows(),
        )?;
        if let Some(old) = self.current_id {
            write!(out, "\x1b_Ga=d,d=I,i={old},q=2\x1b\\")?;
        }
        out.flush()?;
        self.current_id = Some(id);
        Ok(())
    }

    pub fn clear(&mut self) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J");
        let _ = out.flush();
        self.current_id = None;
    }

    /// Remove a frame the terminal has not read (call on exit).
    pub fn discard_in_flight(&mut self) {
        if let Some((name, _)) = self.in_flight.take() {
            shm_unlink(&name);
        }
    }
}
