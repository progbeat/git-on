//! Native FFI bindings to libfuse2.
//!
//! This is a small set of bindings that are required to mount/unmount FUSE filesystems and
//! open/close a fd to the FUSE kernel driver.

#![warn(missing_debug_implementations)]
#![allow(missing_docs)]

use libc::{c_char, c_int};

#[repr(C)]
#[derive(Debug)]
pub struct fuse_args {
    pub argc: c_int,
    pub argv: *const *const c_char,
    pub allocated: c_int,
}

#[cfg(all(fuser_mount_impl = "libfuse2", not(target_os = "macos")))]
unsafe extern "C" {
    // *_compat25 functions were introduced in FUSE 2.6 when function signatures changed.
    // Therefore, the minimum version requirement for *_compat25 functions is libfuse-2.6.0.

    pub fn fuse_mount_compat25(mountpoint: *const c_char, args: *const fuse_args) -> c_int;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    pub fn fuse_unmount_compat22(mountpoint: *const c_char);
}

// The macFUSE ABI is the same compat25 interface used by upstream's libfuse2
// backend. Resolve only the documented system installation, never a search path.
#[cfg(target_os = "macos")]
pub unsafe fn fuse_mount_compat25(mountpoint: *const c_char, args: *const fuse_args) -> c_int {
    use std::sync::OnceLock;
    static ENTRY: OnceLock<Option<usize>> = OnceLock::new();
    let entry = ENTRY.get_or_init(|| {
        let library = unsafe { libc::dlopen(c"/usr/local/lib/libfuse.2.dylib".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() { return None; }
        let entry = unsafe { libc::dlsym(library, c"fuse_mount_compat25".as_ptr()) };
        if entry.is_null() {
            unsafe { libc::dlclose(library) };
            None
        } else {
            // Keep the library loaded until process exit: its mount helpers may
            // retain internal state beyond this call.
            Some(entry as usize)
        }
    });
    match entry {
        Some(entry) => {
            let mount: unsafe extern "C" fn(*const c_char, *const fuse_args) -> c_int = unsafe { std::mem::transmute(*entry) };
            unsafe { mount(mountpoint, args) }
        }
        None => {
            unsafe { *libc::__error() = libc::ENODEV };
            -1
        }
    }
}
