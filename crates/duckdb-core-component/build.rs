fn main() {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "wasm32" {
        return;
    }

    if std::env::var_os("CARGO_FEATURE_FS_SHIMS").is_none() {
        return;
    }

    const SHIM_SYMBOLS: &[&str] = &[
        "open",
        "close",
        "read",
        "pread",
        "write",
        "pwrite",
        "lseek",
        "fsync",
        "fdatasync",
        "ftruncate",
        "stat",
        "lstat",
        "fstat",
        "mkdir",
        "rmdir",
        "unlink",
        "remove",
        "rename",
        "access",
        "isatty",
        "opendir",
        "readdir",
        "closedir",
        "chdir",
        "getcwd",
        "readlink",
        "_ZN6duckdb16DatabaseInstance21LoadExtensionSettingsEv",
    ];

    for sym in SHIM_SYMBOLS {
        println!("cargo:rustc-link-arg=--wrap={}", sym);
    }
}
