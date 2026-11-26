//! 远控会话 FFI:打开/轮询/输入/指令/关闭。
//!
//! 会话句柄注册表:移动端每次会话一个 u64 id。
//! 全部函数非阻塞(spawn 后立即返回,连接进度经 `rc_session_poll` 推送),
//! 但 `rc_session_open` 内部有少量同步初始化,仍建议后台线程调用。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use rc_client_core::session_bridge::{
    SessionCommand, SessionHandle, SessionUpdate, UiSessionConfig,
};
use rc_client_core::ui_error::{classify_session_reason, tool_forbidden_ui_error};
use rc_input::InputEvent;
use rc_session::ToolPermissions;
use serde::Deserialize;

use crate::ui_error::{normalize_ui_error_for_ios, FfiError, MOBILE_ERROR_PLATFORM};
use crate::{read_c_str, to_c_string};

fn sessions() -> &'static Mutex<HashMap<u64, SessionHandle>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, SessionHandle>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<u64, SessionHandle>> {
    sessions().lock().unwrap_or_else(|e| e.into_inner())
}

/// 在会话上执行闭包(供 tools/runtime 复用);会话不存在返回 None。
pub(crate) fn with_session<R>(id: u64, f: impl FnOnce(&SessionHandle) -> R) -> Option<R> {
    lock_sessions().get(&id).map(f)
}

/// 注册会话句柄并返回 id(供 runtime 一键连接复用)。
pub(crate) fn register_session(handle: SessionHandle) -> u64 {
    let id = next_id();
    lock_sessions().insert(id, handle);
    id
}

/// `rc_session_open` 输入。
///
/// 最小直连示例:`{"connect_addr":"192.168.1.10:9901"}`;
/// 完整参数对齐 `UiSessionConfig`。
#[derive(Debug, Deserialize)]
struct OpenRequest {
    /// 对端直连地址 `ip:port`(无信令直连模式)。
    connect_addr: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    watch_only: bool,
    #[serde(default = "default_width")]
    width: u32,
    #[serde(default = "default_height")]
    height: u32,
    #[serde(default = "default_fps")]
    fps: u32,
    #[serde(default = "default_bitrate")]
    bitrate_bps: u32,
}

fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}
fn default_fps() -> u32 {
    30
}
fn default_bitrate() -> u32 {
    8_000_000
}

/// 将工具通道权限序列化为与 `SessionUpdate::ToolPermissionsUpdated.data` 对齐的 JSON 对象。
pub(crate) fn tool_permissions_value(perms: ToolPermissions) -> serde_json::Value {
    serde_json::json!({
        "allow_file_transfer": perms.allow_file_transfer,
        "allow_terminal": perms.allow_terminal,
        "allow_port_map": perms.allow_port_map,
    })
}

/// 将 `SessionUpdate` 中附带的结构化错误统一按 iOS 平台归一化，并保证 `Ended` 始终携带 `ui_error`。
pub(crate) fn normalize_session_update_for_ios(update: SessionUpdate) -> SessionUpdate {
    match update {
        SessionUpdate::Ended {
            reason,
            recoverable,
            ui_error,
        } => {
            let normalized = match ui_error {
                Some(ui) => normalize_ui_error_for_ios(ui),
                None => classify_session_reason(&reason, MOBILE_ERROR_PLATFORM),
            };
            SessionUpdate::Ended {
                reason,
                recoverable,
                ui_error: Some(normalized),
            }
        }
        SessionUpdate::ToolPermissionDenied { channel, ui_error } => {
            SessionUpdate::ToolPermissionDenied {
                channel,
                ui_error: tool_forbidden_ui_error(channel, MOBILE_ERROR_PLATFORM, ui_error.detail),
            }
        }
        other => other,
    }
}

/// 打开会话。返回 `{"session_id": N}` 或错误信封。
///
/// 当前切片:直连模式(含 LAN 直连),信令/中继路由待后续切片。
#[no_mangle]
pub extern "C" fn rc_session_open(json: *const c_char) -> *mut c_char {
    let result: Result<u64, FfiError> = (|| {
        let Some(s) = read_c_str(json) else {
            return Err(FfiError::invalid_json("缺少 JSON 参数"));
        };
        let req: OpenRequest = serde_json::from_str(s)
            .map_err(|e| FfiError::invalid_json(format!("JSON 参数解析失败: {e}")))?;
        let addr: SocketAddr = req.connect_addr.parse().map_err(|_| {
            FfiError::invalid_input(format!("connect_addr 非法: {}", req.connect_addr))
        })?;
        let cfg = UiSessionConfig {
            connect_addr: addr,
            password: req.password,
            watch_only: req.watch_only,
            width: req.width,
            height: req.height,
            fps: req.fps,
            bitrate_bps: req.bitrate_bps,
            prefer_hardware: true,
            clipboard_enabled: false,
            audio_enabled: cfg!(target_os = "ios"),
            ..UiSessionConfig::default()
        };
        let handle = SessionHandle::spawn(cfg);
        Ok(register_session(handle))
    })();
    match result {
        Ok(id) => to_c_string(serde_json::json!({ "session_id": id }).to_string()),
        Err(e) => e.into_c_string(),
    }
}

/// 排空会话状态更新队列。输入 `{"session_id": N}`;
/// 返回 `{"updates": [...], "running": bool, "tool_permissions": {...}}`。
/// `updates` 元素为 `SessionUpdate` JSON(带 `kind` 标签，含 `tool_permissions_updated`、
/// `tool_permission_denied` 与带 `ui_error` 的 `ended`)。
#[no_mangle]
pub extern "C" fn rc_session_poll(json: *const c_char) -> *mut c_char {
    #[derive(Deserialize)]
    struct Req {
        session_id: u64,
    }
    let result: Result<serde_json::Value, FfiError> = (|| {
        let Some(s) = read_c_str(json) else {
            return Err(FfiError::invalid_json("缺少 JSON 参数"));
        };
        let req: Req =
            serde_json::from_str(s).map_err(|e| FfiError::invalid_json(e.to_string()))?;
        let sessions = lock_sessions();
        let Some(handle) = sessions.get(&req.session_id) else {
            return Err(FfiError::session_not_found());
        };
        let mut updates = Vec::new();
        while let Some(update) = handle.try_recv_update() {
            let normalized = normalize_session_update_for_ios(update);
            if let Ok(v) = serde_json::to_value(&normalized) {
                updates.push(v);
            }
        }
        Ok(serde_json::json!({
            "updates": updates,
            "running": handle.is_running(),
            "tool_permissions": tool_permissions_value(handle.tool_permissions()),
        }))
    })();
    match result {
        Ok(v) => to_c_string(v.to_string()),
        Err(e) => e.into_c_string(),
    }
}

/// 查询指定远控会话当前被控端通告的工具通道权限 JSON:
/// `{"allow_file_transfer": bool, "allow_terminal": bool, "allow_port_map": bool}`。
/// 会话不存在时返回带 `ui_error`（`session.not_found`）的标准错误信封。
#[no_mangle]
pub extern "C" fn rc_session_tool_permissions_json(session_id: u64) -> *mut c_char {
    let Some(perms) = with_session(session_id, SessionHandle::tool_permissions) else {
        return FfiError::session_not_found().into_c_string();
    };
    to_c_string(tool_permissions_value(perms).to_string())
}

/// 发送低频控制指令。输入 `{"session_id": N, "command": {...}}`,
/// `command` 为 `SessionCommand` JSON(带 `type` 标签,如 `{"type":"disconnect"}`)。
#[no_mangle]
pub extern "C" fn rc_session_command(json: *const c_char) -> *mut c_char {
    #[derive(Deserialize)]
    struct Req {
        session_id: u64,
        command: SessionCommand,
    }
    let result: Result<(), FfiError> = (|| {
        let Some(s) = read_c_str(json) else {
            return Err(FfiError::invalid_json("缺少 JSON 参数"));
        };
        let req: Req =
            serde_json::from_str(s).map_err(|e| FfiError::invalid_json(e.to_string()))?;
        let sessions = lock_sessions();
        let Some(handle) = sessions.get(&req.session_id) else {
            return Err(FfiError::session_not_found());
        };
        handle.send(req.command);
        Ok(())
    })();
    match result {
        Ok(()) => to_c_string(r#"{"ok":true}"#.to_string()),
        Err(e) => e.into_c_string(),
    }
}

/// 发送输入事件。输入:`{"session_id": N, "kind": "mouse_move"|"mouse_button"|"mouse_wheel"|"key", ...}`。
/// - mouse_move: `{x, y}` 归一化 0..=65535
/// - mouse_button: `{button: "left"|"right"|"middle", pressed}`
/// - mouse_wheel: `{delta, horizontal?}`
/// - key: `{scancode, pressed, modifiers?}`
#[no_mangle]
pub extern "C" fn rc_session_input(json: *const c_char) -> *mut c_char {
    #[derive(Deserialize)]
    struct Req {
        session_id: u64,
        kind: String,
        #[serde(default)]
        x: u16,
        #[serde(default)]
        y: u16,
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        pressed: bool,
        #[serde(default)]
        delta: i16,
        #[serde(default)]
        horizontal: bool,
        #[serde(default)]
        scancode: u16,
        #[serde(default)]
        modifiers: u8,
    }
    let result: Result<(), FfiError> = (|| {
        let Some(s) = read_c_str(json) else {
            return Err(FfiError::invalid_json("缺少 JSON 参数"));
        };
        let req: Req =
            serde_json::from_str(s).map_err(|e| FfiError::invalid_json(e.to_string()))?;
        let ev = match req.kind.as_str() {
            "mouse_move" => InputEvent::MouseMove { x: req.x, y: req.y },
            "mouse_button" => {
                let button = match req.button.as_deref() {
                    Some("left") => rc_input::MouseButton::Left,
                    Some("right") => rc_input::MouseButton::Right,
                    Some("middle") => rc_input::MouseButton::Middle,
                    other => {
                        return Err(FfiError::invalid_input(format!("button 非法: {other:?}")));
                    }
                };
                InputEvent::MouseButton {
                    button,
                    pressed: req.pressed,
                }
            }
            "mouse_wheel" => InputEvent::MouseWheel {
                delta: req.delta,
                horizontal: req.horizontal,
            },
            "key" => InputEvent::Key {
                scancode: req.scancode,
                pressed: req.pressed,
                modifiers: req.modifiers,
            },
            other => return Err(FfiError::invalid_input(format!("kind 非法: {other}"))),
        };
        let sessions = lock_sessions();
        let Some(handle) = sessions.get(&req.session_id) else {
            return Err(FfiError::session_not_found());
        };
        handle.send_input(ev);
        Ok(())
    })();
    match result {
        Ok(()) => to_c_string(r#"{"ok":true}"#.to_string()),
        Err(e) => e.into_c_string(),
    }
}

/// 关闭并销毁会话。输入 `{"session_id": N}`。
#[no_mangle]
pub extern "C" fn rc_session_close(json: *const c_char) -> *mut c_char {
    #[derive(Deserialize)]
    struct Req {
        session_id: u64,
    }
    let result: Result<(), FfiError> = (|| {
        let Some(s) = read_c_str(json) else {
            return Err(FfiError::invalid_json("缺少 JSON 参数"));
        };
        let req: Req =
            serde_json::from_str(s).map_err(|e| FfiError::invalid_json(e.to_string()))?;
        let Some(handle) = lock_sessions().remove(&req.session_id) else {
            return Ok(()); // 已关闭,幂等
        };
        handle.send(SessionCommand::Disconnect);
        Ok(())
    })();
    match result {
        Ok(()) => to_c_string(r#"{"ok":true}"#.to_string()),
        Err(e) => e.into_c_string(),
    }
}
