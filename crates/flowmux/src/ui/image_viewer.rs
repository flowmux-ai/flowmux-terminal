// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::RefCell;
use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use zip::ZipArchive;

use super::thorvg as tvg;

const MAX_VIEW_WIDTH: u32 = 1200;
const MAX_VIEW_HEIGHT: u32 = 900;
const DEFAULT_LOTTIE_WIDTH: u32 = 640;
const DEFAULT_LOTTIE_HEIGHT: u32 = 480;

pub fn open_image_viewer(parent: &adw::ApplicationWindow, path: PathBuf) {
    let result = load_viewer_content(&path);
    let title = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Image")
        .to_string();

    let mut builder = adw::ApplicationWindow::builder()
        .title(&title)
        .default_width(800)
        .default_height(600);
    if let Some(app) = parent.application() {
        builder = builder.application(&app);
    }
    let window = builder.build();

    match result {
        Ok(ViewerContent::Static(frame)) => {
            window.set_default_size(frame.width as i32, frame.height as i32);
            let picture = image_picture(&path);
            picture.set_paintable(Some(&texture_from_frame(&frame)));
            set_viewer_content(&window, &picture);
        }
        Ok(ViewerContent::Animated(renderer)) => {
            let initial = renderer.borrow().frame();
            window.set_default_size(initial.width as i32, initial.height as i32);

            let picture = image_picture(&path);
            picture.set_paintable(Some(&texture_from_frame(&initial)));
            set_viewer_content(&window, &picture);

            let interval = renderer.borrow().frame_interval();
            glib::timeout_add_local(interval, {
                let renderer = renderer.clone();
                let picture = picture.clone();
                move || {
                    if picture.root().is_none() {
                        return glib::ControlFlow::Break;
                    }
                    match renderer.borrow_mut().advance() {
                        Ok(frame) => {
                            picture.set_paintable(Some(&texture_from_frame(&frame)));
                            glib::ControlFlow::Continue
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "failed to render Lottie frame");
                            glib::ControlFlow::Break
                        }
                    }
                }
            });
        }
        Err(err) => {
            window.set_default_size(520, 180);
            let label = gtk::Label::new(Some(&format!(
                "Could not open image with ThorVG:\n{}\n\n{}",
                path.display(),
                err
            )));
            label.set_wrap(true);
            label.set_selectable(true);
            label.set_margin_top(18);
            label.set_margin_bottom(18);
            label.set_margin_start(18);
            label.set_margin_end(18);
            set_viewer_content(&window, &label);
        }
    }

    window.present();
}

fn set_viewer_content<W: IsA<gtk::Widget>>(window: &adw::ApplicationWindow, content: &W) {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(content));
    window.set_content(Some(&toolbar));
}

fn image_picture(path: &Path) -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_alternative_text(path.to_str());
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture
}

enum ViewerContent {
    Static(RenderedFrame),
    Animated(Rc<RefCell<ThorvgAnimationRenderer>>),
}

fn load_viewer_content(path: &Path) -> Result<ViewerContent, String> {
    match image_kind(path) {
        // ThorVG decodes png / jpg / webp natively; fall back to the
        // `image` crate only if ThorVG's loader rejects the file.
        ImageKind::NativeRaster => render_native(path)
            .or_else(|_| render_raster(path))
            .map(ViewerContent::Static),
        // ThorVG has no loader for these (e.g. gif); decode with the
        // `image` crate, then hand the pixels to ThorVG to render.
        ImageKind::Raster => render_raster(path).map(ViewerContent::Static),
        ImageKind::Svg => render_native(path).map(ViewerContent::Static),
        ImageKind::Lottie => ThorvgAnimationRenderer::new(path)
            .map(|renderer| ViewerContent::Animated(Rc::new(RefCell::new(renderer)))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    /// Decoded by the `image` crate (formats ThorVG can't load, e.g. gif).
    Raster,
    /// Decoded natively by ThorVG's built-in loaders.
    NativeRaster,
    Svg,
    Lottie,
}

fn image_kind(path: &Path) -> ImageKind {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("svg") => ImageKind::Svg,
        Some("json") | Some("lottie") => ImageKind::Lottie,
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") => ImageKind::NativeRaster,
        _ => ImageKind::Raster,
    }
}

#[derive(Clone)]
struct RenderedFrame {
    width: u32,
    height: u32,
    buffer: Vec<u32>,
}

fn render_raster(path: &Path) -> Result<RenderedFrame, String> {
    let image = image::ImageReader::open(path)
        .map_err(|err| format!("read failed: {err}"))?
        .with_guessed_format()
        .map_err(|err| format!("format detection failed: {err}"))?
        .decode()
        .map_err(|err| format!("decode failed: {err}"))?;
    let rgba = image.to_rgba8();
    let (source_width, source_height) = rgba.dimensions();
    let source_pixels = rgba
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes([px[0], px[1], px[2], px[3]]))
        .collect::<Vec<_>>();
    let (width, height) = fit_size(source_width, source_height);

    let _engine = ThorvgEngine::init()?;
    let mut canvas = ThorvgCanvas::new(width, height)?;
    let picture = unsafe { tvg::tvg_picture_new() };
    if picture.is_null() {
        return Err("failed to allocate ThorVG picture".to_string());
    }

    let load = unsafe {
        tvg::tvg_picture_load_raw(
            picture,
            source_pixels.as_ptr(),
            source_width,
            source_height,
            tvg::Tvg_Colorspace::TVG_COLORSPACE_ABGR8888S,
            true,
        )
    };
    if let Err(err) = check(load, "load raster pixels") {
        unsafe {
            tvg::tvg_paint_rel(picture);
        }
        return Err(err);
    }

    if let Err(err) = check(
        unsafe { tvg::tvg_picture_set_size(picture, width as f32, height as f32) },
        "set raster size",
    ) {
        unsafe {
            tvg::tvg_paint_rel(picture);
        }
        return Err(err);
    }
    if let Err(err) = check(
        unsafe { tvg::tvg_canvas_add(canvas.raw, picture) },
        "add raster",
    ) {
        unsafe {
            tvg::tvg_paint_rel(picture);
        }
        return Err(err);
    }
    canvas.render()
}

/// Load an image through ThorVG's native file loaders (svg, png, jpg, webp).
/// ThorVG picks the loader from the file's extension and content.
fn render_native(path: &Path) -> Result<RenderedFrame, String> {
    let path = path
        .to_str()
        .ok_or_else(|| "image path is not valid UTF-8".to_string())?;
    let path = CString::new(path).map_err(|_| "image path contains NUL byte".to_string())?;
    let _engine = ThorvgEngine::init()?;

    let picture = unsafe { tvg::tvg_picture_new() };
    if picture.is_null() {
        return Err("failed to allocate ThorVG picture".to_string());
    }

    let load = unsafe { tvg::tvg_picture_load(picture, path.as_ptr()) };
    if let Err(err) = check(load, "load image") {
        unsafe {
            tvg::tvg_paint_rel(picture);
        }
        return Err(err);
    }

    let (source_width, source_height) = picture_size(picture)
        .unwrap_or((DEFAULT_LOTTIE_WIDTH as f32, DEFAULT_LOTTIE_HEIGHT as f32));
    let (width, height) = fit_size(
        (source_width.ceil() as u32).max(1),
        (source_height.ceil() as u32).max(1),
    );
    let mut canvas = match ThorvgCanvas::new(width, height) {
        Ok(canvas) => canvas,
        Err(err) => {
            unsafe {
                tvg::tvg_paint_rel(picture);
            }
            return Err(err);
        }
    };

    if let Err(err) = check(
        unsafe { tvg::tvg_picture_set_size(picture, width as f32, height as f32) },
        "set image size",
    ) {
        unsafe {
            tvg::tvg_paint_rel(picture);
        }
        return Err(err);
    }

    if let Err(err) = check(
        unsafe { tvg::tvg_canvas_add(canvas.raw, picture) },
        "add image",
    ) {
        unsafe {
            tvg::tvg_paint_rel(picture);
        }
        return Err(err);
    }

    canvas.render()
}

struct ThorvgAnimationRenderer {
    canvas: ThorvgCanvas,
    animation: tvg::Tvg_Animation,
    current_frame: f32,
    total_frames: f32,
    duration: f32,
    _engine: ThorvgEngine,
}

impl ThorvgAnimationRenderer {
    fn new(path: &Path) -> Result<Self, String> {
        let _engine = ThorvgEngine::init()?;
        let animation = unsafe { tvg::tvg_animation_new() };
        if animation.is_null() {
            return Err("failed to allocate ThorVG animation".to_string());
        }

        let picture = unsafe { tvg::tvg_animation_get_picture(animation) };
        if picture.is_null() {
            unsafe {
                tvg::tvg_animation_del(animation);
            }
            return Err("ThorVG animation has no picture".to_string());
        }

        let data = read_lottie_data(path)?;
        let mimetype = CString::new("lottie+json").expect("static string has no NUL");
        let load = unsafe {
            tvg::tvg_picture_load_data(
                picture,
                data.as_ptr().cast(),
                data.len() as u32,
                mimetype.as_ptr(),
                std::ptr::null(),
                true,
            )
        };
        if let Err(err) = check(load, "load Lottie") {
            unsafe {
                tvg::tvg_animation_del(animation);
            }
            return Err(err);
        }

        let (source_width, source_height) = picture_size(picture)
            .unwrap_or((DEFAULT_LOTTIE_WIDTH as f32, DEFAULT_LOTTIE_HEIGHT as f32));
        let (width, height) = fit_size(source_width as u32, source_height as u32);
        if let Err(err) = check(
            unsafe { tvg::tvg_picture_set_size(picture, width as f32, height as f32) },
            "set Lottie size",
        ) {
            unsafe {
                tvg::tvg_animation_del(animation);
            }
            return Err(err);
        }

        let mut canvas = match ThorvgCanvas::new(width, height) {
            Ok(canvas) => canvas,
            Err(err) => {
                unsafe {
                    tvg::tvg_animation_del(animation);
                }
                return Err(err);
            }
        };
        unsafe {
            tvg::tvg_paint_ref(picture);
        }
        if let Err(err) = check(
            unsafe { tvg::tvg_canvas_add(canvas.raw, picture) },
            "add Lottie",
        ) {
            unsafe {
                tvg::tvg_paint_unref(picture, false);
                tvg::tvg_animation_del(animation);
            }
            return Err(err);
        }

        let total_frames = animation_float(animation, tvg::tvg_animation_get_total_frame)
            .unwrap_or(0.0)
            .max(1.0);
        let duration = animation_float(animation, tvg::tvg_animation_get_duration)
            .unwrap_or(0.0)
            .max(0.0);

        check_frame_set(
            unsafe { tvg::tvg_animation_set_frame(animation, 0.0) },
            "set initial Lottie frame",
        )?;
        canvas.render()?;

        Ok(Self {
            canvas,
            animation,
            current_frame: 0.0,
            total_frames,
            duration,
            _engine,
        })
    }

    fn frame(&self) -> RenderedFrame {
        self.canvas.frame()
    }

    fn frame_interval(&self) -> Duration {
        if self.duration > 0.0 && self.total_frames > 1.0 {
            let millis = (self.duration * 1000.0 / self.total_frames).round();
            Duration::from_millis(millis.clamp(10.0, 100.0) as u64)
        } else {
            Duration::from_millis(16)
        }
    }

    fn advance(&mut self) -> Result<RenderedFrame, String> {
        self.current_frame += 1.0;
        if self.current_frame >= self.total_frames {
            self.current_frame = 0.0;
        }
        let set = unsafe { tvg::tvg_animation_set_frame(self.animation, self.current_frame) };
        check_frame_set(set, "set Lottie frame")?;
        self.canvas.render()
    }
}

impl Drop for ThorvgAnimationRenderer {
    fn drop(&mut self) {
        self.canvas.destroy();
        unsafe {
            tvg::tvg_animation_del(self.animation);
        }
    }
}

struct ThorvgCanvas {
    raw: tvg::Tvg_Canvas,
    width: u32,
    height: u32,
    buffer: Vec<u32>,
}

impl ThorvgCanvas {
    fn new(width: u32, height: u32) -> Result<Self, String> {
        let raw =
            unsafe { tvg::tvg_swcanvas_create(tvg::Tvg_Engine_Option::TVG_ENGINE_OPTION_NONE) };
        if raw.is_null() {
            return Err("failed to allocate ThorVG canvas".to_string());
        }

        let mut canvas = Self {
            raw,
            width,
            height,
            buffer: vec![0; (width * height) as usize],
        };
        check(
            unsafe {
                tvg::tvg_swcanvas_set_target(
                    canvas.raw,
                    canvas.buffer.as_mut_ptr(),
                    width,
                    width,
                    height,
                    tvg::Tvg_Colorspace::TVG_COLORSPACE_ABGR8888,
                )
            },
            "set canvas target",
        )?;
        Ok(canvas)
    }

    fn render(&mut self) -> Result<RenderedFrame, String> {
        check(unsafe { tvg::tvg_canvas_update(self.raw) }, "update canvas")?;
        check(
            unsafe { tvg::tvg_canvas_draw(self.raw, true) },
            "draw canvas",
        )?;
        check(unsafe { tvg::tvg_canvas_sync(self.raw) }, "sync canvas")?;
        Ok(self.frame())
    }

    fn frame(&self) -> RenderedFrame {
        RenderedFrame {
            width: self.width,
            height: self.height,
            buffer: self.buffer.clone(),
        }
    }
}

impl Drop for ThorvgCanvas {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl ThorvgCanvas {
    fn destroy(&mut self) {
        if self.raw.is_null() {
            return;
        }
        unsafe {
            tvg::tvg_canvas_destroy(self.raw);
        }
        self.raw = std::ptr::null_mut();
    }
}

struct ThorvgEngine;

// The ThorVG engine is a process-global, reference-counted resource. Serialize
// init/term behind a mutex and only call the C init/term at the 0<->1 edges, so
// concurrent callers (e.g. parallel tests) don't tear the engine down while
// another is mid-render. The lock is held only across the count change, never
// during rendering, so an active animation holding a `ThorvgEngine` can't
// deadlock a second image open on the same thread.
static ENGINE_REFCOUNT: Mutex<u32> = Mutex::new(0);

impl ThorvgEngine {
    fn init() -> Result<Self, String> {
        if !tvg::available() {
            return Err(
                "ThorVG is not installed.\n\nThe image viewer needs the ThorVG \
                 library. Install it (see the project README, e.g. \
                 scripts/install-thorvg.sh) and try again."
                    .to_string(),
            );
        }
        let mut count = ENGINE_REFCOUNT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *count == 0 {
            check(unsafe { tvg::tvg_engine_init(0) }, "initialize ThorVG")?;
        }
        *count += 1;
        Ok(Self)
    }
}

impl Drop for ThorvgEngine {
    fn drop(&mut self) {
        let mut count = ENGINE_REFCOUNT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count -= 1;
        if *count == 0 {
            unsafe {
                tvg::tvg_engine_term();
            }
        }
    }
}

fn read_lottie_data(path: &Path) -> Result<Vec<u8>, String> {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lottie"))
    {
        read_dotlottie_json(path)
    } else {
        std::fs::read(path).map_err(|err| format!("read failed: {err}"))
    }
}

fn read_dotlottie_json(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|err| format!("open .lottie failed: {err}"))?;
    let mut archive =
        ZipArchive::new(file).map_err(|err| format!("read .lottie archive failed: {err}"))?;

    let mut fallback = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("read .lottie entry failed: {err}"))?;
        let name = entry.name().to_string();
        if !name.ends_with(".json") || name.ends_with("manifest.json") {
            continue;
        }

        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|err| format!("read .lottie JSON failed: {err}"))?;
        if name.starts_with("animations/") {
            return Ok(data);
        }
        fallback = Some(data);
    }

    fallback.ok_or_else(|| "no Lottie JSON found in .lottie archive".to_string())
}

fn picture_size(picture: tvg::Tvg_Paint) -> Option<(f32, f32)> {
    let mut width = 0.0f32;
    let mut height = 0.0f32;
    let result = unsafe { tvg::tvg_picture_get_size(picture, &mut width, &mut height) };
    if result == tvg::Tvg_Result::TVG_RESULT_SUCCESS && width > 0.0 && height > 0.0 {
        Some((width, height))
    } else {
        None
    }
}

fn animation_float(
    animation: tvg::Tvg_Animation,
    getter: unsafe fn(tvg::Tvg_Animation, *mut f32) -> tvg::Tvg_Result,
) -> Option<f32> {
    let mut value = 0.0f32;
    let result = unsafe { getter(animation, &mut value) };
    if result == tvg::Tvg_Result::TVG_RESULT_SUCCESS {
        Some(value)
    } else {
        None
    }
}

fn texture_from_frame(frame: &RenderedFrame) -> gdk::MemoryTexture {
    let mut bytes = Vec::with_capacity(frame.buffer.len() * 4);
    for pixel in &frame.buffer {
        bytes.extend_from_slice(&pixel.to_le_bytes());
    }
    let bytes = glib::Bytes::from_owned(bytes);
    gdk::MemoryTexture::new(
        frame.width as i32,
        frame.height as i32,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes,
        (frame.width * 4) as usize,
    )
}

fn fit_size(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (DEFAULT_LOTTIE_WIDTH, DEFAULT_LOTTIE_HEIGHT);
    }
    let scale = (MAX_VIEW_WIDTH as f32 / width as f32)
        .min(MAX_VIEW_HEIGHT as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    )
}

fn check(result: tvg::Tvg_Result, context: &str) -> Result<(), String> {
    if result == tvg::Tvg_Result::TVG_RESULT_SUCCESS {
        Ok(())
    } else {
        Err(format!("{context}: {result:?}"))
    }
}

fn check_frame_set(result: tvg::Tvg_Result, context: &str) -> Result<(), String> {
    if result == tvg::Tvg_Result::TVG_RESULT_SUCCESS
        || result == tvg::Tvg_Result::TVG_RESULT_INSUFFICIENT_CONDITION
    {
        Ok(())
    } else {
        Err(format!("{context}: {result:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn fit_size_caps_large_images_without_upscaling() {
        assert_eq!(fit_size(2400, 1800), (1200, 900));
        assert_eq!(fit_size(320, 200), (320, 200));
        assert_eq!(
            fit_size(0, 0),
            (DEFAULT_LOTTIE_WIDTH, DEFAULT_LOTTIE_HEIGHT)
        );
    }

    #[test]
    fn render_raster_draws_png_through_thorvg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.png");
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        image.save(&path).expect("write png");

        let frame = render_raster(&path).expect("render png");

        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.buffer.len(), 2);
        assert!(frame.buffer.iter().any(|pixel| (pixel >> 24) == 0xff));
    }

    #[test]
    fn render_raster_guesses_format_for_web_extension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.web");
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("write png data");

        let frame = render_raster(&path).expect("render png data");

        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.buffer.len(), 2);
        assert!(frame.buffer.iter().any(|pixel| (pixel >> 24) == 0xff));
    }

    #[test]
    fn render_native_draws_vector_through_thorvg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vector.svg");
        std::fs::write(
        &path,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="2"><rect width="4" height="2" fill="#ff0000"/></svg>"##,
    )
    .expect("write svg");

        let frame = render_native(&path).expect("render svg");

        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(frame.buffer.len(), 8);
        assert!(frame.buffer.iter().any(|pixel| (pixel >> 24) == 0xff));
    }

    #[test]
    fn render_native_decodes_png_through_thorvg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.png");
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        image.save(&path).expect("write png");

        // ThorVG's built-in png loader decodes the file directly.
        let frame = render_native(&path).expect("render png natively");

        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.buffer.len(), 2);
        assert!(frame.buffer.iter().any(|pixel| (pixel >> 24) == 0xff));
    }

    #[test]
    fn render_native_decodes_jpeg_through_thorvg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.jpg");
        // JPEG has no alpha channel, so encode from RGB.
        let image = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        image
            .save_with_format(&path, image::ImageFormat::Jpeg)
            .expect("write jpeg");

        // ThorVG's built-in jpg loader decodes the file directly.
        let frame = render_native(&path).expect("render jpeg natively");

        assert_eq!((frame.width, frame.height), (4, 4));
        assert_eq!(frame.buffer.len(), 16);
        assert!(frame.buffer.iter().any(|pixel| (pixel >> 24) == 0xff));
    }

    #[test]
    fn render_native_decodes_webp_through_thorvg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.webp");
        let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        image
            .save_with_format(&path, image::ImageFormat::WebP)
            .expect("write webp");

        // ThorVG's built-in webp loader decodes the file directly.
        let frame = render_native(&path).expect("render webp natively");

        assert_eq!((frame.width, frame.height), (4, 4));
        assert_eq!(frame.buffer.len(), 16);
        assert!(frame.buffer.iter().any(|pixel| (pixel >> 24) == 0xff));
    }

    #[test]
    fn lottie_json_renderer_advances_frames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.json");
        std::fs::write(
            &path,
            r#"{"v":"5.7.4","fr":30,"ip":0,"op":30,"w":10,"h":10,"nm":"empty","ddd":0,"assets":[],"layers":[]}"#,
        )
        .expect("write lottie");

        let mut renderer = ThorvgAnimationRenderer::new(&path).expect("render lottie");
        let frame = renderer.advance().expect("advance lottie");

        assert_eq!((frame.width, frame.height), (10, 10));
        assert_eq!(frame.buffer.len(), 100);
    }

    #[test]
    fn dotlottie_reader_extracts_animation_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.lottie");
        let file = File::create(&path).expect("create lottie");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("manifest.json", options)
            .expect("manifest entry");
        zip.write_all(br#"{"animations":[{"id":"a"}]}"#)
            .expect("manifest");
        zip.start_file("animations/a.json", options)
            .expect("animation entry");
        zip.write_all(br#"{"v":"5.7.4","fr":30}"#)
            .expect("animation");
        zip.finish().expect("finish zip");

        let data = read_dotlottie_json(&path).expect("read dotlottie");

        assert_eq!(data, br#"{"v":"5.7.4","fr":30}"#);
    }
}
