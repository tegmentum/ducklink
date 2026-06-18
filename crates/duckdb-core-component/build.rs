fn main() {
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "wasm32" {
        return;
    }

    // DuckDB's libpg_query parser (base_yyparse) is deeply recursive and runs at
    // database-open time: statically-linked extensions (e.g. json) register
    // their internal SQL macros during Load(), which parses SQL via the pg
    // parser. The default 1 MiB wasm stack overflows on that
    // open -> Load -> ParseExpressionList -> yyparse chain and traps in
    // core_yylex. Reserve a larger stack; --stack-first (set by the target)
    // places it at the base of linear memory so an overflow faults cleanly.
    println!("cargo:rustc-link-arg=-z");
    println!("cargo:rustc-link-arg=stack-size=8388608");

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
