fn main() {
    // Register rustc cfg for switching between mount implementations.
    // When fuser MSRV is updated to v1.77 or above, we should switch from 'cargo:' to 'cargo::' syntax.
    println!(
        "cargo:rustc-check-cfg=cfg(fuser_mount_impl, values(\"pure-rust\", \"libfuse2\", \"libfuse3\"))"
    );

    #[cfg(target_os = "macos")]
    {
        // The official macFUSE libfuse2 mount entry point is loaded at runtime.
        // Building and --write-policy=all materialization need no installed driver/SDK.
        println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse2\"");
        println!("cargo:rustc-cfg=feature=\"macfuse-4-compat\"");
    }
    #[cfg(all(not(feature = "libfuse"), not(any(target_os = "linux", target_os = "macos"))))]
    unimplemented!("Building without libfuse is only supported on Linux");

    #[cfg(all(not(feature = "libfuse"), not(target_os = "macos")))]
    {
        println!("cargo:rustc-cfg=fuser_mount_impl=\"pure-rust\"");
    }
    #[cfg(all(feature = "libfuse", not(target_os = "macos")))]
    {
        if cfg!(target_os = "macos") {
            if pkg_config::Config::new()
                .atleast_version("2.6.0")
                .probe("fuse") // for macFUSE 4.x
                .map_err(|e| eprintln!("{e}"))
                .is_ok()
            {
                println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse2\"");
                println!("cargo:rustc-cfg=feature=\"macfuse-4-compat\"");
            } else {
                pkg_config::Config::new()
                    .atleast_version("2.6.0")
                    .probe("osxfuse") // for osxfuse 3.x
                    .map_err(|e| eprintln!("{e}"))
                    .unwrap();
                println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse2\"");
            }
        } else {
            // First try to link with libfuse3
            if pkg_config::Config::new()
                .atleast_version("3.0.0")
                .probe("fuse3")
                .map_err(|e| eprintln!("{e}"))
                .is_ok()
            {
                println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse3\"");
            } else {
                // Fallback to libfuse
                pkg_config::Config::new()
                    .atleast_version("2.6.0")
                    .probe("fuse")
                    .map_err(|e| eprintln!("{e}"))
                    .unwrap();
                println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse2\"");
            }
        }
    }
}
