//! 账号路径连接诊断:TLS 初始化 + 登录 + 信令 + 连接 898060465,全程打印。
//! 用法:cargo run --example connect_probe
use std::ffi::CString;
fn call(
    f: extern "C" fn(*const std::os::raw::c_char) -> *mut std::os::raw::c_char,
    json: &str,
) -> String {
    let cj = CString::new(json).unwrap();
    let ptr = f(cj.as_ptr());
    assert!(!ptr.is_null(), "FFI null");
    let s = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe { rc_mobile_ffi::rc_string_free(ptr) };
    s
}
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    use rc_mobile_ffi::runtime::*;
    let dir = std::env::temp_dir().join("rc-connect-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let ca_pem = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/rabbitcontrol/ca.pem"),
    )
    .expect("write ca.pem first");
    let init = call(
        rc_runtime_init,
        &format!(
            r#"{{"server_host":"127.0.0.1","use_https":true,"ca_cert_pem":{},"data_dir":"{}"}}"#,
            serde_json::json!(ca_pem),
            dir.display()
        ),
    );
    println!("init: {}", &init[..init.len().min(200)]);
    let login = call(
        rc_runtime_login,
        r#"{"email":"devin@example.com","password":"test-pass-123","hostname":"probe-mac","os":"macos"}"#,
    );
    println!("login: {}", &login[..login.len().min(200)]);
    let sig = call(rc_signal_start, r#"{"hostname":"probe-mac","os":"macos"}"#);
    println!("signal_start: {}", &sig[..sig.len().min(200)]);
    std::thread::sleep(std::time::Duration::from_secs(8));
    let conn = call(
        rc_runtime_connect,
        &{
            let mut j = serde_json::json!({"target_id":"898060465","watch_only":true,"width":1920,"height":1080,"fps":30,"bitrate_bps":4000000,"timeout_sec":25});
            if let Ok(lan) = std::env::var("PROBE_LAN_DIRECT") {
                j["lan_direct_addr"] = serde_json::json!(lan);
            }
            j.to_string()
        },
    );
    println!("connect: {}", &conn[..conn.len().min(300)]);
    println!("signal_status: {}", call(rc_signal_status, "{}"));
    // 有 session_id 则轮询 20s 打印更新
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&conn) {
        if let Some(sid) = v.get("session_id").and_then(|x| x.as_u64()) {
            for i in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let out = call(
                    rc_mobile_ffi::session::rc_session_poll,
                    &format!(r#"{{"session_id":{sid}}}"#),
                );
                if out.contains("updates") && !out.contains(r#""updates":[]"#) {
                    println!("[{i}] {}", &out[..out.len().min(400)]);
                }
                if out.contains(r#""running":false"#) {
                    println!("[{i}] session ended: {}", &out[..out.len().min(400)]);
                    break;
                }
            }
        }
    }
    std::process::exit(0);
}