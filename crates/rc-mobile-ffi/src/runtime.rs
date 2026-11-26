//! 移动端运行时 FFI:认证会话管理 + 信令连接 + 一键设备连接。
//!
//! 分层:`auth.rs` 是无状态 HTTP 包装;本模块是有状态运行时——
//! 包装 core `AuthSessionManager`(令牌持久化/自动刷新/设备证书签发)
//! 与 `SignalClientHandle`(信令),把「输入 9 位设备码建立远控会话」
//! 的整条链(证书确保 → 信令请求 → 路由三阶 → mTLS/免密票据)
//! 压缩为一次 `rc_runtime_connect` 调用。
//!
//! 全部函数阻塞式,移动端必须在后台线程调用。

use std::collections::BTreeMap;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rc_client_core::auth_client::{
    parse_rfc3339_unix_sec, AuthClientError, AuthHttpClient, AuthSessionManager, BindDeviceRequest,
};
use rc_client_core::cert_manager::{CertManager, DeviceCertMetadata};
use rc_client_core::proxy::{active_proxy_config, set_global_proxy_config, ProxyConfig};
use rc_client_core::relay_transport::SessionMtlsBundle;
use rc_client_core::server_config::{ServerConfig, ServerStatusSnapshot};
use rc_client_core::session_bridge::{SessionHandle, UiSessionConfig};
use rc_client_core::signal_client::{SignalClientHandle, SignalClientOptions, WolAckResult};
use rc_client_core::ui_error::{codes, IntoUiError, UiError};
use serde::Deserialize;

use crate::ui_error::{
    classify_signal_error_for_ios, normalize_ui_error_for_ios, FfiError, MOBILE_ERROR_PLATFORM,
};
use crate::{read_c_str, to_c_string};

struct Runtime {
    auth: Arc<AuthSessionManager>,
    external_auth: bool,
    cert_manager: Arc<CertManager>,
    signal: Mutex<Option<Arc<SignalClientHandle>>>,
}

fn runtime() -> &'static Mutex<Option<Runtime>> {
    static RT: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();
    RT.get_or_init(|| Mutex::new(None))
}

fn lock_rt() -> std::sync::MutexGuard<'static, Option<Runtime>> {
    runtime().lock().unwrap_or_else(|e| e.into_inner())
}

fn parse<'a, T: Deserialize<'a>>(ptr: *const c_char) -> Result<T, FfiError> {
    let Some(s) = read_c_str(ptr) else {
        return Err(FfiError::invalid_json("缺少 JSON 参数"));
    };
    serde_json::from_str(s).map_err(|e| FfiError::invalid_json(format!("JSON 参数解析失败: {e}")))
}

fn with_runtime(f: impl FnOnce(&Runtime) -> Result<serde_json::Value, FfiError>) -> *mut c_char {
    let guard = lock_rt();
    let Some(rt) = guard.as_ref() else {
        return FfiError::runtime_not_initialized("运行时尚未初始化,请先调用 rc_runtime_init")
            .into_c_string();
    };
    match f(rt) {
        Ok(v) => to_c_string(v.to_string()),
        Err(e) => e.into_c_string(),
    }
}

pub(crate) fn normalize_server_status_for_ios(
    mut snap: ServerStatusSnapshot,
) -> ServerStatusSnapshot {
    let non_empty_err = snap
        .error_message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    snap.ui_error = match snap.ui_error {
        Some(ui) => {
            let normalized = normalize_ui_error_for_ios(ui);
            if normalized.code == codes::NETWORK_UNREACHABLE {
                if let Some(msg) = non_empty_err {
                    let refined = classify_signal_error_for_ios(msg);
                    if refined.code != codes::NETWORK_UNREACHABLE {
                        Some(refined)
                    } else {
                        Some(normalized)
                    }
                } else {
                    Some(normalized)
                }
            } else {
                Some(normalized)
            }
        }
        None => non_empty_err.map(classify_signal_error_for_ios),
    };
    snap
}

// ── 初始化 ──

#[derive(Deserialize)]
struct InitRequest {
    /// The app's Keychain owns token persistence and refresh.
    #[serde(default)]
    external_auth: bool,
    /// 服务端主机(IP 或域名)。
    server_host: String,
    #[serde(default)]
    use_https: bool,
    #[serde(default = "default_signal_port")]
    signal_port: u16,
    #[serde(default = "default_relay_port")]
    relay_port: u16,
    #[serde(default = "default_api_port")]
    api_port: u16,
    /// 数据目录(存放设备密钥/证书/会话持久化);iOS 传 App 容器内路径。
    data_dir: String,
    /// 服务端 CA PEM(HTTPS/mTLS 校验);可由 `rc_server_probe` 预取。
    #[serde(default)]
    ca_cert_pem: Option<String>,
    #[serde(default)]
    ca_fingerprint_sha256: Option<String>,
    #[serde(default)]
    server_auth_pubkey_hex: Option<String>,
}

fn default_signal_port() -> u16 {
    21116
}
fn default_relay_port() -> u16 {
    21117
}
fn default_api_port() -> u16 {
    21118
}

fn now_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn access_token_for_signal(auth: &AuthSessionManager, external: bool) -> Result<String, String> {
    if external {
        auth.current_session()
            .filter(|s| s.access_expires_at_unix_sec > now_sec())
            .map(|s| s.access_token)
            .ok_or_else(|| "请刷新钥匙串登录态".into())
    } else {
        auth.ensure_valid_access_token().map_err(|e| e.to_string())
    }
}

fn access_token_ffi(auth: &AuthSessionManager, external: bool) -> Result<String, FfiError> {
    if external {
        auth.current_session()
            .filter(|s| s.access_expires_at_unix_sec > now_sec())
            .map(|s| s.access_token)
            .ok_or_else(|| {
                FfiError::resolved_with_kind(
                    codes::AUTH_RELOGIN_REQUIRED,
                    "unauthorized",
                    "请刷新钥匙串登录态",
                )
            })
    } else {
        auth.ensure_valid_access_token()
            .map_err(|e| FfiError::from_auth(&e))
    }
}

// The core certificate helper can rotate refresh tokens on a 401. On iOS
// Keychain owns rotation, so request the certificate with the supplied access token only.
fn ensure_external_cert(
    rt: &Runtime,
    client: &AuthHttpClient,
    token: &str,
    id: &str,
) -> Result<bool, FfiError> {
    let user = rt
        .auth
        .current_session()
        .ok_or_else(|| {
            FfiError::resolved_with_kind(codes::AUTH_UNAUTHORIZED, "unauthorized", "未登录")
        })?
        .user_id;
    if rt.cert_manager.needs_renewal_for(now_sec(), &user, id) {
        let csr = rt
            .cert_manager
            .generate_csr_pem(Some(&format!("rc-device-{id}")))
            .map_err(|e| FfiError::resolved(codes::PKI_INVALID_CSR, e))?;
        let cert = client
            .issue_device_cert(token, id, &csr)
            .map_err(|e| FfiError::from_auth(&e))?;
        let expiry = cert
            .expires_at_unix_sec
            .or_else(|| parse_rfc3339_unix_sec(&cert.expires_at));
        if cert.device_id != id || cert.user_id != user || !expiry.is_some_and(|t| t > now_sec()) {
            return Err(FfiError::resolved(
                codes::PKI_ERROR,
                "设备证书身份或有效期不符",
            ));
        }
        rt.cert_manager
            .save_device_cert(
                &cert.cert_pem,
                &DeviceCertMetadata {
                    serial_hex: cert.serial_hex,
                    device_id: cert.device_id,
                    user_id: cert.user_id,
                    expires_at_unix_sec: expiry.unwrap(),
                    issued_at_unix_sec: parse_rfc3339_unix_sec(&cert.issued_at).unwrap_or_default(),
                    fingerprint_sha256: cert.fingerprint_sha256,
                },
            )
            .map_err(|e| FfiError::resolved(codes::PKI_ERROR, e))?;
    }
    Ok(true)
}

/// 初始化运行时(幂等:重复调用重建)。返回 `{"device_pubkey_hex": ...}`。
#[no_mangle]
pub extern "C" fn rc_runtime_init(json: *const c_char) -> *mut c_char {
    let result: Result<serde_json::Value, FfiError> = (|| {
        let req: InitRequest = parse(json)?;
        let data_dir = PathBuf::from(&req.data_dir);
        let certs_dir = data_dir.join("certs");
        let auth_path = data_dir.join("auth_session.json");
        let cert_manager =
            Arc::new(CertManager::load_or_init(certs_dir).map_err(|e| {
                FfiError::resolved(codes::PKI_ERROR, format!("证书管理器初始化: {e}"))
            })?);
        let mut cfg = ServerConfig {
            enabled: true,
            server_host: req.server_host,
            signal_port: req.signal_port,
            relay_port: req.relay_port,
            api_port: req.api_port,
            use_https: req.use_https,
            ca_fingerprint_sha256: req.ca_fingerprint_sha256,
            ca_cert_pem: req.ca_cert_pem,
            server_auth_pubkey_hex: req.server_auth_pubkey_hex,
            ..ServerConfig::default()
        };
        if req.external_auth {
            cfg.device_key = cert_manager.device_pubkey_hex();
            rc_client_core::auth_client::delete_auth_session(&auth_path)
                .map_err(FfiError::invalid_config)?;
        }
        let auth = AuthSessionManager::new(cfg, auth_path, Arc::clone(&cert_manager));
        if !req.external_auth {
            let _ = auth.restore_on_startup();
        }
        let pubkey = cert_manager.device_pubkey_hex();
        let old_signal = {
            let mut guard = lock_rt();
            let prev_rt = guard.replace(Runtime {
                auth: Arc::new(auth),
                external_auth: req.external_auth,
                cert_manager,
                signal: Mutex::new(None),
            });
            prev_rt.and_then(|old_rt| {
                old_rt
                    .signal
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
            })
        };
        if let Some(old) = old_signal {
            old.request_stop();
            drop(old);
        }
        Ok(serde_json::json!({ "device_pubkey_hex": pubkey }))
    })();
    match result {
        Ok(v) => to_c_string(v.to_string()),
        Err(e) => e.into_c_string(),
    }
}

// ── 登录 / 状态 / 登出 ──

#[derive(Deserialize)]
struct LoginReq {
    email: String,
    password: String,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    os: Option<String>,
}

fn session_json(rt: &Runtime) -> serde_json::Value {
    match rt.auth.current_session() {
        Some(s) => serde_json::json!({
            "logged_in": true,
            "user_id": s.user_id,
            "email": s.email,
            "display_name": s.display_name,
            "must_change_password": s.must_change_password,
        }),
        None => serde_json::json!({ "logged_in": false }),
    }
}

/// 登录(自动绑定本机设备)。输入 `{email, password, hostname?, os?}`。
#[no_mangle]
pub extern "C" fn rc_runtime_login(json: *const c_char) -> *mut c_char {
    let req: LoginReq = match parse(json) {
        Ok(r) => r,
        Err(e) => return e.into_c_string(),
    };
    with_runtime(|rt| {
        if rt.external_auth {
            return Err(FfiError::invalid_config("此客户端由系统钥匙串提供登录态"));
        }
        rt.auth
            .login(
                &req.email,
                &req.password,
                req.hostname.as_deref(),
                req.os.as_deref(),
            )
            .map_err(|e: AuthClientError| FfiError::from_auth(&e))?;
        Ok(session_json(rt))
    })
}

/// Adopt a short-lived token in memory only. Refresh credentials never enter this runtime.
#[no_mangle]
pub extern "C" fn rc_runtime_set_session(json: *const c_char) -> *mut c_char {
    let session: rc_client_core::auth_client::StoredAuthSession = match parse(json) {
        Ok(session) => session,
        Err(error) => return error.into_c_string(),
    };
    with_runtime(move |rt| {
        let cfg = rt.auth.server_config();
        if !rt.external_auth
            || !session.refresh_token.is_empty()
            || session.user_id.is_empty()
            || session.access_token.is_empty()
            || session.server_host != cfg.server_host
            || session.api_port != cfg.api_port
        {
            return Err(FfiError::invalid_config("登录态与当前运行时不匹配"));
        }
        rt.auth.apply_external_session(session);
        Ok(serde_json::json!({"ok": true}))
    })
}

/// 当前会话状态:`{logged_in, user_id?, email?, ...}`。
#[no_mangle]
pub extern "C" fn rc_runtime_status() -> *mut c_char {
    with_runtime(|rt| Ok(session_json(rt)))
}

/// 登出并清除本地凭据。
#[no_mangle]
pub extern "C" fn rc_runtime_logout() -> *mut c_char {
    with_runtime(|rt| {
        if rt.external_auth {
            rt.auth.wipe_local_credentials();
        } else {
            rt.auth.logout(None).map_err(|e| FfiError::from_auth(&e))?;
        }
        Ok(serde_json::json!({ "ok": true }))
    })
}

// ── 信令 ──

/// 启动信令连接:绑定/确认本机设备 → 确保设备证书 → 拉起信令客户端。
/// 输入 `{}`;返回 `{"device_id": "...", "cert_ready": bool}`。
#[no_mangle]
pub extern "C" fn rc_signal_start(json: *const c_char) -> *mut c_char {
    #[derive(Deserialize)]
    struct Req {
        #[serde(default)]
        hostname: Option<String>,
        #[serde(default)]
        os: Option<String>,
        #[serde(default)]
        lan_addrs: Option<Vec<String>>,
    }
    let req: Req = parse(json).unwrap_or(Req {
        hostname: None,
        os: None,
        lan_addrs: None,
    });
    with_runtime(|rt| {
        let token = access_token_ffi(&rt.auth, rt.external_auth)?;
        let cfg = rt.auth.server_config();
        let client = rc_client_core::auth_client::AuthHttpClient::from_server_config(&cfg)
            .map_err(|e| FfiError::from_auth(&e))?;
        // 绑定(幂等):同 device_key 重绑返回同一设备
        let bound = client
            .bind_device(
                &token,
                &BindDeviceRequest {
                    device_key: cfg.device_key.clone(),
                    hostname: req.hostname.clone(),
                    os: req.os.clone(),
                    alias: None,
                    device_pubkey: Some(rt.cert_manager.device_pubkey_hex()),
                    allow_remote_assist: None,
                    app_version: Some(rc_client_core::platform::current_app_version()),
                    arch: Some(rc_client_core::platform::current_app_arch().to_string()),
                },
            )
            .map_err(|e| FfiError::from_auth_with_message(format!("设备绑定: {e}"), &e))?;
        let cert_ready = if rt.external_auth {
            ensure_external_cert(rt, &client, &token, &bound.device_id)?
        } else {
            rt.auth
                .ensure_device_cert(&bound.device_id)
                .map_err(|e| FfiError::from_auth_with_message(format!("设备证书: {e}"), &e))?
        };
        // 拉起信令(已运行则先显式触发停止，避免在途 Arc 克隆挂起或撞 duplicate_login)
        let old_signal = {
            let mut slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
            slot.take()
        };
        if let Some(old) = old_signal {
            old.request_stop();
            drop(old);
        }
        {
            let hostname = req
                .hostname
                .clone()
                .unwrap_or_else(|| "ios-device".to_string());
            let auth_for_token = Arc::clone(&rt.auth);
            let external = rt.external_auth;
            let token_provider: rc_client_core::signal_client::TokenProvider =
                Arc::new(move || access_token_for_signal(&auth_for_token, external));
            let auth_for_refresh = Arc::clone(&rt.auth);
            let token_refresher: rc_client_core::signal_client::TokenRefresher =
                Arc::new(move |stale| {
                    if external {
                        access_token_for_signal(&auth_for_refresh, true)
                    } else {
                        auth_for_refresh
                            .refresh_after_unauthorized(stale)
                            .map_err(|e| format!("{e:?}"))
                    }
                });
            let handle = SignalClientHandle::spawn_with_options(
                cfg.clone(),
                hostname,
                req.lan_addrs.clone().unwrap_or_default(),
                SignalClientOptions {
                    token_provider: Some(token_provider),
                    token_refresher: Some(token_refresher),
                    device_pubkey: Some(rt.cert_manager.device_pubkey_hex()),
                    allow_remote_assist: false,
                    allow_remote_control: Some(false),
                    dynamic_lan_discovery: true,
                    upnp_enabled: false,
                },
                |_| {},
            );
            let mut slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(Arc::new(handle));
        }
        Ok(serde_json::json!({
            "device_id": bound.device_id,
            "formatted_id": bound.formatted_id,
            "cert_ready": cert_ready,
        }))
    })
}

/// 停止信令连接（显式通知停止后再释放，避免在途阻塞请求持有的 `Arc` 挂起或滞留旧连接）。
#[no_mangle]
pub extern "C" fn rc_signal_stop() -> *mut c_char {
    let old_signal = {
        let guard = lock_rt();
        let Some(rt) = guard.as_ref() else {
            return FfiError::runtime_not_initialized("运行时尚未初始化,请先调用 rc_runtime_init")
                .into_c_string();
        };
        let mut slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    if let Some(old) = old_signal {
        old.request_stop();
        drop(old);
    }
    to_c_string(serde_json::json!({ "ok": true }).to_string())
}

// ── 一键连接设备 ──

#[derive(Deserialize)]
struct ConnectReq {
    /// 目标 9 位设备码(允许带空格格式化)。
    target_id: String,
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
    /// LAN 直连地址(调用方从局域网发现匹配得到,可选加速)。
    #[serde(default)]
    lan_direct_addr: Option<std::net::SocketAddr>,
    #[serde(default = "default_timeout")]
    timeout_sec: u64,
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
fn default_timeout() -> u64 {
    15
}

const DEFAULT_WOL_TIMEOUT_MS: u32 = 10_000;
const MAX_WOL_TIMEOUT_MS: u32 = 60_000;

/// 钳位 Wake-on-LAN 等待超时毫秒数：`0` 回退至 `10_000ms`，大于 `60_000ms` 钳位至 `60_000ms`。
pub(crate) fn effective_wol_timeout_ms(timeout_ms: u32) -> u32 {
    if timeout_ms == 0 {
        DEFAULT_WOL_TIMEOUT_MS
    } else {
        timeout_ms.min(MAX_WOL_TIMEOUT_MS)
    }
}

fn ui_error_kind(ui_error: &UiError) -> &'static str {
    if ui_error.code == codes::AUTH_TOKEN_REUSE_DETECTED {
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
    }
}

pub(crate) fn normalize_wol_ack_for_ios(mut ack: WolAckResult) -> WolAckResult {
    ack.ui_error = ack.ui_error.map(normalize_ui_error_for_ios);
    ack
}

#[cfg(test)]
pub(crate) fn install_test_signal_for_unit_test(
    handle: SignalClientHandle,
) -> Arc<SignalClientHandle> {
    let arc = Arc::new(handle);
    let old_signal = {
        let guard = lock_rt();
        let rt = guard
            .as_ref()
            .expect("install_test_signal_for_unit_test requires rc_runtime_init first");
        let mut slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
        slot.replace(Arc::clone(&arc))
    };
    if let Some(old) = old_signal {
        old.request_stop();
        drop(old);
    }
    arc
}

// ── 出站网络代理（进程级全局配置，可在 rc_runtime_init 前调用）──

/// 更新进程级活跃出站代理配置（`ProxyConfig` JSON，camelCase 字段）。
///
/// - 反序列化并调用 `ProxyConfig::validate()` 严格校验；
/// - 校验通过后对比 `active_proxy_config() != cfg` 再写入
///   `rc_client_core::proxy::set_global_proxy_config(...)`；若配置发生变更且存在运行中的
///   `SignalClientHandle`，释放全部锁后自动触发快速重连以切换至新代理路径；
/// - 校验或解析失败时返回标准 iOS `ui_error` 信封，绝不在任何返回或错误字段中泄露代理密码；
/// - 调用方必须经 `rc_string_free` 释放返回字符串。
#[no_mangle]
pub extern "C" fn rc_proxy_set_config_json(config_json: *const c_char) -> *mut c_char {
    let Some(raw) = read_c_str(config_json) else {
        return FfiError::invalid_json("缺少代理配置 JSON 参数").into_c_string();
    };
    let cfg: ProxyConfig = match serde_json::from_str(raw) {
        Ok(cfg) => cfg,
        // 不回显 serde_json 原始 token，防止格式错误 JSON 中的代理密码明文进入错误信封。
        Err(_) => return FfiError::invalid_json("代理配置 JSON 格式无效").into_c_string(),
    };
    if let Err(err) = cfg.validate() {
        let ui_error = err.to_ui_error(MOBILE_ERROR_PLATFORM);
        return FfiError::from_ui_error(err.to_string(), "other", ui_error).into_c_string();
    }
    let changed = active_proxy_config() != cfg;
    set_global_proxy_config(cfg);
    if changed {
        let active_signal = {
            let guard = lock_rt();
            guard.as_ref().and_then(|rt| {
                let slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
                slot.as_ref().map(Arc::clone)
            })
        };
        if let Some(handle) = active_signal {
            handle.trigger_reconnect("出站代理配置变更");
        }
    }
    to_c_string(serde_json::json!({ "ok": true }).to_string())
}

/// 查询当前信令状态快照（含 `uiError` 结构化模型）。
#[no_mangle]
pub extern "C" fn rc_signal_status(_json: *const c_char) -> *mut c_char {
    let guard = lock_rt();
    let Some(rt) = guard.as_ref() else {
        return FfiError::runtime_not_initialized("运行时未初始化").into_c_string();
    };
    let slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some(handle) => {
            let snap = normalize_server_status_for_ios(handle.status());
            to_c_string(serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into()))
        }
        None => FfiError::signal_not_started("信令未启动").into_c_string(),
    }
}

/// 向服务端发起 Wake-on-LAN 远程开机请求（阻塞式，禁止主线程调用）。
///
/// - `target_device_id`: 目标 9 位设备码（允许带空格格式化）；
/// - `timeout_ms`: 等待 `wol_ack` 超时毫秒数（传 0 默认 10000ms，上限 60000ms）；
/// - 收到服务端应答时返回 `WolAckResult` JSON（camelCase 字段，失败态同步附带归一化 iOS `uiError`）；
/// - 入参非法、运行时未初始化、信令未启动、写帧失败或超时/断线时返回标准 iOS `ui_error` 信封；
/// - 调用方必须经 `rc_string_free` 释放返回字符串。
#[no_mangle]
pub extern "C" fn rc_signal_request_wol(
    target_device_id: *const c_char,
    timeout_ms: u32,
) -> *mut c_char {
    let Some(raw_target) = read_c_str(target_device_id) else {
        return FfiError::invalid_input("缺少目标设备码").into_c_string();
    };
    let trimmed = raw_target.trim();
    if trimmed.is_empty() {
        return FfiError::invalid_input("目标设备码不能为空").into_c_string();
    }
    let signal = {
        let guard = lock_rt();
        let Some(rt) = guard.as_ref() else {
            return FfiError::runtime_not_initialized("运行时尚未初始化,请先调用 rc_runtime_init")
                .into_c_string();
        };
        let slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
        let Some(handle) = slot.as_ref() else {
            return FfiError::signal_not_started("信令未启动,请先 rc_signal_start").into_c_string();
        };
        Arc::clone(handle)
    };
    let timeout = Duration::from_millis(u64::from(effective_wol_timeout_ms(timeout_ms)));
    match signal.request_wol(trimmed, timeout) {
        Ok(ack) => {
            let normalized = normalize_wol_ack_for_ios(ack);
            to_c_string(serde_json::to_string(&normalized).unwrap_or_else(|_| "{}".to_string()))
        }
        Err(ui_err) => {
            let ui_error = normalize_ui_error_for_ios(ui_err);
            let kind = ui_error_kind(&ui_error);
            let msg = ui_error
                .detail
                .clone()
                .unwrap_or_else(|| ui_error.message.clone());
            FfiError::from_ui_error(msg, kind, ui_error).into_c_string()
        }
    }
}

/// 返回当前已缓存的同账号设备 Wake-on-LAN 能力快照 JSON 对象（键为 9 位规范设备码，值为 bool）。
///
/// 运行时未初始化或信令未启动时返回 `"{}"`。调用方必须经 `rc_string_free` 释放返回字符串。
#[no_mangle]
pub extern "C" fn rc_signal_wol_capable_snapshot_json() -> *mut c_char {
    let snapshot: BTreeMap<String, bool> = {
        let guard = lock_rt();
        guard
            .as_ref()
            .and_then(|rt| {
                let slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
                slot.as_ref().map(|h| h.device_wol_capable_snapshot())
            })
            .unwrap_or_default()
            .into_iter()
            .collect()
    };
    to_c_string(serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_string()))
}

/// 一键连接设备:信令 request_connect → 组装路由/mTLS/免密票据 → 开会话。
/// 阻塞至拿到连接授权(默认 15s 超时)。返回 `{"session_id": N, "owner_connection": bool}`。
#[no_mangle]
pub extern "C" fn rc_runtime_connect(json: *const c_char) -> *mut c_char {
    let req: ConnectReq = match parse(json) {
        Ok(r) => r,
        Err(e) => return e.into_c_string(),
    };
    let result: Result<(u64, bool), FfiError> = (|| {
        let guard = lock_rt();
        let rt = guard
            .as_ref()
            .ok_or_else(|| FfiError::runtime_not_initialized("运行时未初始化"))?;
        let signal = {
            let slot = rt.signal.lock().unwrap_or_else(|e| e.into_inner());
            slot.as_ref()
                .map(Arc::clone)
                .ok_or_else(|| FfiError::signal_not_started("信令未启动,请先 rc_signal_start"))?
        };
        let grant = signal
            .request_connect(&req.target_id, Duration::from_secs(req.timeout_sec))
            .map_err(|e| FfiError::from_signal_error(format!("连接请求: {e}"), &e))?;

        // mTLS 证书束(加密连接;无本地证书时降级为不加密)
        let mtls = build_mtls_bundle(rt, &grant.target_id);
        if rt.external_auth && mtls.is_none() {
            return Err(FfiError::resolved(
                codes::PKI_CERT_MISSING,
                "缺少可信设备证书，已停止连接。请测试服务器连接后重新登录。",
            ));
        }
        let encrypt = mtls.is_some();

        // 同账号免密票据签名器
        let owner_signer = grant.owner_grant.as_ref().and_then(|g| {
            rt.cert_manager
                .create_owner_ticket_signer(g.clone())
                .ok()
                .map(|s| Arc::new(s) as Arc<dyn rc_session::auth::OwnerTicketSigner>)
        });
        let is_owner = grant.is_owner_connection;

        let cfg = UiSessionConfig {
            connect_addr: std::net::SocketAddr::from(([127, 0, 0, 1], 9901)), // grant 存在时被忽略
            password: req.password,
            watch_only: req.watch_only,
            width: req.width,
            height: req.height,
            fps: req.fps,
            bitrate_bps: req.bitrate_bps,
            prefer_hardware: true,
            clipboard_enabled: false,
            audio_enabled: cfg!(target_os = "ios"),
            relay_grant: Some(grant),
            encrypt_connection: encrypt,
            mtls_config: mtls,
            owner_signer,
            lan_direct_addr: req.lan_direct_addr,
            ..UiSessionConfig::default()
        };
        let handle = SessionHandle::spawn(cfg);
        let id = crate::session::register_session(handle);
        Ok((id, is_owner))
    })();
    match result {
        Ok((id, owner)) => to_c_string(
            serde_json::json!({ "session_id": id, "owner_connection": owner }).to_string(),
        ),
        Err(e) => e.into_c_string(),
    }
}

/// 按桌面端同构逻辑组装 mTLS 证书束;任一前置缺失返回 None(降级不加密)。
fn build_mtls_bundle(rt: &Runtime, peer_device_id: &str) -> Option<SessionMtlsBundle> {
    let cfg = rt.auth.server_config();
    let ca_cert_pem = cfg.ca_cert_pem.clone()?;
    let sess = rt.auth.current_session()?;
    let (cert_pem, meta) = rt.cert_manager.load_device_cert()?;
    if !meta.user_id.eq_ignore_ascii_case(sess.user_id.trim()) {
        return None;
    }
    let key_pem = rt.cert_manager.tls_key_pem().ok()?;
    Some(SessionMtlsBundle {
        ca_cert_pem,
        cert_pem,
        key_pem,
        local_user_id: meta.user_id,
        expected_peer_device_id: Some(peer_device_id.to_string()),
        expected_peer_user_id: None,
    })
}
