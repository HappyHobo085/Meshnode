//! The Android context for `ndk-context` (a raw JNI export: UniFFI cannot pass an Android
//! `Context`). iroh's network monitor (`netdev`) and DNS resolver read it, unguarded, from
//! worker threads, so it must be set before a node starts. As in `mesh-ffi`'s
//! `android.rs`; this library has its own `ndk-context` static.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use tracing::error;

/// `initAndroid` has run and a context is set: `join`/`open` may run.
static INITIALISED: AtomicBool = AtomicBool::new(false);
/// Serialises `initAndroid` (so that at most one global reference is made).
static INIT: Mutex<()> = Mutex::new(());

pub(crate) fn initialised() -> bool {
    INITIALISED.load(Ordering::SeqCst)
}

/// `net.meshnode.client.MeshClientAndroid.initAndroid(context)`: gives `ndk-context` the
/// JVM and the **application** context of `context`. Idempotent; never throws (a failure
/// is logged, and `join`/`open` then fail with `NotInitialised`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_meshnode_client_MeshClientAndroid_initAndroid(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _g = INIT.lock().unwrap_or_else(|e| e.into_inner());
        init(&mut env, &context)
    }));
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!("initAndroid failed: {e}"),
        Err(_) => error!("initAndroid panicked"),
    }
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
        // away; the global reference is never deleted).
        unsafe {
            ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), ctx.cast());
        }
    }
    INITIALISED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Compile-time check that the patched `ndk-context` is in use (the published 0.1.1 has
/// no `try_android_context`).
const _: fn() -> Option<ndk_context::AndroidContext> = ndk_context::try_android_context;
