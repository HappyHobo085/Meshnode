//! The Android context for `ndk-context` (raw JNI exports: UniFFI cannot pass an Android
//! `Context`), and the logcat log layer. All code touching `ndk-context` is here.
//!
//! `ndk-context` 0.1.1 holds one process-wide context: `initialize_android_context`
//! asserts that none is set, `release_android_context` asserts that one is. This library
//! shares that static with Tauri's `tao`, which initialises it with the application
//! context in its activity's `onCreate` (tao 0.37 `platform_impl/android/ndk_glue.rs`),
//! once per process. So:
//! - activity first, then the service: `initAndroid` finds the context set and only
//!   records that it is ready;
//! - service first (boot, alarm), then the activity: `initAndroid` sets the context;
//!   `MainActivity` calls `releaseAndroidContext` just before `super.onCreate`, so that
//!   tao's initialisation does not assert. The window between the two is short, and the
//!   context tao sets is the same application context.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use mesh_host::logging::ExtraLayer;

/// The logcat tag.
const LOG_TAG: &str = "meshnode";

/// `ndk-context` has a context (whoever set it): `start` may run.
static INITIALISED: AtomicBool = AtomicBool::new(false);
/// This library set the current context (and may release it for tao).
static OWNED: AtomicBool = AtomicBool::new(false);
/// Serialises the two exports.
static CONTEXT: Mutex<()> = Mutex::new(());

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

/// Whether `ndk-context` has a context (its accessor panics when it has none).
fn context_set() -> bool {
    catch_unwind(ndk_context::android_context).is_ok()
}

/// `net.meshnode.app.MeshNative.initAndroid(context: Context)`: gives `ndk-context` (used
/// by the networking and file-system crates) the JVM and the **application** context
/// (`context.getApplicationContext()`, so an Activity passed in is never kept). If a
/// context is already set (by tao), it is kept. Later calls do nothing. Must run before
/// `start`; a failure shows in `status().last_error`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_meshnode_app_MeshNative_initAndroid(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _one = CONTEXT.lock().unwrap_or_else(|e| e.into_inner());
        if initialised() {
            return Ok(());
        }
        if context_set() {
            INITIALISED.store(true, Ordering::SeqCst);
            return Ok(());
        }
        let vm = env.get_java_vm().map_err(|e| e.to_string())?;
        let app = env
            .call_method(
                &context,
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
        // is leaked deliberately (also after a release: tao sets the same context).
        std::mem::forget(global);
        // SAFETY: both pointers stay valid for the life of the process (the VM never goes
        // away; the global reference is never deleted). No context is set (checked above
        // under `CONTEXT`), so the assertion in `initialize_android_context` holds.
        unsafe {
            ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), ctx.cast());
        }
        OWNED.store(true, Ordering::SeqCst);
        INITIALISED.store(true, Ordering::SeqCst);
        Ok::<(), String>(())
    }));
    let err = match r {
        Ok(Ok(())) => return,
        Ok(Err(e)) => format!("initAndroid failed: {e}"),
        Err(_) => "initAndroid panicked".to_owned(),
    };
    let _ = catch_unwind(|| crate::host::record_error(err));
}

/// `net.meshnode.app.MeshNative.releaseAndroidContext()`: called by `MainActivity` just
/// before `super.onCreate`, where tao sets the (application) context and asserts that
/// none is set. Releases the context only if `initAndroid` set it; the host stays
/// initialised (tao sets the context again at once).
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_meshnode_app_MeshNative_releaseAndroidContext(
    _env: JNIEnv,
    _class: JClass,
) {
    let _ = catch_unwind(|| {
        let _one = CONTEXT.lock().unwrap_or_else(|e| e.into_inner());
        if OWNED.swap(false, Ordering::SeqCst) && context_set() {
            // SAFETY: this library set the context (`OWNED`), and it is still set.
            unsafe { ndk_context::release_android_context() };
        }
    });
}

/// The JNI exports, referenced so that a `cdylib` linking this crate keeps them.
pub(crate) fn jni_symbols() -> [usize; 2] {
    [
        Java_net_meshnode_app_MeshNative_initAndroid as *const () as usize,
        Java_net_meshnode_app_MeshNative_releaseAndroidContext as *const () as usize,
    ]
}
