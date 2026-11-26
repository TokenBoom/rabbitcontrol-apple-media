//! 移动端 FFI 统一结构化错误信封与错误码解析边界。
//!
//! 约定:
//! - 所有失败 JSON 保留旧版 `error` + `kind` 字段确保旧版 iOS 前向兼容；
//! - 同时增量附加标准 `ui_error` 对象（`rc_client_core::ui_error::UiError`，camelCase 10 字段，
//!   与 `rabbitcontrol-spec` `RCUiError` 100% 字节级对齐）；
//! - 全链路固定使用 [`ErrorPlatform::Ios`] 解析中文文案与平台覆写。

use std::os::raw::c_char;

use rc_client_core::auth_client::AuthClientError;
use rc_client_core::ui_error::{
    classify_signal_error, codes, lookup_error_entry, resolve_ui_error, ErrorPlatform, IntoUiError,
    UiError,
};

use crate::{read_c_str, to_c_string};

/// 移动端 FFI 固定使用的错误文案平台（确保单测跑在 macOS 宿主上也输出 iOS 覆写文案）。
pub const MOBILE_ERROR_PLATFORM: ErrorPlatform = ErrorPlatform::Ios;

/// 移动端 FFI 统一结构化错误（`UiError` 装箱以保持 `Result<_, FfiError>` 轻量，避免栈膨胀）。
#[derive(Debug, Clone)]
pub(crate) struct FfiError {
    pub(crate) message: String,
    pub(crate) kind: &'static str,
    pub(crate) ui_error: Box<UiError>,
}

impl FfiError {
    /// 由 `AuthClientError` 构造带前向兼容 `kind` 与 iOS 规范 `UiError` 的错误信封。
    pub(crate) fn from_auth(err: &AuthClientError) -> Self {
        Self::from_auth_with_message(err.to_string(), err)
    }

    /// 由 `AuthClientError` 构造错误信封，并保留调用侧定制的旧版 `error` 文案前缀。
    pub(crate) fn from_auth_with_message(
        message: impl Into<String>,
        err: &AuthClientError,
    ) -> Self {
        let ui_error = err.to_ui_error(MOBILE_ERROR_PLATFORM);
        let kind = auth_error_kind(err, &ui_error);
        Self {
            message: message.into(),
            kind,
            ui_error: Box::new(ui_error),
        }
    }

    /// 通过规范错误码构造 `kind = "other"` 的错误信封。
    pub(crate) fn resolved(code: &str, detail: impl Into<String>) -> Self {
        Self::resolved_with_kind(code, "other", detail)
    }

    /// 通过规范错误码与显式 `kind` 构造错误信封。
    pub(crate) fn resolved_with_kind(
        code: &str,
        kind: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        let detail_str = detail.into();
        let ui_error = resolve_ui_error(
            code,
            MOBILE_ERROR_PLATFORM,
            &[],
            None,
            Some(detail_str.clone()),
        );
        Self {
            message: detail_str,
            kind,
            ui_error: Box::new(ui_error),
        }
    }

    /// 直接使用已构造的 `UiError` 包装错误信封。
    pub(crate) fn from_ui_error(
        message: impl Into<String>,
        kind: &'static str,
        ui_error: UiError,
    ) -> Self {
        Self {
            message: message.into(),
            kind,
            ui_error: Box::new(ui_error),
        }
    }

    /// JSON 参数缺失或反序列化失败。
    pub(crate) fn invalid_json(detail: impl Into<String>) -> Self {
        Self::resolved(codes::VALIDATION_INVALID_JSON, detail)
    }

    /// 业务输入字段缺失或非法。
    pub(crate) fn invalid_input(detail: impl Into<String>) -> Self {
        Self::resolved(codes::VALIDATION_INVALID_INPUT, detail)
    }

    /// 运行时配置不合法或与当前模式不匹配。
    pub(crate) fn invalid_config(detail: impl Into<String>) -> Self {
        Self::resolved(codes::VALIDATION_INVALID_CONFIG, detail)
    }

    /// 指定远控会话不存在。
    pub(crate) fn session_not_found() -> Self {
        Self::resolved(codes::SESSION_NOT_FOUND, "会话不存在")
    }

    /// 移动端运行时尚未初始化。
    pub(crate) fn runtime_not_initialized(detail: impl Into<String>) -> Self {
        Self::resolved(codes::SERVICE_RUNTIME_NOT_INITIALIZED, detail)
    }

    /// 信令客户端未启动。
    pub(crate) fn signal_not_started(detail: impl Into<String>) -> Self {
        Self::resolved(codes::SIGNAL_NOT_STARTED, detail)
    }

    /// 将信令建连/查询错误归因为结构化 iOS `UiError`。
    pub(crate) fn from_signal_error(message: impl Into<String>, raw_signal_err: &str) -> Self {
        let ui_error = classify_signal_error_for_ios(raw_signal_err);
        let kind = if ui_error.code == codes::AUTH_TOKEN_REUSE_DETECTED {
            "token_reuse"
        } else if matches!(
            ui_error.code.as_str(),
            codes::AUTH_UNAUTHORIZED
                | codes::AUTH_TOKEN_EXPIRED
                | codes::AUTH_TOKEN_REVOKED
                | codes::AUTH_RELOGIN_REQUIRED
        ) {
            "unauthorized"
        } else {
            "other"
        };
        Self {
            message: message.into(),
            kind,
            ui_error: Box::new(ui_error),
        }
    }

    /// 序列化为前向兼容的错误 JSON 对象：`{"error": "...", "kind": "...", "ui_error": {...}}`。
    pub(crate) fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.message,
            "kind": self.kind,
            "ui_error": self.ui_error,
        })
    }

    /// 序列化为 JSON 字符串。
    pub(crate) fn to_json_string(&self) -> String {
        self.to_json_value().to_string()
    }

    /// 转换为调用方经 `rc_string_free` 释放的 C 字符串。
    pub(crate) fn into_c_string(self) -> *mut c_char {
        to_c_string(self.to_json_string())
    }
}

pub(crate) fn auth_error_kind(err: &AuthClientError, ui_error: &UiError) -> &'static str {
    if err.is_token_reuse() || ui_error.code == codes::AUTH_TOKEN_REUSE_DETECTED {
        "token_reuse"
    } else if err.is_unauthorized() {
        "unauthorized"
    } else if err.is_password_change_required()
        || ui_error.code == codes::AUTH_PASSWORD_CHANGE_REQUIRED
    {
        "password_change_required"
    } else if err.is_registration_disabled() || ui_error.code == codes::AUTH_REGISTRATION_DISABLED {
        "registration_disabled"
    } else if matches!(err, AuthClientError::ReloginRequired(_)) {
        "unauthorized"
    } else {
        "other"
    }
}

/// 将信令通道错误字符串归因为 iOS 平台 [`UiError`]，补充本地校验/未启动/钥匙串令牌过期分支。
pub(crate) fn classify_signal_error_for_ios(raw: &str) -> UiError {
    let trimmed = raw.trim();
    if trimmed.contains("请刷新钥匙串登录态") {
        return resolve_ui_error(
            codes::AUTH_RELOGIN_REQUIRED,
            MOBILE_ERROR_PLATFORM,
            &[],
            None,
            Some(trimmed.to_string()),
        );
    }
    if trimmed.contains("设备码格式无效") {
        return resolve_ui_error(
            codes::VALIDATION_INVALID_INPUT,
            MOBILE_ERROR_PLATFORM,
            &[],
            None,
            Some(trimmed.to_string()),
        );
    }
    if trimmed.contains("未启用信令服务器")
        || trimmed.contains("信令客户端已停止")
        || trimmed.contains("信令未启动")
    {
        return resolve_ui_error(
            codes::SIGNAL_NOT_STARTED,
            MOBILE_ERROR_PLATFORM,
            &[],
            None,
            Some(trimmed.to_string()),
        );
    }
    let classified = classify_signal_error(trimmed, MOBILE_ERROR_PLATFORM);
    if classified.code == codes::NETWORK_UNREACHABLE
        && (trimmed.contains("获取信令认证令牌失败")
            || trimmed.contains("刷新失效信令认证令牌失败"))
    {
        return resolve_ui_error(
            codes::AUTH_RELOGIN_REQUIRED,
            MOBILE_ERROR_PLATFORM,
            &[],
            None,
            Some(trimmed.to_string()),
        );
    }
    classified
}

/// 将 `rc-client-core` 按宿主编译平台生成的 `UiError` 归一化为 [`ErrorPlatform::Ios`] 文案。
///
/// 对带模板参数的条目保留已插值文案（当前目录中带参数条目均无 iOS 覆写），
/// 对无参数条目重新按 `ErrorPlatform::Ios` 解析以应用 iOS 平台覆写建议。
pub(crate) fn normalize_ui_error_for_ios(ui: UiError) -> UiError {
    let has_params = lookup_error_entry(&ui.code).is_some_and(|e| !e.params.is_empty());
    if has_params {
        ui
    } else {
        resolve_ui_error(&ui.code, MOBILE_ERROR_PLATFORM, &[], ui.status, ui.detail)
    }
}

/// 按 iOS 平台解析错误码或 wire alias，返回标准 `UiError` JSON（camelCase 10 字段，
/// 与 `rabbitcontrol-spec` `RCUiError` 100% 字节级对齐）。
///
/// - `code`：规范错误码（如 `"platform.ios.keychain_error"`）或 wire alias（如 `"ios_keychain_error"`）；
///   传入空指针、非 UTF-8、空白或未知码时安全兜底为 `"common.unknown"`。
/// - `detail`：可选原始技术诊断细节；传入空指针、非 UTF-8 或纯空白时省略 `detail` 字段。
/// - 返回值由 Rust 侧分配，调用方必须经 [`crate::rc_string_free`] 释放。
#[no_mangle]
pub extern "C" fn rc_ui_error_resolve(code: *const c_char, detail: *const c_char) -> *mut c_char {
    let raw_code = read_c_str(code).map(str::trim).unwrap_or("");
    let effective_code = if raw_code.is_empty() {
        codes::COMMON_UNKNOWN
    } else {
        raw_code
    };
    let detail_opt = read_c_str(detail).map(str::to_string);
    let ui_error = resolve_ui_error(effective_code, MOBILE_ERROR_PLATFORM, &[], None, detail_opt);
    let json = serde_json::to_string(&ui_error).unwrap_or_else(|_| {
        let fallback = resolve_ui_error(
            codes::COMMON_UNKNOWN,
            MOBILE_ERROR_PLATFORM,
            &[],
            None,
            None,
        );
        serde_json::to_string(&fallback).unwrap_or_else(|_| "{}".to_string())
    });
    to_c_string(json)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::{CStr, CString};

    use rc_client_core::auth_client::AuthClientError;
    use rc_client_core::server_config::{ServerConnectionState, ServerStatusSnapshot};
    use rc_client_core::session_bridge::SessionUpdate;
    use rc_client_core::ui_error::{
        codes, resolve_ui_error, tool_forbidden_ui_error, ErrorPlatform, UiError,
    };
    use rc_session::{ToolPermissions, TOOL_CHANNEL_FILE};

    use super::*;
    use crate::auth::{rc_auth_login, rc_auth_logout, rc_auth_refresh, rc_server_probe};
    use crate::rc_string_free;
    use crate::runtime::{
        normalize_server_status_for_ios, rc_runtime_connect, rc_runtime_init, rc_signal_status,
    };
    use crate::session::{
        normalize_session_update_for_ios, rc_session_close, rc_session_command, rc_session_input,
        rc_session_open, rc_session_poll, rc_session_tool_permissions_json,
    };
    use crate::tools::{rc_tools_call, rc_tools_poll};

    fn take_json(ptr: *mut c_char) -> serde_json::Value {
        assert!(!ptr.is_null(), "FFI returned null pointer");
        let raw = unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .expect("FFI returned invalid UTF-8")
            .to_string();
        unsafe { rc_string_free(ptr) };
        serde_json::from_str(&raw).expect("FFI returned invalid JSON")
    }

    fn assert_rc_ui_error_schema(val: &serde_json::Value) -> UiError {
        let obj = val.as_object().expect("ui_error must be a JSON object");
        let allowed: BTreeSet<&str> = [
            "code",
            "title",
            "message",
            "suggestion",
            "severity",
            "presentation",
            "action",
            "retryable",
            "status",
            "detail",
        ]
        .into_iter()
        .collect();
        for key in obj.keys() {
            assert!(
                allowed.contains(key.as_str()),
                "unexpected key in ui_error (not in RCUiError): {key}"
            );
        }
        for required in [
            "code",
            "title",
            "message",
            "suggestion",
            "severity",
            "presentation",
            "action",
            "retryable",
        ] {
            assert!(
                obj.contains_key(required),
                "missing required RCUiError key: {required}"
            );
        }
        let parsed: UiError =
            serde_json::from_value(val.clone()).expect("ui_error must deserialize as UiError");
        assert!(!parsed.code.is_empty());
        assert!(!parsed.title.contains('{') && !parsed.title.contains('}'));
        assert!(!parsed.message.contains('{') && !parsed.message.contains('}'));
        assert!(!parsed.suggestion.contains('{') && !parsed.suggestion.contains('}'));
        parsed
    }

    fn assert_error_envelope(val: &serde_json::Value) -> (String, String, UiError) {
        let error = val
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error envelope missing legacy 'error' string")
            .to_string();
        let kind = val
            .get("kind")
            .and_then(|v| v.as_str())
            .expect("error envelope missing legacy 'kind' string")
            .to_string();
        let ui_val = val
            .get("ui_error")
            .expect("error envelope missing 'ui_error' object");
        let ui_error = assert_rc_ui_error_schema(ui_val);
        (error, kind, ui_error)
    }

    #[test]
    fn rc_ui_error_resolve_handles_null_known_alias_and_unknown_codes() {
        // 1) (NULL, NULL) -> common.unknown without detail
        let v_null = take_json(rc_ui_error_resolve(std::ptr::null(), std::ptr::null()));
        let ui_null = assert_rc_ui_error_schema(&v_null);
        assert_eq!(ui_null.code, codes::COMMON_UNKNOWN);
        assert_eq!(ui_null.detail, None);
        assert!(v_null.get("detail").is_none());

        // 2) (NULL, detail) -> common.unknown preserving detail
        let detail = CString::new("NSURLErrorTimedOut").unwrap();
        let v_null_code = take_json(rc_ui_error_resolve(std::ptr::null(), detail.as_ptr()));
        let ui_null_code = assert_rc_ui_error_schema(&v_null_code);
        assert_eq!(ui_null_code.code, codes::COMMON_UNKNOWN);
        assert_eq!(ui_null_code.detail.as_deref(), Some("NSURLErrorTimedOut"));

        // 3) whitespace-only code & detail -> common.unknown without detail
        let blank = CString::new("   \t ").unwrap();
        let v_blank = take_json(rc_ui_error_resolve(blank.as_ptr(), blank.as_ptr()));
        let ui_blank = assert_rc_ui_error_schema(&v_blank);
        assert_eq!(ui_blank.code, codes::COMMON_UNKNOWN);
        assert_eq!(ui_blank.detail, None);

        // 4) known canonical iOS code -> applies ErrorPlatform::Ios override
        let keychain_code = CString::new(codes::PLATFORM_IOS_KEYCHAIN_ERROR).unwrap();
        let keychain_detail = CString::new("OSStatus -25308 errSecInteractionNotAllowed").unwrap();
        let v_keychain = take_json(rc_ui_error_resolve(
            keychain_code.as_ptr(),
            keychain_detail.as_ptr(),
        ));
        let ui_keychain = assert_rc_ui_error_schema(&v_keychain);
        assert_eq!(ui_keychain.code, codes::PLATFORM_IOS_KEYCHAIN_ERROR);
        assert!(
            ui_keychain.suggestion.contains("iPhone/iPad"),
            "expected iOS override suggestion, got: {}",
            ui_keychain.suggestion
        );
        assert_eq!(
            ui_keychain.detail.as_deref(),
            Some("OSStatus -25308 errSecInteractionNotAllowed")
        );

        // 5) wire alias (case-insensitive & padded) -> resolves with ErrorPlatform::Ios override
        let alias = CString::new("  TOKEN_REUSE_DETECTED  ").unwrap();
        let v_alias = take_json(rc_ui_error_resolve(alias.as_ptr(), std::ptr::null()));
        let ui_alias = assert_rc_ui_error_schema(&v_alias);
        assert_eq!(ui_alias.code, codes::AUTH_TOKEN_REUSE_DETECTED);
        assert!(
            ui_alias.suggestion.contains("钥匙串"),
            "expected iOS keychain override for token_reuse_detected, got: {}",
            ui_alias.suggestion
        );

        let ios_alias = CString::new("ios_keychain_error").unwrap();
        let v_ios_alias = take_json(rc_ui_error_resolve(ios_alias.as_ptr(), std::ptr::null()));
        let ui_ios_alias = assert_rc_ui_error_schema(&v_ios_alias);
        assert_eq!(ui_ios_alias.code, codes::PLATFORM_IOS_KEYCHAIN_ERROR);

        // 6) unknown code -> fallback to common.unknown preserving detail
        let unknown = CString::new("custom.unknown.code").unwrap();
        let unknown_detail = CString::new("raw diagnostic").unwrap();
        let v_unknown = take_json(rc_ui_error_resolve(
            unknown.as_ptr(),
            unknown_detail.as_ptr(),
        ));
        let ui_unknown = assert_rc_ui_error_schema(&v_unknown);
        assert_eq!(ui_unknown.code, codes::COMMON_UNKNOWN);
        assert_eq!(ui_unknown.detail.as_deref(), Some("raw diagnostic"));

        // 7) invalid non-UTF-8 byte strings -> safe fallback without panic or NULL
        let invalid_utf8: &[u8] = b"\xff\xfe\0";
        let invalid_ptr = invalid_utf8.as_ptr().cast::<c_char>();
        let v_bad_both = take_json(rc_ui_error_resolve(invalid_ptr, invalid_ptr));
        let ui_bad_both = assert_rc_ui_error_schema(&v_bad_both);
        assert_eq!(ui_bad_both.code, codes::COMMON_UNKNOWN);
        assert_eq!(ui_bad_both.detail, None);

        let v_bad_detail = take_json(rc_ui_error_resolve(keychain_code.as_ptr(), invalid_ptr));
        let ui_bad_detail = assert_rc_ui_error_schema(&v_bad_detail);
        assert_eq!(ui_bad_detail.code, codes::PLATFORM_IOS_KEYCHAIN_ERROR);
        assert_eq!(ui_bad_detail.detail, None);
    }

    #[test]
    fn auth_error_envelope_preserves_legacy_fields_and_attaches_ios_ui_error() {
        let reuse = AuthClientError::Api {
            status: 401,
            code: "token_reuse_detected".to_string(),
            message: "replayed refresh token".to_string(),
        };
        let reuse_val: serde_json::Value =
            serde_json::from_str(&crate::auth::error_json(&reuse)).unwrap();
        let (msg, kind, ui) = assert_error_envelope(&reuse_val);
        assert!(msg.contains("token_reuse_detected"));
        assert_eq!(kind, "token_reuse");
        assert_eq!(ui.code, codes::AUTH_TOKEN_REUSE_DETECTED);
        assert_eq!(ui.status, Some(401));
        assert!(ui.suggestion.contains("钥匙串"));

        let unauth = AuthClientError::Api {
            status: 401,
            code: "invalid_credentials".to_string(),
            message: "bad password".to_string(),
        };
        let unauth_val: serde_json::Value =
            serde_json::from_str(&crate::auth::error_json(&unauth)).unwrap();
        let (_, kind, ui) = assert_error_envelope(&unauth_val);
        assert_eq!(kind, "unauthorized");
        assert_eq!(ui.code, codes::AUTH_INVALID_CREDENTIALS);
        assert_eq!(ui.status, Some(401));

        let pw_change = AuthClientError::Api {
            status: 403,
            code: "password_change_required".to_string(),
            message: "must change".to_string(),
        };
        let pw_val: serde_json::Value =
            serde_json::from_str(&crate::auth::error_json(&pw_change)).unwrap();
        let (_, kind, ui) = assert_error_envelope(&pw_val);
        assert_eq!(kind, "password_change_required");
        assert_eq!(ui.code, codes::AUTH_PASSWORD_CHANGE_REQUIRED);

        let reg_disabled = AuthClientError::Api {
            status: 403,
            code: "registration_disabled".to_string(),
            message: "closed".to_string(),
        };
        let reg_val: serde_json::Value =
            serde_json::from_str(&crate::auth::error_json(&reg_disabled)).unwrap();
        let (_, kind, ui) = assert_error_envelope(&reg_val);
        assert_eq!(kind, "registration_disabled");
        assert_eq!(ui.code, codes::AUTH_REGISTRATION_DISABLED);

        // FFI invalid JSON & missing param paths in auth.rs
        let v_probe_null = take_json(rc_server_probe(std::ptr::null()));
        let (_, kind, ui) = assert_error_envelope(&v_probe_null);
        assert_eq!(kind, "other");
        assert_eq!(ui.code, codes::VALIDATION_INVALID_JSON);

        let bad_json = CString::new("{not-json").unwrap();
        let v_login_bad = take_json(rc_auth_login(bad_json.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_login_bad);
        assert_eq!(ui.code, codes::VALIDATION_INVALID_JSON);

        let missing_refresh = CString::new(r#"{"server_host":"127.0.0.1"}"#).unwrap();
        let v_refresh = take_json(rc_auth_refresh(missing_refresh.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_refresh);
        assert_eq!(ui.code, codes::VALIDATION_INVALID_INPUT);

        let v_logout = take_json(rc_auth_logout(missing_refresh.as_ptr()));
        let (_, kind, ui) = assert_error_envelope(&v_logout);
        assert_eq!(kind, "other");
        assert_eq!(ui.code, codes::VALIDATION_INVALID_INPUT);
    }

    #[test]
    fn runtime_and_signal_error_envelopes_and_snapshot_normalization() {
        let v_init_null = take_json(rc_runtime_init(std::ptr::null()));
        let (_, kind, ui) = assert_error_envelope(&v_init_null);
        assert_eq!(kind, "other");
        assert_eq!(ui.code, codes::VALIDATION_INVALID_JSON);

        let v_connect_null = take_json(rc_runtime_connect(std::ptr::null()));
        let (_, _, ui) = assert_error_envelope(&v_connect_null);
        assert_eq!(ui.code, codes::VALIDATION_INVALID_JSON);

        let v_status_uninit = take_json(rc_signal_status(std::ptr::null()));
        let (_, _, ui) = assert_error_envelope(&v_status_uninit);
        assert!(
            ui.code == codes::SERVICE_RUNTIME_NOT_INITIALIZED
                || ui.code == codes::SIGNAL_NOT_STARTED
        );

        let sig_err = FfiError::from_signal_error("连接请求: peer_offline", "peer_offline");
        let sig_val = sig_err.to_json_value();
        let (_, kind, ui) = assert_error_envelope(&sig_val);
        assert_eq!(kind, "other");
        assert_eq!(ui.code, codes::SIGNAL_PEER_OFFLINE);

        let bad_id_err = FfiError::from_signal_error(
            "连接请求: 目标设备码格式无效，请输入 9 位数字设备码",
            "目标设备码格式无效，请输入 9 位数字设备码",
        );
        let (_, _, ui) = assert_error_envelope(&bad_id_err.to_json_value());
        assert_eq!(ui.code, codes::VALIDATION_INVALID_INPUT);

        // ServerStatusSnapshot: macOS ui_error is normalized to iOS copy
        let mac_ui = resolve_ui_error(
            codes::AUTH_TOKEN_REUSE_DETECTED,
            ErrorPlatform::Macos,
            &[],
            None,
            Some("replayed".to_string()),
        );
        let snap = ServerStatusSnapshot {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            state: ServerConnectionState::Error,
            device_id: None,
            device_id_formatted: None,
            error_message: Some("token_reuse_detected".to_string()),
            ui_error: Some(mac_ui),
            signal_addr: None,
            relay_addr: None,
            revision: 1,
        };
        let normalized = normalize_server_status_for_ios(snap);
        let normalized_json = serde_json::to_value(&normalized).unwrap();
        let snap_ui = assert_rc_ui_error_schema(&normalized_json["uiError"]);
        assert_eq!(snap_ui.code, codes::AUTH_TOKEN_REUSE_DETECTED);
        assert!(snap_ui.suggestion.contains("钥匙串"));

        // ServerStatusSnapshot: ui_error=None with error_message gets classified for iOS
        let snap_legacy = ServerStatusSnapshot {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            state: ServerConnectionState::Error,
            device_id: None,
            device_id_formatted: None,
            error_message: Some("目标设备离线".to_string()),
            ui_error: None,
            signal_addr: None,
            relay_addr: None,
            revision: 2,
        };
        let normalized_legacy = normalize_server_status_for_ios(snap_legacy);
        let legacy_ui = normalized_legacy
            .ui_error
            .expect("error_message should produce ui_error");
        assert_eq!(legacy_ui.code, codes::SIGNAL_PEER_OFFLINE);

        // ServerStatusSnapshot: iOS Keychain expired token misclassified upstream as network.unreachable
        // must be refined to auth.relogin_required
        for msg in [
            "获取信令认证令牌失败：请刷新钥匙串登录态",
            "刷新失效信令认证令牌失败：请刷新钥匙串登录态",
            "获取信令认证令牌失败：本地令牌已失效",
        ] {
            let fallback_net = resolve_ui_error(
                codes::NETWORK_UNREACHABLE,
                ErrorPlatform::Ios,
                &[],
                None,
                Some(msg.to_string()),
            );
            let snap_keychain = ServerStatusSnapshot {
                enabled: true,
                server_host: "127.0.0.1".to_string(),
                signal_port: 21116,
                relay_port: 21117,
                state: ServerConnectionState::Error,
                device_id: None,
                device_id_formatted: None,
                error_message: Some(msg.to_string()),
                ui_error: Some(fallback_net),
                signal_addr: None,
                relay_addr: None,
                revision: 3,
            };
            let refined = normalize_server_status_for_ios(snap_keychain);
            let refined_ui = refined
                .ui_error
                .expect("keychain token error must produce ui_error");
            assert_eq!(
                refined_ui.code,
                codes::AUTH_RELOGIN_REQUIRED,
                "unexpected code for '{msg}'"
            );
        }

        // More specific signal error inside token refresh message must not be downgraded
        let token_reuse_refresh =
            classify_signal_error_for_ios("刷新失效信令认证令牌失败：token_reuse_detected");
        assert_eq!(token_reuse_refresh.code, codes::AUTH_TOKEN_REUSE_DETECTED);
        assert!(token_reuse_refresh.suggestion.contains("钥匙串"));

        // Whitespace-only error_message with ui_error=None must remain None
        let snap_blank = ServerStatusSnapshot {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            state: ServerConnectionState::Connected,
            device_id: None,
            device_id_formatted: None,
            error_message: Some("   \t ".to_string()),
            ui_error: None,
            signal_addr: None,
            relay_addr: None,
            revision: 4,
        };
        assert_eq!(normalize_server_status_for_ios(snap_blank).ui_error, None);
    }

    #[test]
    fn session_and_tools_error_envelopes_and_tool_permissions_api() {
        // Nonexistent session error envelopes across session & tools FFI
        let missing_sid = 987_654_321u64;
        let v_perms_missing = take_json(rc_session_tool_permissions_json(missing_sid));
        let (_, kind, ui) = assert_error_envelope(&v_perms_missing);
        assert_eq!(kind, "other");
        assert_eq!(ui.code, codes::SESSION_NOT_FOUND);

        let poll_req = CString::new(format!(r#"{{"session_id":{missing_sid}}}"#)).unwrap();
        let v_poll_missing = take_json(rc_session_poll(poll_req.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_poll_missing);
        assert_eq!(ui.code, codes::SESSION_NOT_FOUND);

        let cmd_req = CString::new(format!(
            r#"{{"session_id":{missing_sid},"command":{{"type":"disconnect"}}}}"#
        ))
        .unwrap();
        let v_cmd_missing = take_json(rc_session_command(cmd_req.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_cmd_missing);
        assert_eq!(ui.code, codes::SESSION_NOT_FOUND);

        let bad_input_req = CString::new(format!(
            r#"{{"session_id":{missing_sid},"kind":"mouse_button","button":"invalid"}}"#
        ))
        .unwrap();
        let v_input_bad = take_json(rc_session_input(bad_input_req.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_input_bad);
        assert_eq!(ui.code, codes::VALIDATION_INVALID_INPUT);

        let bad_open_req = CString::new(r#"{"connect_addr":"not-a-socket-addr"}"#).unwrap();
        let v_open_bad = take_json(rc_session_open(bad_open_req.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_open_bad);
        assert_eq!(ui.code, codes::VALIDATION_INVALID_INPUT);

        let v_tools_poll_missing = take_json(rc_tools_poll(missing_sid));
        let (_, kind, ui) = assert_error_envelope(&v_tools_poll_missing);
        assert_eq!(kind, "other");
        assert_eq!(ui.code, codes::SESSION_NOT_FOUND);

        let tools_call_req = CString::new(r#"{"op":"file_list_remote","path":null}"#).unwrap();
        let v_tools_call_missing = take_json(rc_tools_call(missing_sid, tools_call_req.as_ptr()));
        let (_, _, ui) = assert_error_envelope(&v_tools_call_missing);
        assert_eq!(ui.code, codes::SESSION_NOT_FOUND);

        // Open a session, verify rc_session_tool_permissions_json & rc_session_poll shape, then close
        let open_req = CString::new(r#"{"connect_addr":"127.0.0.1:1"}"#).unwrap();
        let open_val = take_json(rc_session_open(open_req.as_ptr()));
        let sid = open_val["session_id"]
            .as_u64()
            .expect("rc_session_open must return session_id");

        let perms_val = take_json(rc_session_tool_permissions_json(sid));
        assert_eq!(perms_val["allow_file_transfer"], serde_json::json!(true));
        assert_eq!(perms_val["allow_terminal"], serde_json::json!(true));
        assert_eq!(perms_val["allow_port_map"], serde_json::json!(true));

        let sid_json = CString::new(format!(r#"{{"session_id":{sid}}}"#)).unwrap();
        let poll_val = take_json(rc_session_poll(sid_json.as_ptr()));
        assert!(poll_val["updates"].is_array());
        assert!(poll_val["running"].is_boolean());
        assert_eq!(
            poll_val["tool_permissions"]["allow_file_transfer"],
            serde_json::json!(true)
        );

        let close_val = take_json(rc_session_close(sid_json.as_ptr()));
        assert_eq!(close_val["ok"], serde_json::json!(true));
    }

    #[test]
    fn session_update_tool_permissions_and_ended_json_serialization() {
        // 1) ToolPermissionsUpdated
        let perms = ToolPermissions {
            allow_file_transfer: false,
            allow_terminal: true,
            allow_port_map: true,
        };
        let perms_update =
            normalize_session_update_for_ios(SessionUpdate::ToolPermissionsUpdated(perms));
        let perms_json = serde_json::to_value(&perms_update).unwrap();
        assert_eq!(perms_json["kind"], "tool_permissions_updated");
        assert_eq!(perms_json["data"]["allow_file_transfer"], false);
        assert_eq!(perms_json["data"]["allow_terminal"], true);
        assert_eq!(perms_json["data"]["allow_port_map"], true);

        // 2) ToolPermissionDenied
        let denied = SessionUpdate::ToolPermissionDenied {
            channel: TOOL_CHANNEL_FILE,
            ui_error: tool_forbidden_ui_error(
                TOOL_CHANNEL_FILE,
                ErrorPlatform::Macos,
                Some("host disabled assist file transfer".to_string()),
            ),
        };
        let denied_norm = normalize_session_update_for_ios(denied);
        let denied_json = serde_json::to_value(&denied_norm).unwrap();
        assert_eq!(denied_json["kind"], "tool_permission_denied");
        assert_eq!(denied_json["data"]["channel"], u64::from(TOOL_CHANNEL_FILE));
        let denied_ui = assert_rc_ui_error_schema(&denied_json["data"]["ui_error"]);
        assert_eq!(denied_ui.code, codes::TOOL_FORBIDDEN);
        assert!(
            denied_ui.message.contains("文件传输"),
            "tool.forbidden message should interpolate tool_name, got: {}",
            denied_ui.message
        );
        assert_eq!(
            denied_ui.detail.as_deref(),
            Some("host disabled assist file transfer")
        );

        // 3) Ended with legacy ui_error=None gets populated with iOS UiError
        let ended_none = SessionUpdate::Ended {
            reason: "鉴权失败：访问密码错误".to_string(),
            recoverable: false,
            ui_error: None,
        };
        let ended_none_json =
            serde_json::to_value(normalize_session_update_for_ios(ended_none)).unwrap();
        assert_eq!(ended_none_json["kind"], "ended");
        assert_eq!(ended_none_json["data"]["recoverable"], false);
        let ended_ui = assert_rc_ui_error_schema(&ended_none_json["data"]["ui_error"]);
        assert_eq!(ended_ui.code, codes::SESSION_AUTH_REJECTED);

        // 4) Ended with macOS ui_error gets re-normalized to iOS override copy
        let ended_mac = SessionUpdate::Ended {
            reason: "token_reuse_detected".to_string(),
            recoverable: false,
            ui_error: Some(resolve_ui_error(
                codes::AUTH_TOKEN_REUSE_DETECTED,
                ErrorPlatform::Macos,
                &[],
                None,
                Some("replayed".to_string()),
            )),
        };
        let ended_ios_json =
            serde_json::to_value(normalize_session_update_for_ios(ended_mac)).unwrap();
        let ended_ios_ui = assert_rc_ui_error_schema(&ended_ios_json["data"]["ui_error"]);
        assert_eq!(ended_ios_ui.code, codes::AUTH_TOKEN_REUSE_DETECTED);
        assert!(ended_ios_ui.suggestion.contains("钥匙串"));
    }
}
