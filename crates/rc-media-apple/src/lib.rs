//! RabbitControl Apple 媒体库 FFI:VT 解码 + 帧泵渲染注册与帧取放。
//!
//! 架构:经 core 的平台注册接口单向接入——
//! - `AppleMediaProvider`(rc-pipeline `PlatformMediaProvider`)提供 VT 硬件解码器;
//! - `AppleMediaHooks`(rc-client-core `ClientPlatformHooks`)提供 `FramePumpRenderer`;
//! - `FramePumpRenderer` 实现 `Renderer`:每帧 CFRetain 后换入全局槽位,
//!   客户端经 `rc_apple_media_frame_take` 取走 CVPixelBuffer 指针(所有权转移),
//!   用 CoreImage/Metal 显示,用完必须调 `rc_apple_media_frame_release`。
//!
//! 限制(v1):全局单帧槽,假定同一时刻只有一路会话。

use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rc_capture::FrameTexture;
use rc_decode::DecodedFrame;
use rc_pipeline::{PipelineConfig, PlatformMediaProvider};
use rc_render::Renderer;
use rc_types::{Error, Result, Timestamp};

#[cfg(any(target_os = "ios", test))]
mod clipboard;
#[cfg(any(target_os = "ios", test))]
mod playback;

#[cfg(target_os = "ios")]
struct PlaybackProvider;

#[cfg(target_os = "ios")]
impl rc_audio::AudioBackendProvider for PlaybackProvider {
    fn create_system_audio_capture(
        &self,
    ) -> std::result::Result<Box<dyn rc_audio::AudioCapture>, String> {
        Err("controller does not capture local audio".into())
    }

    fn create_audio_player(
        &self,
        rate: u32,
        channels: u8,
    ) -> std::result::Result<Box<dyn rc_audio::AudioPlayer>, String> {
        Ok(Box::new(playback::AppleAudioPlayer::new(rate, channels)?))
    }
}

// ── CoreFoundation 引用计数(手工 FFI,避免额外依赖)──

type CFTypeRef = *const std::ffi::c_void;

extern "C" {
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

fn to_c_string(s: String) -> *mut c_char {
    match std::ffi::CString::new(s) {
        Ok(cs) => cs.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// 释放 `rc_apple_media_frame_take`/JSON 字符串缓冲区。
///
/// # Safety
/// `ptr` 必须为本库返回且尚未释放的字符串指针。
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn rc_apple_media_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            drop(std::ffi::CString::from_raw(ptr));
        }
    }
}

// ── 全局帧槽 ──

/// 零分配 C ABI 帧输出结构体（供 `rc_apple_media_frame_take_raw` 使用）。
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RcAppleFrame {
    /// `CVPixelBufferRef` 裸指针（所有权已转移，用毕必须调 `rc_apple_media_frame_release`）。
    pub ptr: u64,
    /// 帧宽（像素）。
    pub width: u32,
    /// 帧高（像素）。
    pub height: u32,
    /// 单调递增帧序号。
    pub seq: u64,
}

struct FrameSlot {
    /// 当前帧 CVPixelBuffer 指针(已 retain,槽位持有)。
    ptr: usize,
    width: u32,
    height: u32,
    /// 单调递增帧序号,客户端据此避免重复取帧。
    seq: u64,
}

static HAS_FRAME: AtomicBool = AtomicBool::new(false);

fn frame_slot() -> &'static Mutex<FrameSlot> {
    static SLOT: OnceLock<Mutex<FrameSlot>> = OnceLock::new();
    SLOT.get_or_init(|| {
        Mutex::new(FrameSlot {
            ptr: 0,
            width: 0,
            height: 0,
            seq: 0,
        })
    })
}

// ── 帧泵渲染器 ──

/// 帧泵渲染器:不直接上屏,把每帧 CVPixelBuffer retain 进全局槽,
/// 由客户端 UI 层(SwiftUI/CoreImage/Metal)取帧显示。
struct FramePumpRenderer {
    #[allow(dead_code)]
    size: (u32, u32),
}

impl Renderer for FramePumpRenderer {
    fn present(&mut self, frame: DecodedFrame) -> Result<Timestamp> {
        match frame.texture {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            FrameTexture::CVPixelBuffer(ptr) => {
                if ptr.is_null() {
                    return Err(Error::InvalidConfig("null pixel buffer".to_string()));
                }
                #[allow(unsafe_code)]
                unsafe {
                    // 跨帧持有必须自行 retain(解码器只保活到下一次 decode/recv_decoded)。
                    CFRetain(ptr as CFTypeRef);
                    let old_ptr = {
                        let mut slot = frame_slot().lock().unwrap_or_else(|e| e.into_inner());
                        let old = slot.ptr;
                        slot.ptr = ptr as usize;
                        slot.width = frame.width;
                        slot.height = frame.height;
                        slot.seq = slot.seq.wrapping_add(1);
                        HAS_FRAME.store(true, Ordering::Release);
                        old
                    };
                    // 将旧帧 CFRelease 移出互斥锁临界区，避免 IOSurface 回收阻塞 UI 取帧。
                    if old_ptr != 0 {
                        CFRelease(old_ptr as CFTypeRef);
                    }
                    FRAMES_CVPB.fetch_add(1, Ordering::Relaxed);
                }
                Ok(frame.decode_done_ts)
            }
            // CPU 帧(软件路径)暂不上屏,静默吞掉保持管线存活。
            _ => {
                FRAMES_CPU.fetch_add(1, Ordering::Relaxed);
                Ok(frame.decode_done_ts)
            }
        }
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        if size.0 == 0 || size.1 == 0 {
            return Err(Error::InvalidConfig(
                "frame pump renderer: size must be non-zero".to_string(),
            ));
        }
        self.size = size;
        Ok(())
    }

    fn set_vsync(&mut self, _on: bool) {}

    fn set_cursor(&mut self, _pos: (i32, i32), _visible: bool) {}
}

// ── 平台注册 ──

struct AppleMediaProvider;

impl PlatformMediaProvider for AppleMediaProvider {
    fn supports_hevc(&self) -> bool {
        // iOS 模拟器的 VideoToolbox 无 HEVC 硬解,默认回退 H.264;真机正常走 HEVC。
        // `RC_FORCE_HEVC=1` / `RC_DISABLE_HEVC=1` 可双向覆盖,便于调试对照。
        if std::env::var_os("RC_FORCE_HEVC").is_some() {
            return true;
        }
        if std::env::var_os("RC_DISABLE_HEVC").is_some() {
            return false;
        }
        !cfg!(target_abi = "sim")
    }

    fn create_decoder(
        &self,
        cfg: &PipelineConfig,
    ) -> Result<(Box<dyn rc_decode::Decoder>, &'static str)> {
        let decoder = rc_decode_vt::new_decoder(rc_decode_vt::VtDecoderConfig {
            codec: cfg.codec,
            max_resolution: rc_types::Resolution::new(
                cfg.resolution.width.max(4096),
                cfg.resolution.height.max(4096),
            ),
            ..rc_decode_vt::VtDecoderConfig::default()
        })
        .map_err(|e| Error::InvalidConfig(format!("vt decoder init: {e}")))?;
        Ok((Box::new(decoder), "videotoolbox"))
    }
}

struct AppleMediaHooks;

impl rc_client_core::platform::ClientPlatformHooks for AppleMediaHooks {
    fn create_hardware_renderer(
        &self,
        width: u32,
        height: u32,
        _session_start: std::time::Instant,
        _preallocated: Option<&rc_client_core::platform::PreallocatedRenderSurface>,
    ) -> std::result::Result<rc_client_core::platform::HardwareRendererOutput, String> {
        Ok(rc_client_core::platform::HardwareRendererOutput {
            renderer: Box::new(FramePumpRenderer {
                size: (width, height),
            }),
            layer_addr: 0,
            label: "frame-pump",
        })
    }
}

static REGISTERED: AtomicBool = AtomicBool::new(false);
static FRAMES_CVPB: AtomicU64 = AtomicU64::new(0);
static FRAMES_CPU: AtomicU64 = AtomicU64::new(0);
static FRAMES_TAKEN: AtomicU64 = AtomicU64::new(0);

/// 帧链路诊断计数:`{"presented_cvpb":n,"presented_cpu":n,"taken":n}`。
/// presented_cvpb=0 且 presented_cpu>0 → 解码走了软件兜底(CPU 帧被吞,黑屏根因);
/// 两者皆 0 → 网络/会话侧无帧到达。
#[no_mangle]
pub extern "C" fn rc_apple_media_stats() -> *mut c_char {
    to_c_string(
        serde_json::json!({
            "presented_cvpb": FRAMES_CVPB.load(Ordering::Relaxed),
            "presented_cpu": FRAMES_CPU.load(Ordering::Relaxed),
            "taken": FRAMES_TAKEN.load(Ordering::Relaxed),
        })
        .to_string(),
    )
}

/// 注册 Apple 媒体通路(幂等),返回 1 成功。打开会话前必须调用一次。
#[no_mangle]
pub extern "C" fn rc_apple_media_init() -> i32 {
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return 1;
    }
    let provider_ok = rc_pipeline::register_platform_provider(Arc::new(AppleMediaProvider));
    #[cfg(target_os = "ios")]
    {
        rc_audio::register_audio_provider(Arc::new(PlaybackProvider));
        clipboard::register();
    }
    let hooks_ok = rc_client_core::platform::register_platform_hooks(Arc::new(AppleMediaHooks));
    if provider_ok && hooks_ok {
        1
    } else {
        0
    }
}

/// 零分配快速取走当前最新帧(所有权转移)。
///
/// 无新帧时直接通过原子标志快退并返回 `false`（零锁、零堆分配）；
/// 有新帧时写入 `*out` 并返回 `true`，用毕必须调 `rc_apple_media_frame_release(out.ptr)`。
///
/// # Safety
/// `out` 必须为空指针（此时恒返回 `false`）或指向合法可写的 `RcAppleFrame`。
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn rc_apple_media_frame_take_raw(out: *mut RcAppleFrame) -> bool {
    if out.is_null() || !HAS_FRAME.load(Ordering::Acquire) {
        return false;
    }
    let taken = {
        let mut slot = frame_slot().lock().unwrap_or_else(|e| e.into_inner());
        if slot.ptr == 0 {
            HAS_FRAME.store(false, Ordering::Release);
            return false;
        }
        FRAMES_TAKEN.fetch_add(1, Ordering::Relaxed);
        let f = RcAppleFrame {
            ptr: slot.ptr as u64,
            width: slot.width,
            height: slot.height,
            seq: slot.seq,
        };
        slot.ptr = 0;
        HAS_FRAME.store(false, Ordering::Release);
        f
    };
    unsafe {
        out.write(taken);
    }
    true
}

/// 取走当前最新帧(所有权转移)。返回:
/// `{"ptr": u64, "width": u32, "height": u32, "seq": u64}`,无帧时 `{}`。
///
/// `ptr` 即 `CVPixelBufferRef`,可直接构造 `CIImage(cvPixelBuffer:)`;
/// 用毕必须调 `rc_apple_media_frame_release`。take 后槽位清空,同帧不会被取两次。
/// 返回的字符串用毕调 `rc_apple_media_string_free`。
#[no_mangle]
pub extern "C" fn rc_apple_media_frame_take() -> *mut c_char {
    if !HAS_FRAME.load(Ordering::Acquire) {
        return to_c_string("{}".to_string());
    }
    let out = {
        let mut slot = frame_slot().lock().unwrap_or_else(|e| e.into_inner());
        if slot.ptr == 0 {
            HAS_FRAME.store(false, Ordering::Release);
            return to_c_string("{}".to_string());
        }
        FRAMES_TAKEN.fetch_add(1, Ordering::Relaxed);
        let taken = serde_json::json!({
            "ptr": slot.ptr,
            "width": slot.width,
            "height": slot.height,
            "seq": slot.seq,
        });
        slot.ptr = 0;
        HAS_FRAME.store(false, Ordering::Release);
        taken
    };
    to_c_string(out.to_string())
}

/// 释放一帧 CVPixelBuffer(`rc_apple_media_frame_take` / `rc_apple_media_frame_take_raw` 取走的帧用毕必须调用)。
///
/// # Safety
/// `ptr` 必须为零或此前 `rc_apple_media_frame_take` / `rc_apple_media_frame_take_raw` 返回且尚未释放的帧指针;
/// 重复释放或传入非帧指针属于未定义行为。
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn rc_apple_media_frame_release(ptr: u64) {
    if ptr != 0 {
        unsafe {
            CFRelease(ptr as CFTypeRef);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_is_idempotent() {
        assert_eq!(super::rc_apple_media_init(), 1);
        assert_eq!(super::rc_apple_media_init(), 1);
    }

    #[test]
    fn frame_take_empty_returns_empty_object() {
        // 测试进程内槽位可能已被其他测试污染,仅校验返回非空合法 JSON。
        let ptr = super::rc_apple_media_frame_take();
        assert!(!ptr.is_null());
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let _: serde_json::Value = serde_json::from_str(&s).unwrap();
        unsafe { super::rc_apple_media_string_free(ptr) };
    }

    #[test]
    fn frame_take_raw_handles_null_and_empty() {
        unsafe {
            assert!(!super::rc_apple_media_frame_take_raw(std::ptr::null_mut()));
            let mut out = super::RcAppleFrame::default();
            // 先排空可能存在的残留，再断言空槽返回 false。
            while super::rc_apple_media_frame_take_raw(&mut out) {
                super::rc_apple_media_frame_release(out.ptr);
            }
            assert!(!super::rc_apple_media_frame_take_raw(&mut out));
        }
    }
}
