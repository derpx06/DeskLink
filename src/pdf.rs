//! PDF pages as pictures, through poppler-glib loaded at run time.
//!
//! Nearly every desktop has `libpoppler-glib.so.8` (GNOME's own previewers
//! use it), but DeskLink must not need it to start: it is opened with
//! `dlopen` on first use, and without it [`available`] is false and PDFs
//! keep their icon and an "Open in Default App" button. Pages are drawn by
//! poppler into a cairo image surface (cairo comes with GTK).
//!
//! A [`Document`] is used on the thread that opened it: poppler documents
//! are not to be shared between threads.

use std::ffi::{c_char, c_double, c_int, c_void, CString};
use std::path::Path;
use std::sync::OnceLock;

use gtk::cairo;
use gtk::glib;

type NewFromFile =
    unsafe extern "C" fn(*const c_char, *const c_char, *mut *mut glib::ffi::GError) -> *mut c_void;
type NPages = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetPage = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;
type PageSize = unsafe extern "C" fn(*mut c_void, *mut c_double, *mut c_double);
type Render = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// The poppler-glib functions DeskLink uses.
struct Poppler {
    new_from_file: NewFromFile,
    n_pages: NPages,
    get_page: GetPage,
    page_size: PageSize,
    render: Render,
}

/// poppler-glib, opened once; `None` when this desktop does not have it.
fn poppler() -> Option<&'static Poppler> {
    static POPPLER: OnceLock<Option<Poppler>> = OnceLock::new();
    POPPLER
        .get_or_init(|| {
            // SAFETY: dlopen/dlsym with NUL-terminated names; each symbol is
            // cast to the signature poppler-glib's headers give it.
            unsafe {
                let handle = ["libpoppler-glib.so.8", "libpoppler-glib.so"]
                    .iter()
                    .find_map(|name| {
                        let name = CString::new(*name).ok()?;
                        let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                        (!handle.is_null()).then_some(handle)
                    })?;
                let symbol = |name: &str| {
                    let name = CString::new(name).ok()?;
                    let symbol = libc::dlsym(handle, name.as_ptr());
                    (!symbol.is_null()).then_some(symbol)
                };
                Some(Poppler {
                    new_from_file: std::mem::transmute::<*mut c_void, NewFromFile>(symbol(
                        "poppler_document_new_from_file",
                    )?),
                    n_pages: std::mem::transmute::<*mut c_void, NPages>(symbol(
                        "poppler_document_get_n_pages",
                    )?),
                    get_page: std::mem::transmute::<*mut c_void, GetPage>(symbol(
                        "poppler_document_get_page",
                    )?),
                    page_size: std::mem::transmute::<*mut c_void, PageSize>(symbol(
                        "poppler_page_get_size",
                    )?),
                    render: std::mem::transmute::<*mut c_void, Render>(symbol(
                        "poppler_page_render",
                    )?),
                })
            }
        })
        .as_ref()
}

/// Whether PDFs can be drawn here (tests adapt to the desktop they run on).
#[cfg(test)]
pub(crate) fn available() -> bool {
    poppler().is_some()
}

/// A page drawn: tightly described BGRA (premultiplied; opaque, on white)
/// rows, as cairo lays them out on a little-endian machine.
pub(crate) struct Rendered {
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) stride: usize,
    pub(crate) bgra: Vec<u8>,
}

impl Rendered {
    /// The same pixels as RGBA, for a `Pixbuf` (opaque, so premultiplied or
    /// not makes no difference).
    pub(crate) fn rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 4);
        for row in self.bgra.chunks(self.stride).take(self.height as usize) {
            for pixel in row[..self.width as usize * 4].chunks_exact(4) {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
        }
        out
    }
}

/// An open PDF. Not `Send`: it stays on the thread that opened it.
pub(crate) struct Document {
    raw: *mut c_void,
    /// Each page's size in points.
    sizes: Vec<(f64, f64)>,
}

impl Document {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let poppler = poppler().ok_or("PDF previews need poppler-glib")?;
        let uri = glib::filename_to_uri(path, None).map_err(|error| error.to_string())?;
        let uri = CString::new(uri.as_str()).map_err(|error| error.to_string())?;
        let mut error: *mut glib::ffi::GError = std::ptr::null_mut();
        // SAFETY: a valid URI, no password, and an out-pointer for the error.
        let raw = unsafe { (poppler.new_from_file)(uri.as_ptr(), std::ptr::null(), &mut error) };
        if raw.is_null() {
            let message = if error.is_null() {
                "This PDF could not be opened".to_string()
            } else {
                // SAFETY: poppler set a GError that is ours to free.
                unsafe {
                    let message = std::ffi::CStr::from_ptr((*error).message)
                        .to_string_lossy()
                        .into_owned();
                    glib::ffi::g_error_free(error);
                    message
                }
            };
            return Err(message);
        }
        // SAFETY: `raw` is a live PopplerDocument.
        let count = unsafe { (poppler.n_pages)(raw) }.max(0);
        if count > MAX_PDF_PAGES {
            // SAFETY: the document reference taken above, released unused.
            unsafe { glib::gobject_ffi::g_object_unref(raw.cast()) };
            return Err(too_many_pages(count));
        }
        let mut sizes = Vec::with_capacity(count as usize);
        for index in 0..count {
            // SAFETY: an index within the document; the page is a new
            // reference, released here.
            unsafe {
                let page = (poppler.get_page)(raw, index);
                let (mut width, mut height) = (612.0, 792.0);
                if !page.is_null() {
                    (poppler.page_size)(page, &mut width, &mut height);
                    glib::gobject_ffi::g_object_unref(page.cast());
                }
                sizes.push((width.max(1.0), height.max(1.0)));
            }
        }
        Ok(Self { raw, sizes })
    }

    /// Each page's size in points.
    pub(crate) fn sizes(&self) -> &[(f64, f64)] {
        &self.sizes
    }

    /// Page `index` drawn `width` pixels wide, on white.
    pub(crate) fn render(&self, index: usize, width: i32) -> Result<Rendered, String> {
        let poppler = poppler().ok_or("PDF previews need poppler-glib")?;
        let &(points_w, points_h) = self.sizes.get(index).ok_or("No such page")?;
        let (width, height, scale) = page_pixels(points_w, points_h, width);
        let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height)
            .map_err(|error| error.to_string())?;
        {
            let context = cairo::Context::new(&surface).map_err(|error| error.to_string())?;
            context.set_source_rgb(1.0, 1.0, 1.0);
            context.paint().map_err(|error| error.to_string())?;
            context.scale(scale, scale);
            // SAFETY: a live document and page (released after), and a
            // live cairo context for poppler to draw on.
            unsafe {
                let page = (poppler.get_page)(self.raw, index as c_int);
                if page.is_null() {
                    return Err("No such page".to_string());
                }
                (poppler.render)(page, context.to_raw_none().cast());
                glib::gobject_ffi::g_object_unref(page.cast());
            }
        }
        surface.flush();
        let stride = surface.stride() as usize;
        let bgra = surface.data().map_err(|error| error.to_string())?.to_vec();
        Ok(Rendered {
            width,
            height,
            stride,
            bgra,
        })
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        // SAFETY: the document reference this side holds.
        unsafe { glib::gobject_ffi::g_object_unref(self.raw.cast()) };
    }
}

/// The freedesktop thumbnail of `path` in `cache` (`$XDG_CACHE_HOME/
/// thumbnails`), `size` "normal" (128 px) or "large" (256 px): named by the
/// MD5 of the file's URI, as Files and every other previewer name them.
pub(crate) fn freedesktop_thumbnail(
    cache: &Path,
    size: &str,
    path: &Path,
) -> Option<std::path::PathBuf> {
    let uri = glib::filename_to_uri(path, None).ok()?;
    let digest = openssl::hash::hash(openssl::hash::MessageDigest::md5(), uri.as_bytes()).ok()?;
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(cache.join(size).join(format!("{name}.png")))
}

/// The most pixels one rendered page may have (a 64 MiB surface; a copy of it
/// is made to hand to the window): a page with an absurd aspect ratio, or at
/// the highest zoom, is drawn smaller rather than allocating more.
const MAX_PAGE_PIXELS: f64 = 16_000_000.0;

/// The most pages one PDF is previewed with: every page is a widget and a
/// layout entry. A longer one is opened in the default app instead.
pub(crate) const MAX_PDF_PAGES: i32 = 500;

/// A page of `points_w` × `points_h` points drawn about `width` px wide:
/// its pixel size and scale, within the side and pixel bounds.
fn page_pixels(points_w: f64, points_h: f64, width: i32) -> (i32, i32, f64) {
    let points_w = points_w.max(1.0);
    let points_h = points_h.max(1.0);
    let mut scale = f64::from(width.clamp(16, 8192)) / points_w;
    scale = scale.min(16_384.0 / points_h);
    let pixels = points_w * points_h * scale * scale;
    if pixels > MAX_PAGE_PIXELS {
        scale *= (MAX_PAGE_PIXELS / pixels).sqrt();
    }
    let width = ((points_w * scale).round() as i32).clamp(16, 8192);
    let height = ((points_h * scale).round() as i32).clamp(16, 16_384);
    (width, height, scale)
}

/// What the viewer says for a PDF of `count` pages, past [`MAX_PDF_PAGES`].
pub(crate) fn too_many_pages(count: i32) -> String {
    format!(
        "This PDF has {count} pages, more than DeskLink previews ({MAX_PDF_PAGES}). \
         Open it in your default app."
    )
}

/// A PDF of `pages` empty pages, for tests.
#[cfg(test)]
pub(crate) fn pdf_of_pages(pages: usize) -> Vec<u8> {
    let kids: String = (0..pages).map(|n| format!("{} 0 R ", 3 + n)).collect();
    let mut text = format!(
        "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
         2 0 obj<</Type/Pages/Kids[{kids}]/Count {pages}>>endobj\n"
    );
    for n in 0..pages {
        text.push_str(&format!(
            "{} 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]>>endobj\n",
            3 + n
        ));
    }
    text.push_str("trailer<</Root 1 0 R>>\n%%EOF\n");
    text.into_bytes()
}

/// A one-page, 200×100 pt PDF with nothing on it, for tests.
#[cfg(test)]
pub(crate) const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]>>endobj\n\
trailer<</Root 1 0 R>>\n%%EOF\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_never_drawn_past_its_pixel_bound() {
        let (width, height, _) = page_pixels(612.0, 792.0, 1200);
        assert_eq!(width, 1200, "a normal page at the asked width");
        assert!((height - 1553).abs() <= 1);
        // A ribbon 200 pages tall: drawn smaller, not 8192 × 16384.
        let (width, height, scale) = page_pixels(612.0, 158_400.0, 8192);
        assert!(f64::from(width) * f64::from(height) <= MAX_PAGE_PIXELS + 20_000.0);
        assert!(height <= 16_384 && scale > 0.0);
        let (width, height, _) = page_pixels(0.0, 0.0, 99_999);
        assert!(width >= 16 && height >= 16);
    }

    #[test]
    fn freedesktop_thumbnails_are_named_by_the_md5_of_the_uri() {
        let cache = Path::new("/home/u/.cache/thumbnails");
        // md5("file:///tmp/a%20b.pdf"), as the thumbnail spec names it.
        let expected = {
            let digest = openssl::hash::hash(
                openssl::hash::MessageDigest::md5(),
                b"file:///tmp/a%20b.pdf",
            )
            .unwrap();
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            freedesktop_thumbnail(cache, "large", Path::new("/tmp/a b.pdf")),
            Some(cache.join("large").join(format!("{expected}.png")))
        );
        assert_eq!(expected.len(), 32);
    }

    /// A PDF past the page cap is refused with a plain sentence (and the
    /// viewer's Open in Default App), not turned into thousands of widgets.
    #[test]
    fn a_pdf_with_too_many_pages_is_not_previewed() {
        let dir = std::env::temp_dir().join(format!("desklink-pdf-long-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long = dir.join("long.pdf");
        std::fs::write(&long, pdf_of_pages(MAX_PDF_PAGES as usize + 1)).unwrap();
        let fits = dir.join("fits.pdf");
        std::fs::write(&fits, pdf_of_pages(3)).unwrap();
        match Document::open(&long) {
            Ok(_) => panic!("a {}-page PDF was opened", MAX_PDF_PAGES + 1),
            Err(error) if available() => {
                assert_eq!(error, too_many_pages(MAX_PDF_PAGES + 1));
                assert!(Document::open(&fits).is_ok_and(|document| document.sizes().len() == 3));
            }
            Err(_) => {}
        }
        assert!(too_many_pages(501).contains("501"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whatever this desktop has, opening a PDF either works or says why;
    /// with poppler, a one-page PDF renders opaque at the asked width.
    #[test]
    fn a_pdf_renders_when_poppler_is_here_and_says_why_when_not() {
        let dir = std::env::temp_dir().join(format!("desklink-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.pdf");
        std::fs::write(&path, MINIMAL_PDF).unwrap();
        match Document::open(&path) {
            Ok(document) => {
                assert!(available());
                assert_eq!(document.sizes(), &[(200.0, 100.0)]);
                let page = document.render(0, 400).unwrap();
                assert_eq!((page.width, page.height), (400, 200));
                assert_eq!(page.rgba().len(), 400 * 200 * 4);
                assert!(page.rgba().chunks(4).all(|pixel| pixel[3] == 255));
                assert!(document.render(1, 400).is_err());
            }
            Err(error) => assert!(!available(), "poppler is here but: {error}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
