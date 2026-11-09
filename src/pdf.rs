//! PDF viewing. Servo has no PDF viewer, so we register a `pdf:` URL scheme:
//!
//! - `pdf:view?u=<url>` or `pdf:view?id=<token>` returns an HTML page with one
//!   `<img>` per PDF page.
//! - `pdf:page?n=<index>&u=<url>` (or `&id=`) returns one page as a PNG, rendered
//!   with hayro (a pure-Rust PDF rasterizer) on a small worker pool.
//!
//! Remote PDFs (`u=`) are downloaded by this module. Local files are read by the
//! embedder and registered under a random token (`id=`), so web content cannot
//! make this handler read arbitrary local paths.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};

use futures::channel::oneshot;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use http::HeaderValue;
use servo::ServoUrl;
use servo::protocol_handler::{
    DoneChannel, FetchContext, HttpStatus, NetworkError, ProtocolHandler, Request,
    ResourceFetchTiming, Response, ResponseBody,
};
use url::Url;

/// Device pixels per PDF point for page images (2x of 72 dpi = sharp on Retina).
const RENDER_SCALE: f32 = 2.0;
/// Largest PDF we download.
const MAX_PDF_BYTES: u64 = 256 * 1024 * 1024;
const WORKERS: usize = 4;

type Job = Box<dyn FnOnce() + Send>;

/// Downloaded or registered PDF bytes, keyed by "u:<url>" or "id:<token>".
fn store() -> &'static Mutex<HashMap<String, Arc<Vec<u8>>>> {
    static STORE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// Run `job` on the render pool. Bounded so a 500-page PDF does not start 500 threads.
fn spawn(job: Job) {
    static POOL: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..WORKERS {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("pdf-{i}"))
                .spawn(move || loop {
                    let job = rx.lock().unwrap().recv();
                    match job {
                        Ok(job) => job(),
                        Err(_) => return,
                    }
                })
                .unwrap();
        }
        Mutex::new(tx)
    });
    let _ = pool.lock().unwrap().send(job);
}

/// Register local PDF bytes and return the viewer URL for them.
pub fn register_local(bytes: Vec<u8>, name: &str) -> Url {
    let token = format!("{:016x}{:016x}", rand_u64(), rand_u64());
    store().lock().unwrap().insert(format!("id:{token}"), Arc::new(bytes));
    let mut url = Url::parse("pdf:view").unwrap();
    url.query_pairs_mut().append_pair("id", &token).append_pair("name", name);
    url
}

fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    unsafe { libc::arc4random_buf(b.as_mut_ptr().cast(), b.len()) };
    u64::from_ne_bytes(b)
}

/// Viewer URL for a remote PDF.
pub fn view_url(pdf_url: &Url) -> Url {
    let mut url = Url::parse("pdf:view").unwrap();
    url.query_pairs_mut().append_pair("u", pdf_url.as_str());
    url
}

/// True if the URL path looks like a PDF file.
pub fn looks_like_pdf(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "file")
        && url.path().to_ascii_lowercase().ends_with(".pdf")
}

/// For a local file: read it and return a viewer URL. For http(s): the viewer URL.
pub fn route(url: &Url) -> Option<Url> {
    if !looks_like_pdf(url) {
        return None;
    }
    if url.scheme() == "file" {
        let path = url.to_file_path().ok()?;
        let bytes = std::fs::read(&path).ok()?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Some(register_local(bytes, &name));
    }
    Some(view_url(url))
}

/// Script that replaces Servo's "Unknown content type (application/pdf)" page with
/// the viewer, for PDFs whose URL does not end in `.pdf`.
pub const DETECT_SCRIPT: &str = r#"(function () {
  var b = document.body;
  if (b && /^\s*Unknown content type \(application\/pdf/.test(b.textContent) &&
      /^https?:/.test(location.href)) {
    location.replace('pdf:view?u=' + encodeURIComponent(location.href));
  }
})()"#;

/// The original document URL shown in the URL bar for a viewer page.
pub fn display_url(url: &Url) -> Option<String> {
    if url.scheme() != "pdf" {
        return None;
    }
    let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
    q.get("u").cloned().or_else(|| q.get("name").map(|n| format!("file: {n}")))
}

// ---------------------------------------------------------------------------
// Loading and rendering
// ---------------------------------------------------------------------------

struct Source {
    key: String,
    remote: Option<String>,
}

impl Source {
    fn from_query(q: &HashMap<String, String>) -> Option<Source> {
        if let Some(id) = q.get("id") {
            return Some(Source { key: format!("id:{id}"), remote: None });
        }
        let u = q.get("u")?;
        let parsed = Url::parse(u).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        Some(Source { key: format!("u:{u}"), remote: Some(u.clone()) })
    }

    /// Bytes from the store, downloading them first if needed. Blocking.
    fn bytes(&self) -> Result<Arc<Vec<u8>>, String> {
        if let Some(b) = store().lock().unwrap().get(&self.key) {
            return Ok(b.clone());
        }
        let Some(url) = &self.remote else {
            return Err("this local PDF is no longer loaded; open it again".into());
        };
        let mut resp = ureq::get(url).call().map_err(|e| format!("download failed: {e}"))?;
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_PDF_BYTES)
            .read_to_vec()
            .map_err(|e| format!("download failed: {e}"))?;
        let bytes = Arc::new(bytes);
        store().lock().unwrap().insert(self.key.clone(), bytes.clone());
        Ok(bytes)
    }
}

fn open(bytes: Arc<Vec<u8>>) -> Result<Pdf, String> {
    Pdf::new(bytes).map_err(|e| format!("not a readable PDF: {e:?}"))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn viewer_html(src: &Source, q: &HashMap<String, String>) -> Result<String, String> {
    let pdf = open(src.bytes()?)?;
    let pages = pdf.pages();
    let title = q
        .get("name")
        .cloned()
        .or_else(|| {
            q.get("u").and_then(|u| Url::parse(u).ok()).and_then(|u| {
                u.path_segments().and_then(|mut s| s.next_back().map(|s| s.to_string()))
            })
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "PDF".into());
    let src_param = match q.get("id") {
        Some(id) => format!("id={}", enc(id)),
        None => format!("u={}", enc(q.get("u").map(String::as_str).unwrap_or(""))),
    };

    let mut body = String::new();
    for (i, page) in pages.iter().enumerate() {
        let (w, h) = page.render_dimensions();
        // CSS size: 1 PDF point = 1/72 in, 1 CSS px = 1/96 in.
        let (cw, ch) = ((w * 96.0 / 72.0).round(), (h * 96.0 / 72.0).round());
        body.push_str(&format!(
            "<figure><img src=\"pdf:page?n={i}&amp;{src_param}\" width=\"{cw}\" height=\"{ch}\" \
             style=\"aspect-ratio:{cw}/{ch}\" alt=\"page {n}\"><figcaption>{n} / {total}</figcaption></figure>\n",
            n = i + 1,
            total = pages.len(),
        ));
    }
    Ok(format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title><style>\
         html{{background:#525659}}body{{margin:0;padding:16px 0;font:13px system-ui;color:#ddd}}\
         figure{{margin:0 auto 16px;max-width:96vw;width:fit-content}}\
         img{{display:block;max-width:96vw;height:auto;background:#fff;box-shadow:0 2px 8px #0008}}\
         figcaption{{text-align:center;padding-top:4px}}</style>\n{body}",
        t = html_escape(&title)
    ))
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn render_page(src: &Source, n: usize) -> Result<Vec<u8>, String> {
    let pdf = open(src.bytes()?)?;
    let pages = pdf.pages();
    let page = pages.get(n).ok_or("no such page")?;
    let cache = RenderCache::new();
    let pixmap = hayro::render(
        page,
        &cache,
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings { x_scale: RENDER_SCALE, y_scale: RENDER_SCALE, bg_color: WHITE },
    );
    pixmap.into_png().map_err(|e| format!("png encode failed: {e:?}"))
}

// ---------------------------------------------------------------------------
// Protocol handler
// ---------------------------------------------------------------------------

pub struct PdfProtocol;

fn response(url: ServoUrl, timing: ResourceFetchTiming, ctype: &'static str, body: Vec<u8>) -> Response {
    let mut r = Response::new(url, timing);
    *r.body.lock() = ResponseBody::Done(body);
    r.headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    r.status = HttpStatus::default();
    r
}

fn error_page(msg: &str) -> Vec<u8> {
    format!(
        "<!doctype html><meta charset=utf-8><title>PDF error</title>\
         <body style=\"font:16px system-ui;padding:24px\"><h3>Could not open PDF</h3><p>{}</p>",
        html_escape(msg)
    )
    .into_bytes()
}

impl ProtocolHandler for PdfProtocol {
    fn load<'a>(
        &'a self,
        request: &'a mut Request,
        _done_chan: &mut DoneChannel,
        _context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
        let url = request.current_url();
        let timing = ResourceFetchTiming::new(request.timing_type());
        let q: HashMap<String, String> = url.as_url().query_pairs().into_owned().collect();
        let path = url.as_url().path().to_string();

        let Some(src) = Source::from_query(&q) else {
            return Box::pin(std::future::ready(Response::network_error(
                NetworkError::ResourceLoadError("bad pdf: URL".into()),
            )));
        };

        let (tx, rx) = oneshot::channel::<Response>();
        match path.as_str() {
            "view" => spawn(Box::new(move || {
                let body = viewer_html(&src, &q).unwrap_or_else(|e| String::from_utf8(error_page(&e)).unwrap());
                let _ = tx.send(response(url, timing, "text/html; charset=utf-8", body.into_bytes()));
            })),
            "page" => {
                let n: usize = q.get("n").and_then(|n| n.parse().ok()).unwrap_or(0);
                spawn(Box::new(move || {
                    let r = match render_page(&src, n) {
                        Ok(png) => response(url, timing, "image/png", png),
                        Err(e) => Response::network_error(NetworkError::ResourceLoadError(e)),
                    };
                    let _ = tx.send(r);
                }))
            }
            _ => {
                return Box::pin(std::future::ready(Response::network_error(
                    NetworkError::ResourceLoadError("unknown pdf: path".into()),
                )));
            }
        }
        Box::pin(async move {
            rx.await.unwrap_or_else(|_| {
                Response::network_error(NetworkError::ResourceLoadError("pdf worker failed".into()))
            })
        })
    }
}
