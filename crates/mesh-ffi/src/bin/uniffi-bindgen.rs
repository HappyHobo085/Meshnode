//! UniFFI's bindings generator, e.g. Kotlin from the Android library:
//! `cargo run -p mesh-ffi --features bindgen --bin uniffi-bindgen -- generate --library
//! <libmesh_ffi.so> --language kotlin --out-dir <dir>`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
