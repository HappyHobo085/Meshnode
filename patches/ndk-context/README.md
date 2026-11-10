# ndk-context 0.1.1 (patched for Meshnode)

This is a copy of [`ndk-context` 0.1.1](https://crates.io/crates/ndk-context) (MIT OR Apache-2.0, The Rust
Windowing contributors), applied with `[patch.crates-io]`. The published crate ships no license files, so
`LICENSE-MIT` (with the authors from its manifest) and `LICENSE-APACHE` hold the standard texts of the licenses
it declares.

## Why

The Meshnode Android app has one native library, `libmeshnode_app.so`, so one `ndk-context` static.
Two parties set it:

- Tauri's `tao` (0.37, `platform_impl/android/ndk_glue.rs`, `onCreate`) sets it with the application
  context, once per process, when the activity is created;
- `mesh-ffi`'s `MeshNative.initAndroid` sets it when the foreground service starts, which can happen
  first (boot, alarm).

The original `initialize_android_context` asserts that no context is set, so whichever goes second
panics. Releasing the context in between leaves a window without one, and `netdev` (iroh's network
monitor) reads `android_context()` unguarded from worker threads. See the sub-spec 5 design
(`docs/superpowers/specs/2026-11-10-android-shell-design.md`, §3) and its task 7 review.

## What changed (`src/lib.rs` only)

1. `initialize_android_context`: when a context is already set, it is kept and the call does nothing
   (no assert, no replace). Both callers pass the same application context.
2. New `pub fn try_android_context() -> Option<AndroidContext>`, which does not panic.
3. The context lives in a `OnceLock` instead of a `static mut` (no data race).
   `release_android_context` is therefore a no-op; no crate in the dependency tree calls it.

The public API is otherwise unchanged.
