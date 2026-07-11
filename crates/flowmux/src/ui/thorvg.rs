// SPDX-License-Identifier: GPL-3.0-or-later

//! Runtime binding to the system ThorVG library for the image viewer.
//!
//! ThorVG is an **optional runtime dependency**: flowmux builds and runs
//! without it. The library is loaded lazily with `dlopen` the first time the
//! image viewer is used. If it is not installed, [`available`] returns `false`
//! and the viewer shows a "ThorVG is not installed" message instead of an
//! image.
//!
//! Only the subset of the ThorVG C API used by
//! [`crate::ui::image_viewer`] is bound here. The type and function names
//! mirror the C API (and the `thorvg-sys` crate this replaced) so call sites
//! read the same.

#![allow(non_camel_case_types)]
// Enums mirror the ThorVG C API in full; not every variant is constructed here.
#![allow(dead_code)]

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

use libloading::{Library, Symbol};

pub type Tvg_Canvas = *mut c_void;
pub type Tvg_Paint = *mut c_void;
pub type Tvg_Animation = *mut c_void;

// C `Tvg_Result` is a plain enum (int); the discriminants match thorvg_capi.h.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tvg_Result {
    TVG_RESULT_SUCCESS = 0,
    TVG_RESULT_INVALID_ARGUMENT = 1,
    TVG_RESULT_INSUFFICIENT_CONDITION = 2,
    TVG_RESULT_FAILED_ALLOCATION = 3,
    TVG_RESULT_MEMORY_CORRUPTION = 4,
    TVG_RESULT_NOT_SUPPORTED = 5,
    TVG_RESULT_UNKNOWN = 255,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tvg_Colorspace {
    TVG_COLORSPACE_ABGR8888 = 0,
    TVG_COLORSPACE_ARGB8888 = 1,
    TVG_COLORSPACE_ABGR8888S = 2,
    TVG_COLORSPACE_ARGB8888S = 3,
    TVG_COLORSPACE_UNKNOWN = 255,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tvg_Engine_Option {
    TVG_ENGINE_OPTION_NONE = 0,
    TVG_ENGINE_OPTION_DEFAULT = 1,
    TVG_ENGINE_OPTION_SMART_RENDER = 2,
    TVG_ENGINE_OPTION_ALIASED = 4,
}

/// Resolved function pointers into the loaded ThorVG library. The `Library`
/// is kept alive for the process lifetime so the pointers stay valid.
struct Api {
    _lib: Library,
    tvg_engine_init: unsafe extern "C" fn(u32) -> Tvg_Result,
    tvg_engine_term: unsafe extern "C" fn() -> Tvg_Result,
    tvg_swcanvas_create: unsafe extern "C" fn(Tvg_Engine_Option) -> Tvg_Canvas,
    tvg_swcanvas_set_target:
        unsafe extern "C" fn(Tvg_Canvas, *mut u32, u32, u32, u32, Tvg_Colorspace) -> Tvg_Result,
    tvg_canvas_destroy: unsafe extern "C" fn(Tvg_Canvas) -> Tvg_Result,
    tvg_canvas_add: unsafe extern "C" fn(Tvg_Canvas, Tvg_Paint) -> Tvg_Result,
    tvg_canvas_update: unsafe extern "C" fn(Tvg_Canvas) -> Tvg_Result,
    tvg_canvas_draw: unsafe extern "C" fn(Tvg_Canvas, bool) -> Tvg_Result,
    tvg_canvas_sync: unsafe extern "C" fn(Tvg_Canvas) -> Tvg_Result,
    tvg_picture_new: unsafe extern "C" fn() -> Tvg_Paint,
    tvg_picture_load: unsafe extern "C" fn(Tvg_Paint, *const c_char) -> Tvg_Result,
    tvg_picture_load_data: unsafe extern "C" fn(
        Tvg_Paint,
        *const c_char,
        u32,
        *const c_char,
        *const c_char,
        bool,
    ) -> Tvg_Result,
    tvg_picture_load_raw:
        unsafe extern "C" fn(Tvg_Paint, *const u32, u32, u32, Tvg_Colorspace, bool) -> Tvg_Result,
    tvg_picture_set_size: unsafe extern "C" fn(Tvg_Paint, f32, f32) -> Tvg_Result,
    tvg_picture_get_size: unsafe extern "C" fn(Tvg_Paint, *mut f32, *mut f32) -> Tvg_Result,
    tvg_paint_ref: unsafe extern "C" fn(Tvg_Paint) -> u16,
    tvg_paint_unref: unsafe extern "C" fn(Tvg_Paint, bool) -> u16,
    tvg_paint_rel: unsafe extern "C" fn(Tvg_Paint) -> Tvg_Result,
    tvg_animation_new: unsafe extern "C" fn() -> Tvg_Animation,
    tvg_animation_del: unsafe extern "C" fn(Tvg_Animation) -> Tvg_Result,
    tvg_animation_set_frame: unsafe extern "C" fn(Tvg_Animation, f32) -> Tvg_Result,
    tvg_animation_get_picture: unsafe extern "C" fn(Tvg_Animation) -> Tvg_Paint,
    tvg_animation_get_total_frame: unsafe extern "C" fn(Tvg_Animation, *mut f32) -> Tvg_Result,
    tvg_animation_get_duration: unsafe extern "C" fn(Tvg_Animation, *mut f32) -> Tvg_Result,
}

static API: OnceLock<Option<Api>> = OnceLock::new();

#[cfg(target_os = "macos")]
const LIBRARY_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/thorvg/lib/libthorvg-1.dylib",
    "/usr/local/opt/thorvg/lib/libthorvg-1.dylib",
    "/opt/homebrew/lib/libthorvg-1.dylib",
    "/usr/local/lib/libthorvg-1.dylib",
    "libthorvg-1.dylib",
    "libthorvg.dylib",
];

#[cfg(not(target_os = "macos"))]
const LIBRARY_CANDIDATES: &[&str] = &["libthorvg-1.so.1", "libthorvg-1.so", "libthorvg.so"];

fn load() -> Option<Api> {
    let lib = LIBRARY_CANDIDATES
        .iter()
        .find_map(|candidate| unsafe { Library::new(candidate).ok() })?;

    // Resolve a symbol into a typed function pointer, returning None from
    // `load` if any is missing (treated as "ThorVG unavailable").
    macro_rules! sym {
        ($name:literal) => {{
            let s: Symbol<_> = unsafe { lib.get($name).ok()? };
            *s
        }};
    }

    let api = Api {
        tvg_engine_init: sym!(b"tvg_engine_init\0"),
        tvg_engine_term: sym!(b"tvg_engine_term\0"),
        tvg_swcanvas_create: sym!(b"tvg_swcanvas_create\0"),
        tvg_swcanvas_set_target: sym!(b"tvg_swcanvas_set_target\0"),
        tvg_canvas_destroy: sym!(b"tvg_canvas_destroy\0"),
        tvg_canvas_add: sym!(b"tvg_canvas_add\0"),
        tvg_canvas_update: sym!(b"tvg_canvas_update\0"),
        tvg_canvas_draw: sym!(b"tvg_canvas_draw\0"),
        tvg_canvas_sync: sym!(b"tvg_canvas_sync\0"),
        tvg_picture_new: sym!(b"tvg_picture_new\0"),
        tvg_picture_load: sym!(b"tvg_picture_load\0"),
        tvg_picture_load_data: sym!(b"tvg_picture_load_data\0"),
        tvg_picture_load_raw: sym!(b"tvg_picture_load_raw\0"),
        tvg_picture_set_size: sym!(b"tvg_picture_set_size\0"),
        tvg_picture_get_size: sym!(b"tvg_picture_get_size\0"),
        tvg_paint_ref: sym!(b"tvg_paint_ref\0"),
        tvg_paint_unref: sym!(b"tvg_paint_unref\0"),
        tvg_paint_rel: sym!(b"tvg_paint_rel\0"),
        tvg_animation_new: sym!(b"tvg_animation_new\0"),
        tvg_animation_del: sym!(b"tvg_animation_del\0"),
        tvg_animation_set_frame: sym!(b"tvg_animation_set_frame\0"),
        tvg_animation_get_picture: sym!(b"tvg_animation_get_picture\0"),
        tvg_animation_get_total_frame: sym!(b"tvg_animation_get_total_frame\0"),
        tvg_animation_get_duration: sym!(b"tvg_animation_get_duration\0"),
        _lib: lib,
    };
    Some(api)
}

fn api() -> Option<&'static Api> {
    API.get_or_init(load).as_ref()
}

/// Whether the system ThorVG library could be loaded. The image viewer calls
/// this before touching any other function in this module.
pub fn available() -> bool {
    api().is_some()
}

// Thin wrappers so call sites read like direct C calls. Each is only reached
// after the image viewer has confirmed `available()`, so `expect` cannot fire
// in practice.
macro_rules! forward {
    ($(fn $name:ident ( $($arg:ident : $ty:ty),* ) $(-> $ret:ty)? ;)*) => {
        $(
            /// # Safety
            /// Same contract as the underlying ThorVG C function.
            pub unsafe fn $name($($arg : $ty),*) $(-> $ret)? {
                let api = api().expect("ThorVG called while unavailable");
                unsafe { (api.$name)($($arg),*) }
            }
        )*
    };
}

forward! {
    fn tvg_engine_init(threads: u32) -> Tvg_Result;
    fn tvg_engine_term() -> Tvg_Result;
    fn tvg_swcanvas_create(op: Tvg_Engine_Option) -> Tvg_Canvas;
    fn tvg_swcanvas_set_target(canvas: Tvg_Canvas, buffer: *mut u32, stride: u32, w: u32, h: u32, cs: Tvg_Colorspace) -> Tvg_Result;
    fn tvg_canvas_destroy(canvas: Tvg_Canvas) -> Tvg_Result;
    fn tvg_canvas_add(canvas: Tvg_Canvas, paint: Tvg_Paint) -> Tvg_Result;
    fn tvg_canvas_update(canvas: Tvg_Canvas) -> Tvg_Result;
    fn tvg_canvas_draw(canvas: Tvg_Canvas, clear: bool) -> Tvg_Result;
    fn tvg_canvas_sync(canvas: Tvg_Canvas) -> Tvg_Result;
    fn tvg_picture_new() -> Tvg_Paint;
    fn tvg_picture_load(picture: Tvg_Paint, path: *const c_char) -> Tvg_Result;
    fn tvg_picture_load_data(picture: Tvg_Paint, data: *const c_char, size: u32, mimetype: *const c_char, rpath: *const c_char, copy: bool) -> Tvg_Result;
    fn tvg_picture_load_raw(picture: Tvg_Paint, data: *const u32, w: u32, h: u32, cs: Tvg_Colorspace, copy: bool) -> Tvg_Result;
    fn tvg_picture_set_size(picture: Tvg_Paint, w: f32, h: f32) -> Tvg_Result;
    fn tvg_picture_get_size(picture: Tvg_Paint, w: *mut f32, h: *mut f32) -> Tvg_Result;
    fn tvg_paint_ref(paint: Tvg_Paint) -> u16;
    fn tvg_paint_unref(paint: Tvg_Paint, free: bool) -> u16;
    fn tvg_paint_rel(paint: Tvg_Paint) -> Tvg_Result;
    fn tvg_animation_new() -> Tvg_Animation;
    fn tvg_animation_del(animation: Tvg_Animation) -> Tvg_Result;
    fn tvg_animation_set_frame(animation: Tvg_Animation, no: f32) -> Tvg_Result;
    fn tvg_animation_get_picture(animation: Tvg_Animation) -> Tvg_Paint;
    fn tvg_animation_get_total_frame(animation: Tvg_Animation, cnt: *mut f32) -> Tvg_Result;
    fn tvg_animation_get_duration(animation: Tvg_Animation, duration: *mut f32) -> Tvg_Result;
}
