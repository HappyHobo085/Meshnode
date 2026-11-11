//! The Android context for `ndk-context` (a raw JNI export: UniFFI cannot pass an Android
//! `Context`), and the logcat log layer. All code touching `ndk-context` is here.
//!
//! This library shares one `ndk-context` static with Tauri's `tao`, which sets it with the
//! application context in its activity's `onCreate` (tao 0.37
//! `platform_impl/android/ndk_glue.rs`). The workspace uses a patched `ndk-context`
//! (`patches/ndk-context`) whose initialisation keeps an existing context instead of
//! asserting, so `initAndroid` and tao may run in either order: the first sets the
//! application context, the second keeps it.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use mesh_host::logging::ExtraLayer;

/// The logcat tag.
const LOG_TAG: &str = "meshnode";

/// `initAndroid` has run and a context is set: `start` may run.
static INITIALISED: AtomicBool = AtomicBool::new(false);
/// Serialises `initAndroid` (so that at most one global reference is made).
static INIT: Mutex<()> = Mutex::new(());

/// Whether `MeshNative.initAndroid` has run successfully.
pub(crate) fn initialised() -> bool {
    INITIALISED.load(Ordering::SeqCst)
}

/// The logcat layer, added beside the log files.
pub(crate) fn log_layer() -> Option<ExtraLayer> {
    tracing_android::layer(LOG_TAG)
        .ok()
        .map(|l| Box::new(l) as ExtraLayer)
}

/// `net.meshnode.app.MeshNative.initAndroid(context: Context)`: gives `ndk-context` (used by
/// the networking and file-system crates) the JVM and the **application** context
/// (`context.getApplicationContext()`; an Activity passed in is never kept).
///
/// Call it on the main thread, from the Service's or the Activity's `onCreate`, before
/// `start`. It is idempotent, and safe in any order relative to tao: if a context is
/// already set, it is kept. A failure shows in `status().last_error` (and `start` then
/// returns `NotInitialised`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_meshnode_app_MeshNative_initAndroid(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _one = INIT.lock().unwrap_or_else(|e| e.into_inner());
        if initialised() {
            return Ok(());
        }
        let r = init(&mut env, &context);
        if r.is_err() && env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        r
    }));
    let err = match r {
        Ok(Ok(())) => return,
        Ok(Err(e)) => format!("initAndroid failed: {e}"),
        Err(_) => "initAndroid panicked".to_owned(),
    };
    let _ = catch_unwind(|| crate::host::record_error(err));
}

fn init(env: &mut JNIEnv, context: &JObject) -> Result<(), String> {
    if ndk_context::try_android_context().is_none() {
        let vm = env.get_java_vm().map_err(|e| e.to_string())?;
        let app = env
            .call_method(
                context,
                "getApplicationContext",
                "()Landroid/content/Context;",
                &[],
            )
            .and_then(|v| v.l())
            .map_err(|e| format!("getApplicationContext: {e}"))?;
        if app.is_null() {
            return Err("getApplicationContext returned null".to_owned());
        }
        let global = env.new_global_ref(&app).map_err(|e| e.to_string())?;
        let ctx = global.as_obj().as_raw();
        // The context must stay valid for the life of the process: the global reference
        // is leaked deliberately.
        std::mem::forget(global);
        // SAFETY: both pointers stay valid for the life of the process (the VM never goes
        // away; the global reference is never deleted). The patched `ndk-context` keeps a
        // context set meanwhile by tao (the same application context).
        unsafe {
            ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), ctx.cast());
        }
    }
    INITIALISED.store(true, Ordering::SeqCst);
    Ok(())
}

/// The JNI export, referenced so that a `cdylib` linking this crate keeps it.
pub(crate) fn jni_symbols() -> [usize; 1] {
    [Java_net_meshnode_app_MeshNative_initAndroid as *const () as usize]
}

/// Compile-time check that the patched `ndk-context` is in use (the published 0.1.1 has
/// no `try_android_context`).
const _: fn() -> Option<ndk_context::AndroidContext> = ndk_context::try_android_context;
