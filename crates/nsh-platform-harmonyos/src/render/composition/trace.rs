//! Native trace scopes share the system's graphics timeline and clock.
use std::ffi::{CStr, c_char};

#[link(name = "hitrace_ndk.z")]
unsafe extern "C" {
    fn OH_HiTrace_StartTrace(name: *const c_char);
    fn OH_HiTrace_FinishTrace();
}

pub(crate) struct Scope;
impl Scope {
    pub(crate) fn new(name: &'static CStr) -> Self {
        // SAFETY: the static name is NUL-terminated and stays alive throughout.
        unsafe { OH_HiTrace_StartTrace(name.as_ptr()) };
        Self
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        // SAFETY: each scope finishes on the same thread that began it.
        unsafe { OH_HiTrace_FinishTrace() };
    }
}
