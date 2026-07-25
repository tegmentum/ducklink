//! Stub provider for `sqlite:wasm/extension-loader@0.1.0`.
//!
//! Composed (via `wac plug`) into `cache.wasm` after `sqlite-lib.wasm`
//! so the sqlite-lib component (which itself imports extension-loader
//! through its `sqlite:wasm/library` surface) has a supplier for that
//! interface at instantiation time. Cache-component only calls the
//! plain `spi.execute*` methods — never `library.load-extension` —
//! so the error path here is unreachable in practice.

wit_bindgen::generate!({
    path: "wit",
    world: "sqlite:wasm-stub/sqlite-loader-stub",
    generate_all,
});

use exports::sqlite::wasm::extension_loader::{
    Guest as LoaderGuest, LoadOptions, LoaderError, Manifest,
};

struct Stub;

fn decline(op: &str) -> LoaderError {
    LoaderError {
        code: 1,
        message: format!(
            "sqlite-loader-stub: {op} is not supported (compose the real \
             loader if extension-loading is required)"
        ),
    }
}

impl LoaderGuest for Stub {
    fn load_extension(_path: String, _options: LoadOptions) -> Result<Manifest, LoaderError> {
        Err(decline("load-extension"))
    }

    fn unload_extension(_name: String) -> Result<(), LoaderError> {
        Err(decline("unload-extension"))
    }

    fn load_extension_from_uri(
        _uri: String,
        _options: LoadOptions,
    ) -> Result<Manifest, LoaderError> {
        Err(decline("load-extension-from-uri"))
    }
}

export!(Stub);
