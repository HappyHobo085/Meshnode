//! Provides a stable api to rust crates for interfacing with the Android platform. It is
//! initialized by the runtime, usually [__ndk-glue__](https://crates.io/crates/ndk-glue),
//! but could also be initialized by Java or Kotlin code when embedding in an existing Android
//! project.
//!
//! ```no_run
//! let ctx = ndk_context::android_context();
//! let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
//! let env = vm.attach_current_thread();
//! let class_ctx = env.find_class("android/content/Context")?;
//! let audio_service = env.get_static_field(class_ctx, "AUDIO_SERVICE", "Ljava/lang/String;")?;
//! let audio_manager = env
//!     .call_method(
//!         ctx.context() as jni::sys::jobject,
//!         "getSystemService",
//!         "(Ljava/lang/String;)Ljava/lang/Object;",
//!         &[audio_service],
//!     )?
//!     .l()?;
//! ```
//!
//! **Patched copy (Meshnode, see `README.md`):** `initialize_android_context` keeps an
//! existing context instead of asserting, [`try_android_context`] is added, and the context
//! lives in a `OnceLock` (no `static mut`), so `release_android_context` is a no-op.
use std::ffi::c_void;
use std::sync::OnceLock;

/// The context, set once for the life of the process.
static ANDROID_CONTEXT: OnceLock<SharedContext> = OnceLock::new();

/// The pointers are process-global JNI handles (a `JavaVM` and a global reference), valid
/// from any thread.
struct SharedContext(AndroidContext);

// SAFETY: see `SharedContext`: the JavaVM pointer and the global reference may be used from
// any thread (each thread attaches itself to the VM).
unsafe impl Send for SharedContext {}
// SAFETY: as above; the value is never mutated after it is set.
unsafe impl Sync for SharedContext {}

/// [`AndroidContext`] provides the pointers required to interface with the jni on Android
/// platforms.
#[derive(Clone, Copy, Debug)]
pub struct AndroidContext {
    java_vm: *mut c_void,
    context_jobject: *mut c_void,
}

impl AndroidContext {
    /// A handle to the `JavaVM` object.
    ///
    /// Usage with [__jni__](https://crates.io/crates/jni) crate:
    /// ```no_run
    /// let ctx = ndk_context::android_context();
    /// let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    /// let env = vm.attach_current_thread();
    /// ```
    pub fn vm(self) -> *mut c_void {
        self.java_vm
    }

    /// A handle to an [android.content.Context](https://developer.android.com/reference/android/content/Context).
    /// In most cases this will be a ptr to an `Activity`, but this isn't guaranteed.
    ///
    /// Usage with [__jni__](https://crates.io/crates/jni) crate:
    /// ```no_run
    /// let ctx = ndk_context::android_context();
    /// let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    /// let env = vm.attach_current_thread();
    /// let class_ctx = env.find_class("android/content/Context")?;
    /// let audio_service = env.get_static_field(class_ctx, "AUDIO_SERVICE", "Ljava/lang/String;")?;
    /// let audio_manager = env
    ///     .call_method(
    ///         ctx.context() as jni::sys::jobject,
    ///         "getSystemService",
    ///         "(Ljava/lang/String;)Ljava/lang/Object;",
    ///         &[audio_service],
    ///     )?
    ///     .l()?;
    /// ```
    pub fn context(self) -> *mut c_void {
        self.context_jobject
    }
}

/// Main entry point to this crate. Returns an [`AndroidContext`].
pub fn android_context() -> AndroidContext {
    try_android_context().expect("android context was not initialized")
}

/// The [`AndroidContext`], or `None` while none has been initialized. (Patched: added.)
pub fn try_android_context() -> Option<AndroidContext> {
    ANDROID_CONTEXT.get().map(|c| c.0)
}

/// Initializes the [`AndroidContext`]. [`AndroidContext`] is initialized by [__ndk-glue__](https://crates.io/crates/ndk-glue)
/// before `main` is called.
///
/// # Safety
///
/// The pointers must be valid for the life of the process.
///
/// Patched: if a context is already set, it is kept and this call does nothing (the
/// original asserted). Every caller in the Meshnode app (tao's `onCreate`, mesh-ffi's
/// `initAndroid`) passes the same application context.
pub unsafe fn initialize_android_context(java_vm: *mut c_void, context_jobject: *mut c_void) {
    let _ = ANDROID_CONTEXT.set(SharedContext(AndroidContext {
        java_vm,
        context_jobject,
    }));
}

/// Removes the [`AndroidContext`]. It is released by [__ndk-glue__](https://crates.io/crates/ndk-glue)
/// when the activity is finished and destroyed.
///
/// # Safety
///
/// This function must only be called after [`initialize_android_context()`],
/// when the activity is subsequently destroyed according to Android.
///
/// Patched: a no-op. The context is set once for the life of the process (a `OnceLock`);
/// nothing in the Meshnode dependency tree calls this.
pub unsafe fn release_android_context() {}
