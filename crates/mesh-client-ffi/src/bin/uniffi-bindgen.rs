//! UniFFI's bindings generator, e.g. Kotlin from the host library:
//! `cargo run -p mesh-client-ffi --features bindgen --bin uniffi-bindgen -- generate
//! --library <mesh_client_ffi.dll> --language kotlin --out-dir <dir>`
//! (`scripts/gen-client-bindings.sh` does this).

fn main() {
    uniffi::uniffi_bindgen_main()
}
