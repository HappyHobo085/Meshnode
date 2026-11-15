//! The Android context for `ndk-context` (a raw JNI export: UniFFI cannot pass an Android
//! `Context`). iroh's network monitor (`netdev`) and DNS resolver read it, unguarded, from
//! worker threads, so it must be set before a node starts. As in `mesh-ffi`'s
//! `android.rs`; this library has its own `ndk-context` static.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::JNIEnv;
use jni::objects::{JClass, JObject};

/// `initAndroid` has run and a context is set: `join`/`open` may run.
static INITIALISED: AtomicBool = AtomicBool::new(false);
/// Serialises `initAndroid` (so that at most one global reference is made), and holds why
/// the last `initAndroid` failed. This library installs no log subscriber, so the reason
/// reaches the app through `NotInitialised`'s message instead of logcat.
static INIT: Mutex<Option<String>> = Mutex::new(None);

/// `Ok` once `initAndroid` has succeeded; otherwise what `NotInitialised` says.
pub(crate) fn check_initialised() -> Result<(), String> {
    if INITIALISED.load(Ordering::SeqCst) {
        return Ok(());
    }
    let failed = INIT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Err(match failed {
        Some(why) => format!("MeshClientAndroid.init failed: {why}"),
        None => "call MeshClientAndroid.init(context) first".to_owned(),
    })
}

/// `net.meshnode.client.MeshClientAndroid.initAndroid(context)`: gives `ndk-context` the
/// JVM and the **application** context of `context`. Idempotent; never throws: a JNI
/// failure's pending Java exception is cleared (as in `mesh-ffi`), and the failure is
/// kept for `join`/`open`, which then fail with `NotInitialised` saying why.
#[unsafe(no_mangle)]
pub extern "system" fn Java_net_meshnode_client_MeshClientAndroid_initAndroid(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut failed = INIT.lock().unwrap_or_else(|e| e.into_inner());
        if INITIALISED.load(Ordering::SeqCst) {
            return;
        }
        let r = init(&mut env, &context);
        if r.is_err() && env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        *failed = r.err();
    }));
    if r.is_err() {
        let mut failed = INIT.lock().unwrap_or_else(|e| e.into_inner());
        *failed = Some("initAndroid panicked".to_owned());
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
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
