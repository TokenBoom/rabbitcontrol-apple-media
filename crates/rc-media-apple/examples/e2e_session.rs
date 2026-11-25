//! 端到端验证:直连本机 rc-cli host,开会话,验证 VT 解码出帧。
//! 运行前先启动:rc-cli host --backend real --listen 0.0.0.0:9901 --approval auto --duration 300s
use std::ffi::CString;

fn call(f: extern "C" fn(*const std::os::raw::c_char) -> *mut std::os::raw::c_char, json: &str) -> String {
    let cj = CString::new(json).unwrap();
    let ptr = f(cj.as_ptr());
    assert!(!ptr.is_null());
    let s = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
    unsafe { rc_mobile_ffi::rc_string_free(ptr) };
    s
}

fn main() {
    assert_eq!(rc_media_apple::rc_apple_media_init(), 1, "媒体通路注册失败");
    let open = call(
        rc_mobile_ffi::session::rc_session_open,
        r#"{"connect_addr":"127.0.0.1:9901","width":1920,"height":1080,"fps":30,"bitrate_bps":8000000}"#,
    );
    println!("open: {open}");
    let id = serde_json::from_str::<serde_json::Value>(&open)
        .ok()
        .and_then(|v| v["session_id"].as_u64())
        .expect("open 失败");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut saw_negotiated = false;
    let mut frames = 0u32;
    while std::time::Instant::now() < deadline && frames < 5 {
        let poll = call(
            rc_mobile_ffi::session::rc_session_poll,
            &format!(r#"{{"session_id":{id}}}"#),
        );
        if poll.contains("\"negotiated\"") && !saw_negotiated {
            saw_negotiated = true;
            println!("negotiated!");
        }
        if poll.contains("\"ended\"") {
            println!("ENDED: {}", &poll[..poll.len().min(400)]);
            break;
        }
        let take = rc_media_apple::rc_apple_media_frame_take();
        assert!(!take.is_null());
        let ts = unsafe { std::ffi::CStr::from_ptr(take) }.to_str().unwrap().to_string();
        unsafe { rc_media_apple::rc_apple_media_string_free(take) };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ts) {
            if let Some(ptr) = v["ptr"].as_u64() {
                if ptr != 0 {
                    frames += 1;
                    println!("frame {frames}: {}x{} seq={}", v["width"], v["height"], v["seq"]);
                    unsafe { rc_media_apple::rc_apple_media_frame_release(ptr) };
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!("RESULT: negotiated={saw_negotiated} frames={frames}");
    std::process::exit(if frames > 0 { 0 } else { 1 });
}
