//! The Android context, set by a raw JNI export (UniFFI cannot pass an Android
//! `Context`), and the logcat log layer.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use mesh_host::logging::ExtraLayer;

/// The logcat tag.
const LOG_TAG: &str = "meshnode";

static INITIALISED: AtomicBool = AtomicBool::new(false);
/// Serialises the initialisation, so that it happens once.
static INIT: Mutex<()> = Mutex::new(());

/// Whether `MeshNative.initAndroid` has run.
pub(crate) fn initialised() -> bool {
    INITIALISED.load(Ordering::SeqCst)
}

/// The logcat layer, added beside the log files.
pub(crate) fn log_layer() -> Option<ExtraLayer> {
    tracing_android::layer(LOG_TAG)
        .ok()
        .map(|l| Box::new(l) as ExtraLayer)
}

/// `net.meshnode.app.MeshNative.initAndroid(context: Context)`: hands the JVM and the
/// application context to `ndk-context` (which the networking and file-system crates
/// use), once. Later calls do nothing. Must run before `start`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_meshnode_app_MeshNative_initAndroid(
    env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _once = INIT.lock().unwrap_or_else(|e| e.into_inner());
        if initialised() {
            return Ok(());
        }
        let vm = env.get_java_vm().map_err(|e| e.to_string())?;
        let global = env.new_global_ref(context).map_err(|e| e.to_string())?;
        let ctx = global.as_obj().as_raw();
        // The context must stay valid for the life of the process: the global reference
        // is leaked deliberately.
        std::mem::forget(global);
        // SAFETY: both pointers are valid for the life of the process (the VM never goes
        // away; the global reference is never deleted), and this runs once (guarded by
        // `INIT` / `INITIALISED`).
        unsafe {
            ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), ctx.cast());
        }
        INITIALISED.store(true, Ordering::SeqCst);
        Ok::<(), String>(())
    }));
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("initAndroid failed: {e}"),
        Err(_) => tracing::error!("initAndroid panicked"),
    }
}
