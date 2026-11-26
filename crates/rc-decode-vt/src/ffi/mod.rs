//! VideoToolbox 解码侧的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**：
//! - `VTDecompressionSession` 的 C API（Create / SetProperty / DecodeFrame /
//!   WaitForAsynchronousFrames / Invalidate）在这里声明与包装；
//! - 解码输出回调（`VTDecompressionOutputCallback`）在这里定义：只做
//!   retain + 按帧号投递邮箱，不做重活、绝不 panic；
//! - Annex-B → AVCC 之后的装帧（`CMBlockBuffer` → `CMSampleBuffer`）与
//!   SPS/PPS → `CMVideoFormatDescription` 在等待线程上完成；
//! - 测试专用的像素缓冲写入/回读（LockBaseAddress 只出现在测试支撑里，
//!   生产解码路径**零 CPU 回读**，D1）。
//!
//! 对外（`lib.rs`）只暴露安全的包装类型与函数。
//!
//! # 关键落死项
//! | 项 | 处理 |
//! |---|---|
//! | 会话建立 | **惰性**：首个带 SPS/PPS 的 IDR 到达才建会话（之前无从构建格式描述）；SPS 变化（分辨率切换）→ 重建 |
//! | 输入封装 | AVCC：4 字节大端长度前缀，参数集经格式描述传递（转换在 [`crate::h264`]，纯函数） |
//! | D3 低延迟 | `kVTDecompressionPropertyKey_RealTime = true`（提示解码器按实时优化；仅容忍 `kVTPropertyNotSupportedErr`） |
//! | D5 顺序 | 严格单帧在途 + 帧号邮箱匹配（[`crate::mailbox`]），提交线程等到本帧回调才返回 |
//! | 解码器选择 | `decoder_specification = null`（系统默认：优先硬件、软件兜底）。与编码侧「RequireHardwareAccelerated = true」刻意不同：编码落回软编会拖垮整条链路、必须在构造期失败；解码端硬件 H.264 不可用时软件兜底仍可工作，且解码器选择没有可靠的查询接口，硬性要求只会把「能解」变「不能解」 |
//! | 输出缓冲保活 | 输出 `CVPixelBuffer` 在回调里 retain(+1)，由会话持有到**下一次 decode / 会话销毁**——与 `FrameTexture` 文档的采集侧句柄生命周期语义对称（下一次采集前有效），零拷贝交给渲染器 |
//!
//! # 输入格式说明（坑 #7）
//! VideoToolbox 解码输入是 AVCC 风格（长度前缀 NAL + 参数集在格式描述里）。
//! 管线统一消费 Annex-B（`rc-encode-vt` 输出侧已重排并在 caps 里声明），
//! 因此解码端在喂给 VT 之前由 [`crate::h264`] 做一次 Annex-B → AVCC 转换。
//! 方向与编码侧（`rc-encode-vt/src/ffi` 的 E6 解码验证）相反，装帧代码
//! 结构对称，含同一个 `CMBlockBuffer` 分配器的双重释放坑（见
//! [`DecompressionSession::decode_frame`] 内注释）。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFData,
    CFDictionary, CFRetained, CFString,
};
use objc2_core_media::{
    kCMBlockBufferAlwaysCopyDataFlag,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, CMBlockBuffer,
    CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};

use crate::mailbox::FrameMailbox;

/// `OSStatus`（CFBase.h：`typedef int32_t OSStatus`）。core-media 绑定把它
/// 保持为 crate 私有，这里自行定义同名别名（与 rc-encode-vt 的 ffi 一致）。
pub(crate) type OSStatus = i32;

/// `kCMVideoCodecType_AV1 = 'av01'`（CoreMedia/CMFormatDescription.h）。
const K_CM_VIDEO_CODEC_TYPE_AV1: u32 = u32::from_be_bytes(*b"av01");

/// `kVTPropertyNotSupportedErr`（VTErrors.h）：属性被当前解码器拒绝。
const K_VT_PROPERTY_NOT_SUPPORTED_ERR: OSStatus = -12900;
/// `kVTDecodeFrame_EnableAsynchronousDecompression`（VTErrors.h：`1 << 0`）。
const K_VT_DECODE_FRAME_ENABLE_ASYNC: u32 = 1 << 0;
/// `kVTDecodeInfo_Asynchronous`（VTErrors.h：`1 << 0`）。
const K_VT_DECODE_INFO_ASYNCHRONOUS: u32 = 1 << 0;
/// 异步流水线解码最大在途帧数（严格遵守铁律 1：≤ 2）。
pub(crate) const MAX_PIPELINED_IN_FLIGHT: usize = 2;

/// `kCMTimeInvalid`：flags 为空（不含 Valid）即非数值时间。
const CMTIME_INVALID: CMTime = CMTime {
    epoch: 0,
    value: 0,
    timescale: 0,
    flags: CMTimeFlags::empty(),
};

// ─────────────────────────────────────────────────────────────────────────────
// 提交线程 QoS（pthread/qos.h，macOS 10.10+，libSystem）
// ─────────────────────────────────────────────────────────────────────────────

/// `QOS_CLASS_USER_INTERACTIVE`（pthread/qos.h：`0x21`）。
const QOS_CLASS_USER_INTERACTIVE: i32 = 0x21;
/// `QOS_CLASS_UNSPECIFIED`（pthread/qos.h：`0x00`）——只由 getter 返回。
const QOS_CLASS_UNSPECIFIED: i32 = 0x00;

/// pthread QoS C API 声明（pthread/qos.h）。
mod qos_raw {
    /// `pthread_t`（不透明指针）。
    type PthreadT = *mut super::c_void;
    // SAFETY: 声明与 pthread/qos.h 逐字对应；链接 libSystem（C 运行时，
    // macOS 10.10+ 全系统可用）。
    extern "C" {
        pub fn pthread_self() -> PthreadT;
        pub fn pthread_get_qos_class_np(
            thread: PthreadT,
            qos_class: *mut i32,
            relative_priority: *mut i32,
        ) -> i32;
        pub fn pthread_set_qos_class_self_np(qos_class: i32, relative_priority: i32) -> i32;
    }
}

/// 每个解码线程仅抬升一次 QoS 到 `USER_INTERACTIVE`（避免每帧重复 3 次 pthread 系统调用）。
fn promote_thread_qos_once() {
    use std::cell::Cell;
    thread_local! {
        static PROMOTED: Cell<bool> = const { Cell::new(false) };
    }
    PROMOTED.with(|flag| {
        if flag.get() {
            return;
        }
        flag.set(true);
        let mut class: i32 = QOS_CLASS_UNSPECIFIED;
        let mut priority: i32 = 0;
        // SAFETY: 两个出参指向本栈帧合法内存；pthread_self 返回当前线程句柄。
        let ok = unsafe {
            qos_raw::pthread_get_qos_class_np(qos_raw::pthread_self(), &mut class, &mut priority)
        };
        if ok == 0 && class >= QOS_CLASS_USER_INTERACTIVE {
            return;
        }
        // SAFETY: 只作用于当前线程；USER_INTERACTIVE 与 priority 0 是合法入参。
        unsafe {
            qos_raw::pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// VideoToolbox C API 声明（VTDecompressionSession.h / VTSession.h）
// ─────────────────────────────────────────────────────────────────────────────

/// 解码输出回调签名（VTDecompressionSession.h：
/// `typedef void (*VTDecompressionOutputCallback)`）。按 C 惯例
/// `CVImageBufferRef` 重解释为 `CVPixelBuffer`（H.264 解码输出的真身）。
type VtDecodeCallbackFn = unsafe extern "C" fn(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: *mut CVPixelBuffer,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

/// `VTDecompressionOutputCallbackRecord`（按值传给 `VTDecompressionSessionCreate`）。
#[repr(C)]
struct VtDecompressionOutputCallbackRecord {
    /// 回调函数指针（本实现恒为 `Some`）。
    callback: VtDecodeCallbackFn,
    /// 回调 refcon（指向 `Arc<FrameMailbox<CallbackPayload>>` 的堆分配）。
    refcon: *mut c_void,
}

/// 属性键与函数的裸声明。会话句柄按 C 惯例用 `*mut c_void`（指向不透明结构）。
mod raw {
    use super::{c_void, CFString, OSStatus, VtDecompressionOutputCallbackRecord};

    // SAFETY: 声明与 VideoToolbox 头文件逐字对应；链接 VideoToolbox 框架。
    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        /// 属性键（CFStringRef 常量，永不指向无效内存但按 C 头文件是可空指针类型）。
        pub static kVTDecompressionPropertyKey_RealTime: Option<&'static CFString>;

        pub fn VTDecompressionSessionCreate(
            allocator: *const c_void,
            video_format_description: *const c_void,
            decoder_specification: *const c_void,
            image_buffer_attributes: *const c_void,
            output_callback: *const VtDecompressionOutputCallbackRecord,
            session_out: *mut *mut c_void,
        ) -> OSStatus;
        /// 在 VTSession.h；`VTDecompressionSessionRef` 可安全重解释为 `VTSessionRef`。
        pub fn VTSessionSetProperty(
            session: *mut c_void,
            property_key: *const CFString,
            property_value: *const c_void,
        ) -> OSStatus;
        pub fn VTDecompressionSessionDecodeFrame(
            session: *mut c_void,
            sample_buffer: *mut c_void,
            decode_flags: u32,
            source_frame_refcon: *mut c_void,
            info_flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTDecompressionSessionWaitForAsynchronousFrames(session: *mut c_void) -> OSStatus;
        pub fn VTDecompressionSessionInvalidate(session: *mut c_void);
        /// `Boolean VTIsHardwareDecodeSupported(CMVideoCodecType codecType)`
        /// （VTDecompressionSession.h，API_AVAILABLE(macos(10.13), ios(11.0))）。
        pub fn VTIsHardwareDecodeSupported(codec_type: u32) -> u8;
    }

    // SAFETY: CoreFoundation 的 CFRelease，同样按 C 头文件声明（CFBase.h）。
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRelease(cf: *const c_void);
    }
}

/// 动态探测当前 Apple 设备是否支持 AV1 硬件解码（`VTIsHardwareDecodeSupported('av01')`）。
///
/// Apple Silicon M1/M2 返回 `false`，M3/M4/A17 Pro 及更新硬件返回 `true`。
pub(crate) fn is_hardware_av1_decode_supported() -> bool {
    // SAFETY: VTIsHardwareDecodeSupported 是 Apple VideoToolbox 线程安全只读查询 API
    // （VTDecompressionSession.h，API_AVAILABLE(macos(10.13), ios(11.0))），仅接受值类型
    // CMVideoCodecType ('av01')，不解引用任何指针，不转移内存所有权。
    unsafe { raw::VTIsHardwareDecodeSupported(K_CM_VIDEO_CODEC_TYPE_AV1) != 0 }
}

// ─────────────────────────────────────────────────────────────────────────────
// 解码输出邮箱（回调线程 → 等待线程）
// ─────────────────────────────────────────────────────────────────────────────

/// 一次解码的回调载荷。
struct CallbackPayload {
    /// 回调携带的 OSStatus（0 = 成功）。
    status: OSStatus,
    /// 输出帧（回调里 retain +1 的自有引用）；成功但无输出帧时为 None。
    buffer: Option<CFRetained<CVPixelBuffer>>,
    /// 回调触发时刻（用于非重叠解码耗时统计，排除调用方渲染/VSync 排队时间）。
    completed_at: Instant,
}

// SAFETY: 载荷只在邮箱 Mutex 内移动（回调线程构造 → 等待线程消费，互不
// 并发触碰内容）；CFRetained 的 retain/release 是线程安全的（CF 引用计数
// 全局可用），CVPixelBuffer 本身在解出后不做任何并发可变访问。
unsafe impl Send for CallbackPayload {}

/// 回调共享上下文：区分同步快路径（`source_frame_refcon == 0`，免 Condvar 唤醒与 VecDeque 扫描）
/// 与异步流水线路径（`source_frame_refcon != 0`，投递至帧序邮箱）。
/// `sync_slot.1` 表示主线程是否在异步回退路径上等待，常规同步内联路径下为 `false`，零 `notify_one` 开销。
struct CallbackContext {
    sync_slot: Mutex<(Option<CallbackPayload>, bool)>,
    sync_cond: Condvar,
    mailbox: FrameMailbox<CallbackPayload>,
}

impl CallbackContext {
    fn new() -> Self {
        Self {
            sync_slot: Mutex::new((None, false)),
            sync_cond: Condvar::new(),
            mailbox: FrameMailbox::new(),
        }
    }
}

/// VT 解码输出回调。运行在 VideoToolbox 内部队列或同步调用栈上：只 retain + 投递，
/// 绝不做重活、绝不 panic（跨 C 边界 unwind 是未定义行为）。
unsafe extern "C" fn vt_decode_callback(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: *mut CVPixelBuffer,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    // SAFETY: refcon 是 Create 时传入的 `Arc::as_ptr(&cb_ctx)`，指向
    // DecompressionSession.cb_ctx 的堆内层分配。存活期由 Drop 顺序保证：
    // WaitForAsynchronousFrames（等在途回调）+ Invalidate（不再有新回调）
    // + 锁屏障（等正在执行的回调离开临界区）都先于 Arc 释放，
    // 因此这里的解引用不可能悬垂。
    let ctx = unsafe { &*(decompression_output_refcon as *const CallbackContext) };
    // SAFETY: NonNull::new 只做空指针检查；闭包里的 retain 依据回调契约——
    // image_buffer 在回调期间有效，retain(+1) 升级为自有引用，随状态移动/释放。
    let buffer = NonNull::new(image_buffer).map(|buf| unsafe { CFRetained::retain(buf) });
    let payload = CallbackPayload {
        status,
        buffer,
        completed_at: Instant::now(),
    };
    if source_frame_refcon.is_null() {
        let mut slot = ctx.sync_slot.lock().unwrap_or_else(|e| e.into_inner());
        let should_notify = slot.1;
        slot.0 = Some(payload);
        drop(slot);
        if should_notify {
            ctx.sync_cond.notify_one();
        }
    } else {
        // sourceFrameRefcon 原样带回提交时的非零帧号。
        let frame_id = source_frame_refcon as usize as u64;
        ctx.mailbox.post(frame_id, payload);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 对 lib.rs 暴露的类型
// ─────────────────────────────────────────────────────────────────────────────

/// 一次成功解码的自有输出缓冲（零拷贝句柄，D1）。
///
/// 由 [`VtDecoder`] 持有到下一次 `decode` / `recv_decoded` / 解码器销毁，
/// 会话重建排空旧帧放入 `drained_outputs` 时仍保持引用计数安全存活。
pub(crate) struct OwnedDecodeOutput {
    buffer: CFRetained<CVPixelBuffer>,
    /// 输出宽度（像素，取自 CVPixelBuffer——VideoToolbox 是权威）。
    pub width: u32,
    /// 输出高度（像素）。
    pub height: u32,
}

// SAFETY: CFRetained<CVPixelBuffer> 的 retain/release 线程安全；解出后不做并发可变写。
unsafe impl Send for OwnedDecodeOutput {}

impl OwnedDecodeOutput {
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        CFRetained::as_ptr(&self.buffer).as_ptr().cast::<c_void>()
    }
}

/// 解码失败原因（lib.rs 映射到 rc_types::Error）。
pub(crate) enum DecodeFailure {
    /// 提交后等到预算尽头回调仍未到达（E 系超时语义的解码侧对应）。
    Timeout,
    /// 回调携带错误状态，或成功但没有交出输出帧（参考链/码流问题）。
    Callback(String),
    /// 提交或装帧失败（会话/样本层面）。
    Failed(String),
}

impl fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeFailure::Timeout => write!(f, "等待解码输出超时（回调未到达）"),
            DecodeFailure::Callback(msg) => write!(f, "{msg}"),
            DecodeFailure::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

/// 异步流水线最大在途帧数（铁律 1：≤ 2）。
pub(crate) const PIPELINED_MAX_IN_FLIGHT: usize = MAX_PIPELINED_IN_FLIGHT;

/// 异步流水线单帧提交元数据。
pub(crate) struct InFlightDecode {
    pub submit_id: u64,
    pub token: u64,
    pub content_is_idr: bool,
    pub started: Instant,
}

/// 异步流水线单帧回收结果。
pub(crate) struct PipelinedDecodeOutput {
    pub token: u64,
    pub content_is_idr: bool,
    pub started: Instant,
    pub completed_at: Instant,
    pub result: Result<OwnedDecodeOutput, DecodeFailure>,
}

/// 一个配置好的 `VTDecompressionSession`。
///
/// `Send`：随 `VtDecoder`（`Decoder: Send`）跨线程**移交所有权**；所有
/// 访问都发生在持有 `&mut self` 的单一线程上，回调线程只经 `cb_ctx`
/// 触碰数据。
pub(crate) struct DecompressionSession {
    /// +1 的会话引用（VTDecompressionSessionCreate 返回）。Drop 前必须先
    /// Invalidate（Apple 文档要求），见 [`DecompressionSession::drop`]。
    session: NonNull<c_void>,
    /// 会话的格式描述（SPS/PPS 构建时的 +1 引用）。每个样本都引用它；
    /// CMSampleBuffer 会自行 retain，这里是会话级的兜底所有权。
    _format_description: CFRetained<CMFormatDescription>,
    /// 回调共享上下文（refcon 指向其堆内层分配）。必须排在 session 之后：
    /// 字段按声明顺序释放，Arc 在 Drop 手动 CFRelease(session) 与锁屏障
    /// 之后才 drop。
    cb_ctx: Arc<CallbackContext>,
    /// 异步流水线在途元数据队列（容量 ≤ `MAX_PIPELINED_IN_FLIGHT = 2`）。
    in_flight: VecDeque<InFlightDecode>,
    /// 上一帧是否可能残留迟到/未消费邮箱条目（仅在超时或提交后异常退出时置位，
    /// 常规单帧同步路径跳过无谓的 `mailbox.clear()` 锁获取）。
    mailbox_dirty: bool,
}

// SAFETY: 见类型文档：所有权随 VtDecoder 移动，无并发访问；回调线程只经
// FrameMailbox（已声明 Send + Sync）触碰数据；CF 对象的 retain/release
// 本身线程安全。
unsafe impl Send for DecompressionSession {}

impl DecompressionSession {
    /// 从参数集（SPS/PPS 的 NAL 载荷，含 NAL 头字节）建格式描述并创建
    /// 解码会话，设置 RealTime（D3）。
    ///
    /// 任何一步失败都返回 `Err`（错误经构造路径上报为 `Error::Decoder`）。
    pub(crate) fn new(sps: &[u8], pps: &[u8]) -> Result<Self, String> {
        // 1. SPS/PPS → CMVideoFormatDescription（AVCC 长度前缀宽度 = 4，
        //    与装帧侧 to_avcc_without_parameter_sets 的 4 字节前缀一致）。
        let pointers = [
            NonNull::from(sps).cast::<u8>(),
            NonNull::from(pps).cast::<u8>(),
        ];
        let sizes = [sps.len(), pps.len()];
        let mut desc_out: *const CMFormatDescription = std::ptr::null();
        // SAFETY: pointers/sizes 指向栈上数组（调用期间存活），参数集切片由
        // 调用方保活到 Create 同步返回；desc_out 是本栈帧的合法出参。
        // nal 头长度 4 必须与装帧侧的 AVCC 前缀宽度一致（坑 #7）。
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                2,
                NonNull::from(pointers.as_slice()).cast(),
                NonNull::from(sizes.as_slice()).cast(),
                4,
                NonNull::from(&mut desc_out),
            )
        };
        if status != 0 {
            return Err(format!(
                "CMVideoFormatDescriptionCreateFromH264ParameterSets 失败: OSStatus {status}"
            ));
        }
        // SAFETY: Create 成功（status==0）时 desc_out 是 +1 所有权引用
        //（CM_RETURNS_RETAINED_PARAMETER）。
        let desc = NonNull::new(desc_out as *mut CMFormatDescription)
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or_else(|| {
                "CMVideoFormatDescriptionCreateFromH264ParameterSets 返回 0 但描述为空".to_string()
            })?;
        Self::from_format_description(desc)
    }

    /// 从 HEVC 参数集（VPS/SPS/PPS 的 NAL 载荷，含 2 字节 NAL 头）建格式描述，先校验分辨率不超过上限再创建解码会话。
    pub(crate) fn new_hevc(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, Result<String, String>> {
        let pointers = [
            NonNull::from(vps).cast::<u8>(),
            NonNull::from(sps).cast::<u8>(),
            NonNull::from(pps).cast::<u8>(),
        ];
        let sizes = [vps.len(), sps.len(), pps.len()];
        let mut desc_out: *const CMFormatDescription = std::ptr::null();
        // SAFETY: pointers/sizes 指向栈上数组（调用期间存活），参数集切片由调用方保活到 Create 同步返回；
        // nal 头长度 4 必须与 HVCC 4 字节长度前缀一致。
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                3,
                NonNull::from(pointers.as_slice()).cast(),
                NonNull::from(sizes.as_slice()).cast(),
                4,
                None,
                NonNull::from(&mut desc_out),
            )
        };
        if status != 0 {
            return Err(Ok(format!(
                "CMVideoFormatDescriptionCreateFromHEVCParameterSets 失败: OSStatus {status}"
            )));
        }
        // SAFETY: Create 成功（status==0）时 desc_out 是 +1 所有权引用。
        let desc = NonNull::new(desc_out as *mut CMFormatDescription)
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or_else(|| {
                Ok(
                    "CMVideoFormatDescriptionCreateFromHEVCParameterSets 返回 0 但描述为空"
                        .to_string(),
                )
            })?;
        // SAFETY: desc 为合法视频格式描述，CMVideoFormatDescriptionGetDimensions 同步只读解析其尺寸。
        let dims = unsafe { CMVideoFormatDescriptionGetDimensions(&desc) };
        if dims.width <= 0
            || dims.height <= 0
            || dims.width as u32 > max_width
            || dims.height as u32 > max_height
        {
            return Err(Err(format!(
                "HEVC SPS 声明 {}x{} 超过解码上限 {}x{}",
                dims.width, dims.height, max_width, max_height
            )));
        }
        Self::from_format_description(desc).map_err(Ok)
    }

    /// 从 ISO BMFF `av1C` 配置记录与帧尺寸构建 AV1 `CMVideoFormatDescription` 并创建硬件解码会话。
    ///
    /// 返回 `Err(Ok(msg))` 表示硬件/会话级不支持或失败（映射为 `Error::Decoder`），
    /// 返回 `Err(Err(msg))` 表示 `av1C` 或尺寸非法（映射为 `Error::InvalidFrame`）。
    /// 全程通过 `CFRetained` 管理 `CFData` / `CFDictionary` / `CMFormatDescription`，
    /// 任何错误路径均零 `CFTypeRef` 泄漏。
    pub(crate) fn new_av1(
        av1c: &[u8],
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, Result<String, String>> {
        if av1c.len() < 4 || av1c[0] != crate::av1_obu::AV1C_MARKER_VERSION {
            return Err(Err(
                "AV1 av1C 配置非法（长度不足 4 字节或 marker/version 不等于 0x81）".to_string(),
            ));
        }
        if width == 0 || height == 0 || width > max_width || height > max_height {
            return Err(Err(format!(
                "AV1 Sequence Header 声明 {width}x{height} 超过解码上限 {max_width}x{max_height}"
            )));
        }
        let width_i32 =
            i32::try_from(width).map_err(|_| Err(format!("AV1 帧宽 {width} 超出 i32::MAX")))?;
        let height_i32 =
            i32::try_from(height).map_err(|_| Err(format!("AV1 帧高 {height} 超出 i32::MAX")))?;

        if !is_hardware_av1_decode_supported() {
            return Err(Ok(
                "Apple VideoToolbox 不支持本机 AV1 硬件解码 (VTIsHardwareDecodeSupported('av01') == false)"
                    .to_string(),
            ));
        }

        // 1. 构造 { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: { "av1C": CFData(av1c) } }
        let av1c_data = CFData::from_bytes(av1c);
        let av1c_key = CFString::from_static_str("av1C");
        let atom_keys = [CFRetained::as_ptr(&av1c_key)
            .as_ptr()
            .cast_const()
            .cast::<c_void>()];
        let atom_vals = [CFRetained::as_ptr(&av1c_data)
            .as_ptr()
            .cast_const()
            .cast::<c_void>()];
        // SAFETY: atom_keys/atom_vals 长度一致（1），指向本栈帧存活的 CFString/CFData，
        // 使用标准 CFType 回调由 CFDictionary 自动 retain/release。
        let atoms_dict = unsafe {
            CFDictionary::new(
                None,
                atom_keys.as_ptr() as *mut *const c_void,
                atom_vals.as_ptr() as *mut *const c_void,
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or_else(|| Ok("创建 AV1 SampleDescriptionExtensionAtoms 字典失败".to_string()))?;

        // SAFETY: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms 为 CoreMedia
        // 框架静态只读 CFString 常量（API_AVAILABLE(macos(10.7), ios(4.0))），只读无竞争。
        let ext_atoms_key =
            unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
        let ext_keys = [ext_atoms_key as *const CFString as *const c_void];
        let ext_vals = [CFRetained::as_ptr(&atoms_dict)
            .as_ptr()
            .cast_const()
            .cast::<c_void>()];
        // SAFETY: ext_keys/ext_vals 长度一致（1），指向合法 CFString 与存活的 atoms_dict。
        let extensions = unsafe {
            CFDictionary::new(
                None,
                ext_keys.as_ptr() as *mut *const c_void,
                ext_vals.as_ptr() as *mut *const c_void,
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or_else(|| Ok("创建 AV1 FormatDescription extensions 字典失败".to_string()))?;

        // 2. CMVideoFormatDescriptionCreate('av01', width, height, extensions)
        let mut desc_out: *const CMFormatDescription = std::ptr::null();
        // SAFETY: extensions 为合法 CFDictionary，desc_out 为本栈帧合法出参；CoreMedia
        // 同步深拷贝 extensions，调用返回后 extensions/atoms_dict/av1c_data 均可安全释放。
        let status = unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                K_CM_VIDEO_CODEC_TYPE_AV1,
                width_i32,
                height_i32,
                Some(&extensions),
                NonNull::from(&mut desc_out),
            )
        };
        if status != 0 {
            return Err(Ok(format!(
                "CMVideoFormatDescriptionCreate(av01) 失败: OSStatus {status}"
            )));
        }
        // SAFETY: Create 成功（status == 0）时 desc_out 是 +1 所有权引用，立即交由 CFRetained 托管。
        let desc = NonNull::new(desc_out as *mut CMFormatDescription)
            .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
            .ok_or_else(|| {
                Ok("CMVideoFormatDescriptionCreate(av01) 返回 0 但描述为空".to_string())
            })?;

        // 3. 显式要求硬件 AV1 解码（通过静态 CFString 键构造，兼容 IPHONEOS_DEPLOYMENT_TARGET=13.0，
        //    不会引入 iOS 17.0 强链接符号；且前面已由 VTIsHardwareDecodeSupported('av01') 兜底拦截）。
        let require_hw_key = CFString::from_static_str("RequireHardwareAcceleratedVideoDecoder");
        let require_hw_val = CFBoolean::new(true);
        let spec_keys = [CFRetained::as_ptr(&require_hw_key)
            .as_ptr()
            .cast_const()
            .cast::<c_void>()];
        let spec_vals = [require_hw_val as *const CFBoolean as *const c_void];
        // SAFETY: spec_keys/spec_vals 长度一致（1），指向存活的 CFString 与不可变单例 CFBoolean。
        let decoder_spec = unsafe {
            CFDictionary::new(
                None,
                spec_keys.as_ptr() as *mut *const c_void,
                spec_vals.as_ptr() as *mut *const c_void,
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or_else(|| Ok("创建 AV1 decoder_specification 字典失败".to_string()))?;

        Self::from_format_description_with_spec(desc, Some(&decoder_spec)).map_err(Ok)
    }

    fn from_format_description(desc: CFRetained<CMFormatDescription>) -> Result<Self, String> {
        Self::from_format_description_with_spec(desc, None)
    }

    fn from_format_description_with_spec(
        desc: CFRetained<CMFormatDescription>,
        decoder_spec: Option<&CFDictionary>,
    ) -> Result<Self, String> {
        // 2. 建解码会话。H.264/HEVC 的 decoder_specification 为 null（系统默认优先硬件、
        //    软件兜底，保持既有热路径完全不变）；AV1 传入显式硬件要求字典；
        //    image_buffer_attributes 传 null，输出原生 NV12 IOSurface，对零拷贝渲染最友好。
        let cb_ctx = Arc::new(CallbackContext::new());
        let callback_record = VtDecompressionOutputCallbackRecord {
            callback: vt_decode_callback,
            refcon: Arc::as_ptr(&cb_ctx) as *mut c_void,
        };
        let decoder_spec_ptr = decoder_spec.map_or(std::ptr::null(), |d| {
            (d as *const CFDictionary).cast::<c_void>()
        });
        let mut session_out: *mut c_void = std::ptr::null_mut();
        // SAFETY: desc 是刚建的合法视频格式描述；decoder_spec_ptr 为 null 或本栈帧存活的
        // 合法 CFDictionary；image_buffer_attributes 传 null；callback record 指向栈上存活
        // 的合法结构（Create 按值拷贝其内容）；session_out 是本栈帧合法出参；refcon 指向
        // cb_ctx 的稳定堆分配（由会话持有 Arc 克隆保活，见 Drop 顺序）。
        let status = unsafe {
            raw::VTDecompressionSessionCreate(
                std::ptr::null(),
                CFRetained::as_ptr(&desc)
                    .as_ptr()
                    .cast::<c_void>()
                    .cast_const(),
                decoder_spec_ptr,
                std::ptr::null(),
                std::ptr::addr_of!(callback_record),
                std::ptr::addr_of_mut!(session_out),
            )
        };
        if status != 0 || session_out.is_null() {
            return Err(format!(
                "VTDecompressionSessionCreate 失败: OSStatus {status}"
            ));
        }
        // SAFETY: 上面已判空；Create 成功时 session_out 是 +1 所有权引用。
        let session = unsafe { NonNull::new_unchecked(session_out) };

        let created = Self {
            session,
            _format_description: desc,
            cb_ctx,
            in_flight: VecDeque::with_capacity(MAX_PIPELINED_IN_FLIGHT),
            mailbox_dirty: false,
        };

        // 3. D3 低延迟：RealTime = true（提示解码器为实时优化，可牺牲质量
        //    保速度）。个别解码器不支持该属性（-12900）——结构上本项目解码
        //    恒为单帧在途同步等待，不依赖该属性兜底，因此仅容忍这一种状态码。
        // SAFETY: 只对 extern static 做一次读取（拿 CFString 引用，不写）；
        // SDK 常量在框架加载时初始化、永不释放，读取无数据竞争。
        let realtime_key =
            unsafe { raw::kVTDecompressionPropertyKey_RealTime }.ok_or_else(|| {
                "VideoToolbox SDK 常量缺失: kVTDecompressionPropertyKey_RealTime".to_string()
            })?;
        // CFBoolean::new 返回的是不可变单例（kCFBooleanTrue）的静态引用。
        let value = CFBoolean::new(true);
        // SAFETY: session 是本包装拥有的合法会话；键是 SDK 常量 CFString；
        // value 是不可变全局单例（'static，永不释放）；VTSessionSetProperty
        // 同步读取。
        let status = unsafe {
            raw::VTSessionSetProperty(
                created.session.as_ptr(),
                realtime_key,
                value as *const CFBoolean as *const c_void,
            )
        };
        if status != 0 && status != K_VT_PROPERTY_NOT_SUPPORTED_ERR {
            return Err(format!(
                "设置 kVTDecompressionPropertyKey_RealTime (D3) 失败: OSStatus {status}"
            ));
        }
        Ok(created)
    }

    fn build_sample_buffer(
        &self,
        avcc: &[u8],
        pts_frame_id: u64,
    ) -> Result<CFRetained<CMSampleBuffer>, DecodeFailure> {
        // 1. AVCC → CMBlockBuffer（kCFAllocatorNull + AlwaysCopyDataFlag）。
        // SAFETY: 只读 extern static 常量（框架加载时初始化、永不释放）。
        let null_allocator = unsafe { objc2_core_foundation::kCFAllocatorNull }
            .ok_or_else(|| DecodeFailure::Failed("kCFAllocatorNull 常量缺失".to_string()))?;
        let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: memory_block 指向 avcc（同步调用期间存活），Null 分配器承诺
        // CMBlockBuffer 释放时不动它；AlwaysCopyDataFlag 让 CoreMedia 把字节
        // 拷进自有内存，avcc 生命周期从此独立（安全支持异步流水线）。
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                avcc.as_ptr().cast_mut().cast::<c_void>(),
                avcc.len(),
                Some(null_allocator),
                std::ptr::null(),
                0,
                avcc.len(),
                kCMBlockBufferAlwaysCopyDataFlag,
                NonNull::from(&mut block_out),
            )
        };
        if status != 0 || block_out.is_null() {
            return Err(DecodeFailure::Failed(format!(
                "CMBlockBufferCreateWithMemoryBlock 失败: OSStatus {status}"
            )));
        }
        // SAFETY: 上面已判空；Create 成功时 block_out 是 +1 所有权引用。
        let block = unsafe { CFRetained::from_raw(NonNull::new_unchecked(block_out)) };

        // 2. CMSampleBuffer（1 个样本 + 1 条单调 PTS timing；无 B 帧不设 DTS）。
        let timing = [CMSampleTimingInfo {
            duration: CMTIME_INVALID,
            presentationTimeStamp: CMTime {
                epoch: 0,
                flags: CMTimeFlags::Valid,
                value: (pts_frame_id as i64).saturating_mul(10),
                timescale: 600,
            },
            decodeTimeStamp: CMTIME_INVALID,
        }];
        let mut sample_out: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: block/desc 是自有引用；timing 指向栈数组；sample_out 是本栈帧合法出参。
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self._format_description),
                1,
                1,
                timing.as_ptr(),
                0,
                std::ptr::null(),
                NonNull::from(&mut sample_out),
            )
        };
        if status != 0 || sample_out.is_null() {
            return Err(DecodeFailure::Failed(format!(
                "CMSampleBufferCreateReady 失败: OSStatus {status}"
            )));
        }
        // SAFETY: 上面已判空；Create 成功时 sample_out 是 +1 所有权引用。
        Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(sample_out)) })
    }

    fn finish_payload(
        frame_id: u64,
        payload: CallbackPayload,
    ) -> (Result<OwnedDecodeOutput, DecodeFailure>, Instant) {
        let completed_at = payload.completed_at;
        if payload.status != 0 {
            return (
                Err(DecodeFailure::Callback(format!(
                    "解码回调携带错误状态: OSStatus {}（帧 {}）",
                    payload.status, frame_id
                ))),
                completed_at,
            );
        }
        let Some(buffer) = payload.buffer else {
            return (
                Err(DecodeFailure::Callback(format!(
                    "解码回调成功但没有交出输出帧（帧 {frame_id}，码流可能损坏）"
                ))),
                completed_at,
            );
        };
        let width = CVPixelBufferGetWidth(&buffer) as u32;
        let height = CVPixelBufferGetHeight(&buffer) as u32;
        (
            Ok(OwnedDecodeOutput {
                buffer,
                width,
                height,
            }),
            completed_at,
        )
    }

    /// 等待所有在途异步帧结束并清空邮箱残留（flush / 切回同步 / 异常清理前调用）。
    pub(crate) fn wait_and_clear(&mut self) {
        if !self.in_flight.is_empty() || self.mailbox_dirty {
            // SAFETY: session 有效，等待全部在途异步回调完成。
            unsafe {
                raw::VTDecompressionSessionWaitForAsynchronousFrames(self.session.as_ptr());
            }
            self.in_flight.clear();
            self.cb_ctx.mailbox.clear();
            {
                let mut g = self
                    .cb_ctx
                    .sync_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                g.0.take();
                g.1 = false;
            }
            self.mailbox_dirty = false;
        }
    }

    /// 异步流水线重建会话前排空旧会话全部在途帧，保留其 FIFO 输出供 `recv_decoded` 顺序交付，
    /// 避免上层 `ControllerPipeline` 与底层在途计数失步。
    pub(crate) fn drain_all_in_flight(&mut self) -> Vec<PipelinedDecodeOutput> {
        let mut drained = Vec::with_capacity(self.in_flight.len());
        if self.in_flight.is_empty() {
            if self.mailbox_dirty {
                self.wait_and_clear();
            }
            return drained;
        }
        // SAFETY: session 有效，等待旧会话全部在途异步回调完成。
        unsafe {
            raw::VTDecompressionSessionWaitForAsynchronousFrames(self.session.as_ptr());
        }
        let now = Instant::now();
        while let Some(meta) = self.in_flight.pop_front() {
            let (result, completed_at) =
                if let Some(payload) = self.cb_ctx.mailbox.take(meta.submit_id, now) {
                    Self::finish_payload(meta.submit_id, payload)
                } else {
                    (Err(DecodeFailure::Timeout), Instant::now())
                };
            drained.push(PipelinedDecodeOutput {
                token: meta.token,
                content_is_idr: meta.content_is_idr,
                started: meta.started,
                completed_at,
                result,
            });
        }
        self.cb_ctx.mailbox.clear();
        {
            let mut g = self
                .cb_ctx
                .sync_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            g.0.take();
            g.1 = false;
        }
        self.mailbox_dirty = false;
        drained
    }

    /// 当前异步流水线在途帧数（`0..=2`）。
    pub(crate) fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// 当前异步流水线中是否存在尚未回调的 IDR。
    pub(crate) fn has_idr_in_flight(&self) -> bool {
        self.in_flight.iter().any(|f| f.content_is_idr)
    }

    /// 同步解码一帧：优先走 `decodeFlags = 0` 同步内联快路径（免 Condvar 唤醒与 VecDeque 扫描）；
    /// 仅当 VT 意外走异步分支时以 `sync_cond.wait_timeout` 有界等待。
    pub(crate) fn decode_frame(
        &mut self,
        avcc: &[u8],
        frame_id: u64,
        wait: Duration,
    ) -> Result<OwnedDecodeOutput, DecodeFailure> {
        promote_thread_qos_once();
        if !self.in_flight.is_empty() || self.mailbox_dirty {
            self.wait_and_clear();
        }
        let sample = self.build_sample_buffer(avcc, frame_id)?;

        // 同步路径传 source_frame_refcon = null，回调写入 sync_slot（无 Condvar 唤醒）。
        let mut info_flags: u32 = 0;
        // SAFETY: 会话与 sample 有效，info_flags 为合法出参，null refcon 走 sync_slot。
        let status = unsafe {
            raw::VTDecompressionSessionDecodeFrame(
                self.session.as_ptr(),
                CFRetained::as_ptr(&sample).as_ptr().cast::<c_void>(),
                0,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(info_flags),
            )
        };
        if status != 0 {
            {
                let mut g = self
                    .cb_ctx
                    .sync_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                g.0.take();
                g.1 = false;
            }
            self.mailbox_dirty = true;
            return Err(DecodeFailure::Failed(format!(
                "VTDecompressionSessionDecodeFrame 失败: OSStatus {status}"
            )));
        }

        // Apple Silicon 真机实证 decodeFlags = 0 时回调 100% 同步内联完成。
        let inline_payload = {
            let mut g = self
                .cb_ctx
                .sync_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            g.0.take()
        };
        if let Some(payload) = inline_payload {
            return Self::finish_payload(frame_id, payload).0;
        }

        // 防御性兜底：若驱动仍返回异步标记，在 sync_cond 上做严格有界等待（不无界阻塞）。
        if info_flags & K_VT_DECODE_INFO_ASYNCHRONOUS != 0 {
            let deadline = Instant::now()
                .checked_add(wait)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
            let fallback_payload = {
                let mut g = self
                    .cb_ctx
                    .sync_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                g.1 = true;
                while g.0.is_none() {
                    let remain = deadline.saturating_duration_since(Instant::now());
                    if remain.is_zero() {
                        break;
                    }
                    let (next_g, _) = self
                        .cb_ctx
                        .sync_cond
                        .wait_timeout(g, remain)
                        .unwrap_or_else(|e| e.into_inner());
                    g = next_g;
                }
                g.1 = false;
                g.0.take()
            };
            if let Some(payload) = fallback_payload {
                return Self::finish_payload(frame_id, payload).0;
            }
        }
        self.mailbox_dirty = true;
        Err(DecodeFailure::Timeout)
    }

    /// 2-deep 异步提交一帧（`kVTDecodeFrame_EnableAsynchronousDecompression`，非阻塞）。
    pub(crate) fn submit_decode_frame(
        &mut self,
        avcc: &[u8],
        submit_id: u64,
        token: u64,
        content_is_idr: bool,
    ) -> Result<(), DecodeFailure> {
        if self.in_flight.len() >= MAX_PIPELINED_IN_FLIGHT {
            return Err(DecodeFailure::Failed(format!(
                "pipelined decode in_flight limit ({MAX_PIPELINED_IN_FLIGHT}) reached"
            )));
        }
        promote_thread_qos_once();
        if self.mailbox_dirty && self.in_flight.is_empty() {
            self.wait_and_clear();
        }
        // 非零 refcon 确保回调路由到异步 mailbox（0 保留给同步快路径）。
        let refcon_id = submit_id.max(1);
        let sample = self.build_sample_buffer(avcc, refcon_id)?;
        let started = Instant::now();
        let mut info_flags: u32 = 0;
        // SAFETY: 会话与 sample 有效；AlwaysCopyDataFlag 保证 sample 自持码流内存；
        // refcon_id 为非零帧号（回调只按整数读取，不解引用）。
        let status = unsafe {
            raw::VTDecompressionSessionDecodeFrame(
                self.session.as_ptr(),
                CFRetained::as_ptr(&sample).as_ptr().cast::<c_void>(),
                K_VT_DECODE_FRAME_ENABLE_ASYNC,
                refcon_id as usize as *mut c_void,
                std::ptr::addr_of_mut!(info_flags),
            )
        };
        if status != 0 {
            self.mailbox_dirty = true;
            return Err(DecodeFailure::Failed(format!(
                "VTDecompressionSessionDecodeFrame(async) 失败: OSStatus {status}"
            )));
        }
        self.in_flight.push_back(InFlightDecode {
            submit_id: refcon_id,
            token,
            content_is_idr,
            started,
        });
        Ok(())
    }

    /// 按 FIFO 提交顺序回收下一帧异步解码结果；超时无就绪返回 `None`。
    pub(crate) fn recv_decoded_frame(
        &mut self,
        timeout: Duration,
    ) -> Option<PipelinedDecodeOutput> {
        let front_id = self.in_flight.front()?.submit_id;
        let deadline = if timeout.is_zero() {
            Instant::now()
        } else {
            Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(1))
        };
        let payload = self.cb_ctx.mailbox.take(front_id, deadline)?;
        let meta = self.in_flight.pop_front()?;
        let (result, completed_at) = Self::finish_payload(meta.submit_id, payload);
        Some(PipelinedDecodeOutput {
            token: meta.token,
            content_is_idr: meta.content_is_idr,
            started: meta.started,
            completed_at,
            result,
        })
    }
}

impl Drop for DecompressionSession {
    fn drop(&mut self) {
        // SAFETY: session 是 Create 返回的 +1 引用。顺序：
        // 1. WaitForAsynchronousFrames：等在途回调执行完；
        // 2. Invalidate：返回后不再有新回调；
        // 3. 锁屏障：等正在执行的回调离开临界区，之后不会再有代码触碰 refcon；
        // 4. CFRelease 会话；随后字段按声明顺序释放（cb_ctx 的 Arc 最后）。
        unsafe {
            raw::VTDecompressionSessionWaitForAsynchronousFrames(self.session.as_ptr());
            raw::VTDecompressionSessionInvalidate(self.session.as_ptr());
            self.cb_ctx.mailbox.clear();
            {
                let mut g = self
                    .cb_ctx
                    .sync_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                g.0.take();
                g.1 = false;
            }
            raw::CFRelease(self.session.as_ptr().cast_const());
        }
        self.in_flight.clear();
    }
}
