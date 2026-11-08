//! Android glue: initialises btleplug's droidplug backend.
//!
//! A GDExtension is `dlopen`ed by Godot, so it never receives a `JNIEnv` nor the app's class
//! loader. btleplug needs both (it looks up its Java helper classes with `FindClass`). The Java
//! side (`LjdcBlePlugin`) therefore loads this library with `System.loadLibrary` and calls
//! `nativeInit` from the app's main thread, which hands us a valid `JNIEnv`.

use jni::objects::JClass;
use jni::sys::{jboolean, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicBool, Ordering};

static JVM: OnceCell<JavaVM> = OnceCell::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// True once `btleplug::platform::init` succeeded.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

/// Attaches the calling thread to the JVM permanently. Used as the Tokio `on_thread_start`
/// hook: droidplug calls `global_jvm().get_env()` from async tasks, which fails on a thread
/// the JVM does not know.
pub fn attach_current_thread() {
    if let Some(vm) = JVM.get() {
        let _ = vm.attach_current_thread_permanently();
    }
}

/// `LjdcBlePlugin.nativeInit()` — idempotent.
#[no_mangle]
pub extern "system" fn Java_com_lesjeuxducycle_ble_LjdcBlePlugin_nativeInit(
    env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if is_initialized() {
        return JNI_TRUE;
    }
    if let Ok(vm) = env.get_java_vm() {
        let _ = JVM.set(vm);
    }
    match btleplug::platform::init(&env) {
        Ok(()) => {
            INITIALIZED.store(true, Ordering::SeqCst);
            JNI_TRUE
        }
        Err(_) => JNI_FALSE,
    }
}
