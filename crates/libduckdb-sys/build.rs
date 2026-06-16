use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let include_dir = find_include_dir();
    let header = include_dir.join("duckdb.h");
    if !header.exists() {
        panic!("Unable to locate duckdb.h at {}", header.display());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let static_lib = env::var("DUCKDB_STATIC_LIB")
        .expect("Set DUCKDB_STATIC_LIB to the prebuilt libduckdb static library");

    let lib_src = Path::new(&static_lib);
    if !lib_src.exists() {
        panic!(
            "The file provided in DUCKDB_STATIC_LIB does not exist: {}",
            lib_src.display()
        );
    }

    let lib_dst = out_dir.join("libduckdb.a");
    fs::copy(lib_src, &lib_dst).expect("Failed to copy libduckdb static library into OUT_DIR");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=duckdb");
    println!("cargo:rustc-link-arg=-Wl,--start-group");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let default_build_dir = manifest_dir
        .parent()
        .expect("crate dir")
        .parent()
        .expect("workspace dir")
        .join("build")
        .join("duckdb-wasi");
    let build_dir = env::var("DUCKDB_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or(default_build_dir);

    let auxiliary_libs = [
        ("third_party/zstd", "duckdb_zstd"),
        ("third_party/re2", "duckdb_re2"),
        ("third_party/skiplist", "duckdb_skiplistlib"),
        ("third_party/hyperloglog", "duckdb_hyperloglog"),
        ("third_party/miniz", "duckdb_miniz"),
        ("third_party/mbedtls", "duckdb_mbedtls"),
        ("third_party/utf8proc", "duckdb_utf8proc"),
        ("third_party/fmt", "duckdb_fmt"),
        ("third_party/fsst", "duckdb_fsst"),
        ("third_party/yyjson", "duckdb_yyjson"),
        ("third_party/libpg_query", "duckdb_pg_query"),
        ("third_party/fastpforlib", "duckdb_fastpforlib"),
        ("extension/core_functions", "core_functions_extension"),
        ("extension/parquet", "parquet_extension"),
    ];

    for (rel_dir, lib_name) in auxiliary_libs.iter() {
        let lib_path = build_dir.join(rel_dir).join(format!("lib{}.a", lib_name));
        if lib_path.exists() {
            if let Some(parent) = lib_path.parent() {
                println!("cargo:rustc-link-search=native={}", parent.display());
            }
            println!("cargo:rustc-link-lib=static={}", lib_name);
        }
    }

    if let Ok(prefix) = env::var("WASI_SDK_PREFIX") {
        let target = env::var("TARGET").unwrap_or_default();
        let sysroot_path = if target.contains("wasip2") {
            "share/wasi-sysroot/lib/wasm32-wasip2"
        } else if target.contains("wasip1") {
            "share/wasi-sysroot/lib/wasm32-wasip1"
        } else {
            "share/wasi-sysroot/lib/wasm32-wasi"
        };
        let sysroot_lib = Path::new(&prefix).join(sysroot_path);
        if sysroot_lib.exists() {
            println!("cargo:rustc-link-search=native={}", sysroot_lib.display());
            println!("cargo:rustc-link-lib=static=c++abi");
            println!("cargo:rustc-link-lib=static=c++");
            println!("cargo:rustc-link-lib=static=m");
        }
    }
    println!("cargo:rustc-link-arg=-Wl,--end-group");
}

fn find_include_dir() -> PathBuf {
    if let Ok(include) = env::var("DUCKDB_INCLUDE_DIR") {
        return PathBuf::from(include);
    }

    if let Ok(source_dir) = env::var("DUCKDB_SOURCE_DIR") {
        let candidate = PathBuf::from(&source_dir).join("src").join("include");
        if candidate.join("duckdb.h").exists() {
            return candidate;
        }
    }

    panic!("Set DUCKDB_INCLUDE_DIR or DUCKDB_SOURCE_DIR so duckdb.h can be located");
}
