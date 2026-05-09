//! Hand-written FFI shim against `<mpv/client.h>` and `<mpv/render.h>`.
//!
//! We deliberately avoid `bindgen` here: the surface is small, the
//! libmpv C API is stable across releases (the version macro hasn't
//! moved the goalposts on these symbols since libmpv 1.x), and a hand
//! shim keeps the unsafe boundary auditable. Anything not listed here
//! we don't call.
//!
//! Coverage matches what the safe wrapper in `lib.rs` actually uses.
//! Add new entries as the wrapper grows; do not preemptively bind
//! anything we don't use.
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_double, c_int, c_void};

pub type mpv_handle = c_void;
pub type mpv_render_context = c_void;

// `mpv_format` — only the variants the wrapper consults.
pub const MPV_FORMAT_INT64: c_int = 4;

// `mpv_event_id` — only the variants the wrapper inspects.
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_VIDEO_RECONFIG: c_int = 17;

// `mpv_render_param_type` — only the values we set/read.
pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
pub const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
pub const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
pub const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

/// Render-update bitflags returned from `mpv_render_context_update`.
pub const MPV_RENDER_UPDATE_FRAME: u64 = 1 << 0;

/// `MPV_RENDER_API_TYPE_SW` — the string mpv expects in
/// `MPV_RENDER_PARAM_API_TYPE` to select the software renderer.
pub const MPV_RENDER_API_TYPE_SW: &[u8] = b"sw\0";

#[repr(C)]
pub struct mpv_render_param {
    pub type_: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct mpv_event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

unsafe extern "C" {
    pub fn mpv_create() -> *mut mpv_handle;
    pub fn mpv_initialize(ctx: *mut mpv_handle) -> c_int;
    pub fn mpv_terminate_destroy(ctx: *mut mpv_handle);

    pub fn mpv_set_option_string(ctx: *mut mpv_handle, name: *const c_char, data: *const c_char) -> c_int;
    pub fn mpv_set_property_string(ctx: *mut mpv_handle, name: *const c_char, data: *const c_char) -> c_int;
    pub fn mpv_get_property(ctx: *mut mpv_handle, name: *const c_char, format: c_int, data: *mut c_void) -> c_int;
    pub fn mpv_command(ctx: *mut mpv_handle, args: *mut *const c_char) -> c_int;

    pub fn mpv_wait_event(ctx: *mut mpv_handle, timeout: c_double) -> *mut mpv_event;
    pub fn mpv_error_string(error: c_int) -> *const c_char;

    pub fn mpv_render_context_create(
        res: *mut *mut mpv_render_context,
        mpv: *mut mpv_handle,
        params: *mut mpv_render_param,
    ) -> c_int;
    pub fn mpv_render_context_render(ctx: *mut mpv_render_context, params: *mut mpv_render_param) -> c_int;
    pub fn mpv_render_context_update(ctx: *mut mpv_render_context) -> u64;
    pub fn mpv_render_context_free(ctx: *mut mpv_render_context);
}
