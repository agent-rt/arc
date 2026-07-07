//! JNI surface for the Android APK (compiled only on `target_os = "android"`).
//!
//! Exposes the four operations the Compose shell needs — generate an adb key,
//! pair, push a file, run a shell command — as blocking calls (each spins a
//! small Tokio runtime and `block_on`s the async engine). Errors are thrown as
//! Java `RuntimeException`s. Kotlin side (`com.github.agent_rt.arc.AdbNative`):
//!
//! ```kotlin
//! external fun generateKey(): String
//! external fun pair(hostPort: String, code: String, keyPem: String, name: String)
//! external fun pushFile(hostPort: String, keyPem: String, data: ByteArray,
//!                       remotePath: String, mode: Int, mtime: Int)
//! external fun runShell(hostPort: String, keyPem: String, command: String): ByteArray
//! ```

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jbyteArray, jint, jstring};

use crate::key::AdbKey;

/// A fresh current-thread Tokio runtime for one blocking JNI call.
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn throw(env: &mut JNIEnv, e: impl std::fmt::Display) {
    let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_agent_1rt_arc_AdbNative_generateKey<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    match (|| -> anyhow::Result<String> { Ok(AdbKey::generate()?.to_pkcs8_pem()?) })() {
        Ok(pem) => env
            .new_string(pem)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            throw(&mut env, e);
            std::ptr::null_mut()
        }
    }
}

/// Finds the adb wireless connect port by probing localhost (mDNS-free). Returns
/// the port, or 0 if not found (Wireless debugging off).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_agent_1rt_arc_AdbNative_findConnectPort<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jint {
    // Multi-threaded: the port scan fans out thousands of connects, which a
    // current-thread runtime would serialize (≈40s); across workers it's ~1-2s.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            throw(&mut env, e);
            return 0;
        }
    };
    rt.block_on(crate::connect::find_connect_port())
        .map(|p| p as jint)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_agent_1rt_arc_AdbNative_pair<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    host_port: JString<'local>,
    code: JString<'local>,
    key_pem: JString<'local>,
    name: JString<'local>,
) {
    let res = (|| -> anyhow::Result<()> {
        let host_port: String = env.get_string(&host_port)?.into();
        let code: String = env.get_string(&code)?.into();
        let key_pem: String = env.get_string(&key_pem)?.into();
        let name: String = env.get_string(&name)?.into();
        let key = AdbKey::from_pkcs8_pem(&key_pem)?;
        runtime()?.block_on(crate::pairing::pair(&host_port, &code, &key, &name))?;
        Ok(())
    })();
    if let Err(e) = res {
        throw(&mut env, e);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_agent_1rt_arc_AdbNative_pushFile<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    host_port: JString<'local>,
    key_pem: JString<'local>,
    data: JByteArray<'local>,
    remote_path: JString<'local>,
    mode: jint,
    mtime: jint,
) {
    let res = (|| -> anyhow::Result<()> {
        let host_port: String = env.get_string(&host_port)?.into();
        let key_pem: String = env.get_string(&key_pem)?.into();
        let remote_path: String = env.get_string(&remote_path)?.into();
        let bytes = env.convert_byte_array(&data)?;
        let key = AdbKey::from_pkcs8_pem(&key_pem)?;
        runtime()?.block_on(crate::connect::push_file(
            &host_port,
            &key,
            &bytes,
            &remote_path,
            mode as u32,
            mtime as u32,
        ))?;
        Ok(())
    })();
    if let Err(e) = res {
        throw(&mut env, e);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_agent_1rt_arc_AdbNative_runShell<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    host_port: JString<'local>,
    key_pem: JString<'local>,
    command: JString<'local>,
) -> jbyteArray {
    match (|| -> anyhow::Result<Vec<u8>> {
        let host_port: String = env.get_string(&host_port)?.into();
        let key_pem: String = env.get_string(&key_pem)?.into();
        let command: String = env.get_string(&command)?.into();
        let key = AdbKey::from_pkcs8_pem(&key_pem)?;
        Ok(runtime()?.block_on(crate::connect::run_shell(&host_port, &key, &command))?)
    })() {
        Ok(out) => env
            .byte_array_from_slice(&out)
            .map(|a| a.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            throw(&mut env, e);
            std::ptr::null_mut()
        }
    }
}
