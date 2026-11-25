//! RabbitControl Apple Media (`rc-decode-vt` + `rc-media-apple`) 编解码深度性能基准与瓶颈归因套件。
//!
//! 运行方式（macOS Apple Silicon / Intel 真机）：
//! ```sh
//! cargo bench -p rc-media-apple --bench codec_bench
//! cargo bench -p rc-media-apple --bench codec_bench -- --frames 240 --json /tmp/apple-media-codec-bench.json
//! ```
//!
//! 覆盖三大维度：
//! 1. **真实 VideoToolbox H.264 / HEVC 编码 + `rc_decode_vt::VtDecoder` 全矩阵实测**
//!    （`1920x1080` / `2560x1440` / `3440x1440` / `3840x2160` × H.264 & HEVC，冷启动建会话、P50/P95/P99/Max、极限吞吐 FPS、帧体积与 NAL 计数）；
//! 2. **解码热路径逐段 A/B 归因微基准**（直接复用真实硬编产出帧）：
//!    - Exp 2A：Annex-B → AVCC/HVCC 转换（现网 `split_nals`+2-pass vs 单遍零分配 SWAR vs 直通 AVCC）
//!    - Exp 2B：`CMBlockBuffer` 拷贝开销（`AlwaysCopyDataFlag` vs `kCFAllocatorNull` 零拷贝）
//!    - Exp 2C：`VTDecompressionSessionDecodeFrame` 同步内联（`decodeFlags=0` 实证 + 去邮箱锁）vs 异步流水线（Depth 1 / 2 / 4）
//!    - Exp 2D：`VTDecompressionSessionCreate` 默认 `null` vs 显式硬件 + Metal/IOSurface 属性
//! 3. **`rc-media-apple` `FramePumpRenderer` 与 C ABI 帧取放 FFI 开销对比**
//!    （现网 JSON + `CString` 堆分配 vs 零分配 `#[repr(C)]` 结构体出参）。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("codec_bench 仅支持在 macOS 上运行 VideoToolbox 硬件编解码基准。");
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = macos_bench::run() {
        eprintln!("基准运行失败: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod macos_bench {
    #![allow(unsafe_code)]
    #![deny(unsafe_op_in_unsafe_fn)]
    #![deny(clippy::undocumented_unsafe_blocks)]

    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::ffi::{c_char, c_int, c_void, CStr};
    use std::fs;
    use std::hint::black_box;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use objc2_core_foundation::{
        kCFAllocatorNull, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
        CFBoolean, CFDictionary, CFNumber, CFRetained, CFString,
    };
    use objc2_core_media::{
        kCMBlockBufferAlwaysCopyDataFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
        CMSampleTimingInfo, CMTime, CMTimeFlags,
        CMVideoFormatDescriptionCreateFromH264ParameterSets,
        CMVideoFormatDescriptionCreateFromHEVCParameterSets,
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };
    use objc2_core_video::{
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
        kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        kCVReturnSuccess, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
        CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use rc_capture::FrameTexture;
    use rc_decode::{DecodeOutput, Decoder};
    use rc_decode_vt::{VtDecoder, VtDecoderConfig};
    use rc_encode::Codec;
    use rc_types::{FrameKind, Resolution, Timestamp};

    type OSStatus = i32;

    const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x61766331;
    const K_CM_VIDEO_CODEC_TYPE_HEVC: u32 = 0x68766331;
    const K_VT_PROPERTY_NOT_SUPPORTED_ERR: OSStatus = -12900;
    const K_VT_DECODE_FRAME_ENABLE_ASYNC: u32 = 1 << 0;
    const K_VT_DECODE_INFO_ASYNCHRONOUS: u32 = 1 << 0;
    const ANNEXB_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

    const CMTIME_INVALID: CMTime = CMTime {
        epoch: 0,
        value: 0,
        timescale: 0,
        flags: CMTimeFlags::empty(),
    };

    type VtEncodeCallbackFn = unsafe extern "C" fn(
        output_callback_refcon: *mut c_void,
        source_frame_refcon: *mut c_void,
        status: OSStatus,
        info_flags: u32,
        sample_buffer: *mut CMSampleBuffer,
    );

    type VtDecodeCallbackFn = unsafe extern "C" fn(
        decompression_output_refcon: *mut c_void,
        source_frame_refcon: *mut c_void,
        status: OSStatus,
        info_flags: u32,
        image_buffer: *mut CVPixelBuffer,
        presentation_time_stamp: CMTime,
        presentation_duration: CMTime,
    );

    #[repr(C)]
    struct VtDecompressionOutputCallbackRecord {
        callback: VtDecodeCallbackFn,
        refcon: *mut c_void,
    }

    mod raw {
        use super::{
            c_void, CFString, CMTime, OSStatus, VtDecompressionOutputCallbackRecord,
            VtEncodeCallbackFn,
        };

        #[link(name = "VideoToolbox", kind = "framework")]
        extern "C" {
            pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:
                Option<&'static CFString>;
            pub static kVTVideoEncoderSpecification_EnableLowLatencyRateControl:
                Option<&'static CFString>;
            pub static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder:
                Option<&'static CFString>;
            pub static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder:
                Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_RealTime: Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_AllowFrameReordering: Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration:
                Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_AverageBitRate: Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_ExpectedFrameRate: Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_ProfileLevel: Option<&'static CFString>;
            pub static kVTCompressionPropertyKey_MaximizePowerEfficiency: Option<&'static CFString>;
            pub static kVTProfileLevel_H264_Baseline_AutoLevel: Option<&'static CFString>;
            pub static kVTProfileLevel_H264_High_AutoLevel: Option<&'static CFString>;
            pub static kVTProfileLevel_HEVC_Main_AutoLevel: Option<&'static CFString>;
            pub static kVTEncodeFrameOptionKey_ForceKeyFrame: Option<&'static CFString>;
            pub static kVTDecompressionPropertyKey_RealTime: Option<&'static CFString>;

            pub fn VTCompressionSessionCreate(
                allocator: *const c_void,
                width: i32,
                height: i32,
                codec_type: u32,
                encoder_specification: *const c_void,
                source_image_buffer_attributes: *const c_void,
                compressed_data_allocator: *const c_void,
                output_callback: Option<VtEncodeCallbackFn>,
                output_callback_refcon: *mut c_void,
                compression_session_out: *mut *mut c_void,
            ) -> OSStatus;
            pub fn VTSessionSetProperty(
                session: *mut c_void,
                property_key: *const CFString,
                property_value: *const c_void,
            ) -> OSStatus;
            pub fn VTCompressionSessionPrepareToEncodeFrames(session: *mut c_void) -> OSStatus;
            pub fn VTCompressionSessionEncodeFrame(
                session: *mut c_void,
                image_buffer: *mut c_void,
                presentation_time_stamp: CMTime,
                duration: CMTime,
                frame_properties: *const c_void,
                source_frame_refcon: *mut c_void,
                info_flags_out: *mut u32,
            ) -> OSStatus;
            pub fn VTCompressionSessionCompleteFrames(
                session: *mut c_void,
                complete_until_presentation_time_stamp: CMTime,
            ) -> OSStatus;
            pub fn VTCompressionSessionInvalidate(session: *mut c_void);

            pub fn VTDecompressionSessionCreate(
                allocator: *const c_void,
                video_format_description: *const c_void,
                decoder_specification: *const c_void,
                image_buffer_attributes: *const c_void,
                output_callback: *const VtDecompressionOutputCallbackRecord,
                session_out: *mut *mut c_void,
            ) -> OSStatus;
            pub fn VTDecompressionSessionDecodeFrame(
                session: *mut c_void,
                sample_buffer: *mut c_void,
                decode_flags: u32,
                source_frame_refcon: *mut c_void,
                info_flags_out: *mut u32,
            ) -> OSStatus;
            pub fn VTDecompressionSessionWaitForAsynchronousFrames(
                session: *mut c_void,
            ) -> OSStatus;
            pub fn VTDecompressionSessionInvalidate(session: *mut c_void);
        }

        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            pub fn CFRetain(cf: *const c_void) -> *const c_void;
            pub fn CFRelease(cf: *const c_void);
        }

        #[link(name = "CoreVideo", kind = "framework")]
        extern "C" {
            pub fn CVPixelBufferGetIOSurface(pixel_buffer: *const c_void) -> *mut c_void;
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 统计工具
    // ─────────────────────────────────────────────────────────────────────────

    #[derive(Clone, Debug)]
    struct LatencySummary {
        count: usize,
        min_us: f64,
        mean_us: f64,
        p50_us: f64,
        p95_us: f64,
        p99_us: f64,
        max_us: f64,
    }

    impl LatencySummary {
        fn from_us(mut samples: Vec<f64>) -> Self {
            if samples.is_empty() {
                return Self {
                    count: 0,
                    min_us: 0.0,
                    mean_us: 0.0,
                    p50_us: 0.0,
                    p95_us: 0.0,
                    p99_us: 0.0,
                    max_us: 0.0,
                };
            }
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = samples.len();
            let mean_us = samples.iter().sum::<f64>() / n as f64;
            let pct = |p: f64| -> f64 {
                let rank = ((p / 100.0) * n as f64).ceil() as usize;
                let idx = rank.saturating_sub(1).min(n - 1);
                samples[idx]
            };
            Self {
                count: n,
                min_us: samples[0],
                mean_us,
                p50_us: pct(50.0),
                p95_us: pct(95.0),
                p99_us: pct(99.0),
                max_us: samples[n - 1],
            }
        }

        fn to_json(&self) -> serde_json::Value {
            serde_json::json!({
                "count": self.count,
                "min_us": round2(self.min_us),
                "mean_us": round2(self.mean_us),
                "p50_us": round2(self.p50_us),
                "p95_us": round2(self.p95_us),
                "p99_us": round2(self.p99_us),
                "max_us": round2(self.max_us),
            })
        }
    }

    fn round2(v: f64) -> f64 {
        (v * 100.0).round() / 100.0
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 高熵动态桌面 IOSurface '420v' 像素池
    // ─────────────────────────────────────────────────────────────────────────

    struct PixelPool {
        buffers: Vec<CFRetained<CVPixelBuffer>>,
    }

    fn make_iosurface_metal_attrs(include_420v: bool) -> Result<CFRetained<CFDictionary>, String> {
        // SAFETY: 读取 CoreVideo SDK 全局只读 CFString 常量。
        let (io_key, metal_key, fmt_key) = unsafe {
            (
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
                kCVPixelBufferPixelFormatTypeKey,
            )
        };
        // SAFETY: 创建空 CFDictionary 表示使用默认 IOSurface 属性。
        let empty_dict = unsafe {
            CFDictionary::new(
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or("创建空 IOSurface 属性字典失败")?;
        let metal_true = CFBoolean::new(true);
        let fmt_num = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);

        let mut keys: Vec<*const c_void> = vec![
            io_key as *const CFString as *const c_void,
            metal_key as *const CFString as *const c_void,
        ];
        let mut values: Vec<*const c_void> = vec![
            CFRetained::as_ptr(&empty_dict)
                .as_ptr()
                .cast_const()
                .cast::<c_void>(),
            metal_true as *const CFBoolean as *const c_void,
        ];
        if include_420v {
            keys.push(fmt_key as *const CFString as *const c_void);
            values.push(CFRetained::as_ptr(&fmt_num).as_ptr().cast_const().cast());
        }
        // SAFETY: keys/values 长度一致，全部指针指向本栈帧存活的 CF 对象。
        let dict = unsafe {
            CFDictionary::new(
                None,
                keys.as_ptr() as *mut *const c_void,
                values.as_ptr() as *mut *const c_void,
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or("创建 PixelBuffer 属性字典失败")?;
        Ok(dict)
    }

    impl PixelPool {
        fn new(width: u32, height: u32, count: usize) -> Result<Self, String> {
            let attrs = make_iosurface_metal_attrs(false)?;
            let mut buffers = Vec::with_capacity(count);
            for frame_idx in 0..count {
                let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
                // SAFETY: out 为本栈帧合法出参，attrs 在调用期存活。
                let rv = unsafe {
                    CVPixelBufferCreate(
                        None,
                        width as usize,
                        height as usize,
                        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                        Some(&attrs),
                        NonNull::from(&mut out),
                    )
                };
                if rv != kCVReturnSuccess {
                    return Err(format!("CVPixelBufferCreate 失败: {rv}"));
                }
                let ptr = NonNull::new(out).ok_or("CVPixelBufferCreate 返回空指针")?;
                // SAFETY: Create 成功返回 +1 引用。
                let buf = unsafe { CFRetained::from_raw(ptr) };
                // SAFETY: Lock/Unlock 成对执行，初始化后缓冲只读。
                let lock_rv =
                    unsafe { CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::empty()) };
                if lock_rv != kCVReturnSuccess {
                    return Err(format!("CVPixelBufferLockBaseAddress 失败: {lock_rv}"));
                }
                // SAFETY: 已持锁，plane 0/1 指针在各自 rows * stride 范围内可写。
                unsafe {
                    let y_base = CVPixelBufferGetBaseAddressOfPlane(&buf, 0).cast::<u8>();
                    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&buf, 0);
                    let y_rows = CVPixelBufferGetHeightOfPlane(&buf, 0);
                    let shift_x = frame_idx * 12;
                    let shift_y = frame_idx * 8;
                    let cursor_x = ((frame_idx * 73) % (width as usize).saturating_sub(160).max(1))
                        .min((width as usize).saturating_sub(160));
                    let cursor_y = ((frame_idx * 47) % y_rows.saturating_sub(120).max(1))
                        .min(y_rows.saturating_sub(120));
                    for y in 0..y_rows {
                        let row = std::slice::from_raw_parts_mut(
                            y_base.add(y * y_stride),
                            width as usize,
                        );
                        let sy = y + shift_y;
                        let text_band = ((sy / 24) & 3) == 1 && (sy % 24) >= 6 && (sy % 24) <= 14;
                        let in_cursor_y = y >= cursor_y && y < cursor_y + 120;
                        for (x, px) in row.iter_mut().enumerate() {
                            let sx = x + shift_x;
                            let in_cursor = in_cursor_y && x >= cursor_x && x < cursor_x + 160;
                            let grid_line = (sx & 127) < 2 || (sy & 127) < 2;
                            let text_glyph = text_band && ((sx / 10) % 7 != 0) && ((sx & 1) == 0);
                            let grad = (((sx >> 3) + (sy >> 3)) & 0x5f) as u8;
                            *px = if in_cursor {
                                if (x + y) & 16 == 0 {
                                    224
                                } else {
                                    36
                                }
                            } else if grid_line {
                                200
                            } else if text_glyph {
                                215
                            } else {
                                44 + grad
                            };
                        }
                    }

                    let uv_base = CVPixelBufferGetBaseAddressOfPlane(&buf, 1).cast::<u8>();
                    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&buf, 1);
                    let uv_rows = CVPixelBufferGetHeightOfPlane(&buf, 1);
                    let u_shift = frame_idx * 6;
                    for y in 0..uv_rows {
                        let row = std::slice::from_raw_parts_mut(
                            uv_base.add(y * uv_stride),
                            (width as usize) & !1,
                        );
                        let band = ((y + u_shift) >> 6) & 3;
                        let (cb, cr) = match band {
                            0 => (110u8, 146u8),
                            1 => (142u8, 118u8),
                            2 => (96u8, 162u8),
                            _ => (128u8, 128u8),
                        };
                        for pair in row.chunks_exact_mut(2) {
                            pair[0] = cb;
                            pair[1] = cr;
                        }
                    }
                    CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags::empty());
                }
                buffers.push(buf);
            }
            Ok(Self { buffers })
        }

        fn ptr_at(&self, i: usize) -> *mut c_void {
            let b = &self.buffers[i % self.buffers.len()];
            CFRetained::as_ptr(b).as_ptr().cast()
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 独立 VideoToolbox 硬件编码生成器（产出与 rc-encode-vt 同规格的真实 Annex-B + AVCC/HVCC 帧）
    // ─────────────────────────────────────────────────────────────────────────

    struct EncodedSample {
        annexb: Vec<u8>,
        avcc: Vec<u8>,
        kind: FrameKind,
        nal_count: usize,
    }

    struct GeneratedStream {
        codec: Codec,
        param_sets: Vec<Vec<u8>>,
        frames: Vec<EncodedSample>,
        enc_init_us: f64,
        enc_latencies_us: Vec<f64>,
        enc_wall_s: f64,
    }

    struct EncShared {
        state: Mutex<Option<Result<CFRetained<CMSampleBuffer>, OSStatus>>>,
        cond: Condvar,
    }

    // SAFETY: 内部仅在 Mutex 内访问，CFRetained 引用计数线程安全。
    unsafe impl Send for EncShared {}
    // SAFETY: 同上。
    unsafe impl Sync for EncShared {}

    unsafe extern "C" fn bench_enc_cb(
        refcon: *mut c_void,
        _source_refcon: *mut c_void,
        status: OSStatus,
        _info_flags: u32,
        sample: *mut CMSampleBuffer,
    ) {
        // SAFETY: refcon 指向存活的 Arc<EncShared>，生命周期由会话 Drop 前的 CompleteFrames + Invalidate 保证。
        let shared = unsafe { &*(refcon as *const EncShared) };
        let res = if status == 0 {
            match NonNull::new(sample) {
                // SAFETY: 回调契约保证 sample 在回调期间有效，retain +1。
                Some(s) => Ok(unsafe { CFRetained::retain(s) }),
                None => Err(-1),
            }
        } else {
            Err(status)
        };
        let mut g = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(res);
        drop(g);
        shared.cond.notify_one();
    }

    fn set_vt_prop<T: ?Sized>(
        session: *mut c_void,
        key: Option<&'static CFString>,
        val: &T,
    ) -> Result<OSStatus, String> {
        let k = key.ok_or("缺少 VT SDK 属性键常量")?;
        // SAFETY: session 有效，key 为静态常量，val 指向当前栈有效 CF 对象。
        Ok(unsafe { raw::VTSessionSetProperty(session, k, val as *const T as *const c_void) })
    }

    fn generate_real_stream(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        frame_count: usize,
        pool: &PixelPool,
    ) -> Result<GeneratedStream, String> {
        let is_hevc = codec == Codec::Hevc;
        let init_start = Instant::now();
        let shared = Arc::new(EncShared {
            state: Mutex::new(None),
            cond: Condvar::new(),
        });

        // SAFETY: 只读访问 SDK 全局常量。
        let (req_hw_k, llrc_k) = unsafe {
            (
                raw::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                raw::kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
            )
        };
        let req_hw_k = req_hw_k.ok_or("缺少 RequireHardwareAcceleratedVideoEncoder")?;
        let llrc_k = llrc_k.ok_or("缺少 EnableLowLatencyRateControl")?;
        let hw_true = CFBoolean::new(true);
        let llrc_val = CFBoolean::new(fps < 90);
        let spec_keys = [
            req_hw_k as *const CFString as *const c_void,
            llrc_k as *const CFString as *const c_void,
        ];
        let spec_vals = [
            hw_true as *const CFBoolean as *const c_void,
            llrc_val as *const CFBoolean as *const c_void,
        ];
        // SAFETY: 栈上 2 个有效 CF 键值对指针构造不可变字典。
        let spec_dict = unsafe {
            CFDictionary::new(
                None,
                spec_keys.as_ptr() as *mut *const c_void,
                spec_vals.as_ptr() as *mut *const c_void,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or("创建 encoder_spec 字典失败")?;

        let mut session_out: *mut c_void = std::ptr::null_mut();
        let codec_type = if is_hevc {
            K_CM_VIDEO_CODEC_TYPE_HEVC
        } else {
            K_CM_VIDEO_CODEC_TYPE_H264
        };
        // SAFETY: spec_dict 与 shared 在调用期间存活，session_out 为栈出参。
        let st = unsafe {
            raw::VTCompressionSessionCreate(
                std::ptr::null(),
                width as i32,
                height as i32,
                codec_type,
                &*spec_dict as *const CFDictionary as *const c_void,
                std::ptr::null(),
                std::ptr::null(),
                Some(bench_enc_cb),
                Arc::as_ptr(&shared) as *mut c_void,
                std::ptr::addr_of_mut!(session_out),
            )
        };
        if st != 0 || session_out.is_null() {
            return Err(format!("VTCompressionSessionCreate 失败: {st}"));
        }

        // SAFETY: 读取全局 SDK 常量并设置与 rc-encode-vt 完全一致的属性。
        unsafe {
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_RealTime,
                CFBoolean::new(true),
            )?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false),
            )?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &*CFNumber::new_i32(i32::MAX),
            )?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &*CFNumber::new_f64(0.0),
            )?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_AverageBitRate,
                &*CFNumber::new_i64(i64::from(bitrate_bps)),
            )?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_ExpectedFrameRate,
                &*CFNumber::new_i32(fps as i32),
            )?;
            let profile = if is_hevc {
                raw::kVTProfileLevel_HEVC_Main_AutoLevel
            } else if u64::from(width) * u64::from(height) > 2_073_600 || fps > 60 {
                raw::kVTProfileLevel_H264_High_AutoLevel
            } else {
                raw::kVTProfileLevel_H264_Baseline_AutoLevel
            }
            .ok_or("缺少 ProfileLevel 常量")?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_ProfileLevel,
                profile,
            )?;
            let _ = set_vt_prop(
                session_out,
                raw::kVTCompressionPropertyKey_MaximizePowerEfficiency,
                CFBoolean::new(false),
            );
            let prep = raw::VTCompressionSessionPrepareToEncodeFrames(session_out);
            if prep != 0 {
                raw::VTCompressionSessionInvalidate(session_out);
                raw::CFRelease(session_out.cast_const());
                return Err(format!("PrepareToEncodeFrames 失败: {prep}"));
            }
        }
        let enc_init_us = init_start.elapsed().as_secs_f64() * 1e6;

        // 构造 ForceKeyFrame 字典（供首帧 IDR 使用）。
        // SAFETY: 读取全局 SDK 常量构造单键值对字典。
        let force_dict = unsafe {
            let k = raw::kVTEncodeFrameOptionKey_ForceKeyFrame.ok_or("缺少 ForceKeyFrame 常量")?;
            let v = CFBoolean::new(true);
            let ks = [k as *const CFString as *const c_void];
            let vs = [v as *const CFBoolean as *const c_void];
            CFDictionary::new(
                None,
                ks.as_ptr() as *mut *const c_void,
                vs.as_ptr() as *mut *const c_void,
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .ok_or("创建 ForceKeyFrame 字典失败")?
        };

        let timescale = (fps * 1000) as i32;
        let mut frames = Vec::with_capacity(frame_count);
        let mut param_sets: Vec<Vec<u8>> = Vec::new();
        let mut enc_latencies_us = Vec::with_capacity(frame_count);
        let wall_start = Instant::now();

        for i in 0..frame_count {
            let force_idr = i == 0;
            let pts = CMTime {
                epoch: 0,
                flags: CMTimeFlags::Valid,
                value: (i as i64) * 1000,
                timescale,
            };
            let props_ptr = if force_idr {
                &*force_dict as *const CFDictionary as *const c_void
            } else {
                std::ptr::null()
            };
            let mut info_flags = 0u32;
            let t0 = Instant::now();
            // SAFETY: session_out 有效，pixel buffer 来自存活的 pool，props_ptr 有效或 null。
            let st = unsafe {
                raw::VTCompressionSessionEncodeFrame(
                    session_out,
                    pool.ptr_at(i),
                    pts,
                    CMTIME_INVALID,
                    props_ptr,
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(info_flags),
                )
            };
            if st != 0 {
                return Err(format!("EncodeFrame 失败: {st}"));
            }

            let sample = {
                let mut g = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(res) = g.take() {
                        break res.map_err(|s| format!("编码回调错误: {s}"))?;
                    }
                    g = shared.cond.wait(g).unwrap_or_else(|e| e.into_inner());
                }
            };
            let enc_us = t0.elapsed().as_secs_f64() * 1e6;
            if i > 0 {
                enc_latencies_us.push(enc_us);
            }

            // 提取原始 AVCC/HVCC 字节与参数集，并拼装 Annex-B。
            // SAFETY: sample 为 +1 自有引用，同步读取 data_buffer / format_description。
            let block = unsafe { sample.data_buffer() }.ok_or("无 data_buffer")?;
            // SAFETY: block 有效。
            let len = unsafe { block.data_length() };
            let mut avcc = vec![0u8; len];
            // SAFETY: avcc 可写且长度为 len。
            let cpy =
                unsafe { block.copy_data_bytes(0, len, NonNull::from(avcc.as_mut_slice()).cast()) };
            if cpy != 0 {
                return Err(format!("copy_data_bytes 失败: {cpy}"));
            }

            if param_sets.is_empty() {
                // SAFETY: sample 有效。
                if let Some(desc) = unsafe { sample.format_description() } {
                    let mut total = 0usize;
                    for idx in 0..8usize {
                        let mut ptr: *const u8 = std::ptr::null();
                        let mut size = 0usize;
                        let mut count_out = 0usize;
                        let mut nal_len: c_int = 0;
                        // SAFETY: desc 存活，出参指向本栈帧。
                        let pst = unsafe {
                            if is_hevc {
                                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                                    &desc,
                                    idx,
                                    std::ptr::addr_of_mut!(ptr),
                                    std::ptr::addr_of_mut!(size),
                                    std::ptr::addr_of_mut!(count_out),
                                    std::ptr::addr_of_mut!(nal_len),
                                )
                            } else {
                                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                                    &desc,
                                    idx,
                                    std::ptr::addr_of_mut!(ptr),
                                    std::ptr::addr_of_mut!(size),
                                    std::ptr::addr_of_mut!(count_out),
                                    std::ptr::addr_of_mut!(nal_len),
                                )
                            }
                        };
                        if pst != 0 {
                            break;
                        }
                        if idx == 0 {
                            total = count_out;
                        }
                        if size > 0 && !ptr.is_null() {
                            // SAFETY: desc 存活期内参数集切片有效。
                            let slice = unsafe { std::slice::from_raw_parts(ptr, size) };
                            param_sets.push(slice.to_vec());
                        }
                        if total != 0 && idx + 1 >= total {
                            break;
                        }
                    }
                }
            }

            // 转换 AVCC/HVCC → Annex-B（IDR 前置参数集），并统计 NAL 数。
            let mut pos = 0usize;
            let mut is_idr = false;
            let mut nal_count = 0usize;
            while pos + 4 <= avcc.len() {
                let nlen =
                    u32::from_be_bytes([avcc[pos], avcc[pos + 1], avcc[pos + 2], avcc[pos + 3]])
                        as usize;
                let pstart = pos + 4;
                let pend = pstart + nlen;
                if pend > avcc.len() || nlen == 0 {
                    break;
                }
                let hdr = avcc[pstart];
                let ntype = if is_hevc {
                    (hdr >> 1) & 0x3f
                } else {
                    hdr & 0x1f
                };
                if (is_hevc && (16..=21).contains(&ntype)) || (!is_hevc && ntype == 5) {
                    is_idr = true;
                }
                nal_count += 1;
                pos = pend;
            }

            let ps_bytes: usize = if is_idr {
                param_sets.iter().map(|p| 4 + p.len()).sum()
            } else {
                0
            };
            let mut annexb = Vec::with_capacity(ps_bytes + avcc.len());
            if is_idr {
                for ps in &param_sets {
                    annexb.extend_from_slice(&ANNEXB_START_CODE);
                    annexb.extend_from_slice(ps);
                }
            }
            pos = 0;
            while pos + 4 <= avcc.len() {
                let nlen =
                    u32::from_be_bytes([avcc[pos], avcc[pos + 1], avcc[pos + 2], avcc[pos + 3]])
                        as usize;
                let pstart = pos + 4;
                let pend = pstart + nlen;
                if pend > avcc.len() || nlen == 0 {
                    break;
                }
                annexb.extend_from_slice(&ANNEXB_START_CODE);
                annexb.extend_from_slice(&avcc[pstart..pend]);
                pos = pend;
            }

            frames.push(EncodedSample {
                annexb,
                avcc,
                kind: if is_idr { FrameKind::Idr } else { FrameKind::P },
                nal_count,
            });
        }
        let enc_wall_s = wall_start.elapsed().as_secs_f64();

        // SAFETY: CompleteFrames → Invalidate → 锁屏障 → CFRelease。
        unsafe {
            raw::VTCompressionSessionCompleteFrames(session_out, CMTIME_INVALID);
            raw::VTCompressionSessionInvalidate(session_out);
            drop(shared.state.lock().unwrap_or_else(|e| e.into_inner()));
            raw::CFRelease(session_out.cast_const());
        }

        Ok(GeneratedStream {
            codec,
            param_sets,
            frames,
            enc_init_us,
            enc_latencies_us,
            enc_wall_s,
        })
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Part 1: `rc_decode_vt::VtDecoder` 全矩阵基准
    // ─────────────────────────────────────────────────────────────────────────

    struct MatrixRow {
        label: String,
        codec: &'static str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_mbps: f64,
        idr_kb: f64,
        p_avg_kb: f64,
        avg_nals: f64,
        enc_init_ms: f64,
        enc_summary: LatencySummary,
        enc_max_fps: f64,
        dec_cold_idr_ms: f64,
        dec_summary: LatencySummary,
        dec_max_fps: f64,
        dec_out_has_iosurface: bool,
        dec_out_pixfmt: u32,
    }

    fn bench_vt_decoder(
        stream: &GeneratedStream,
    ) -> Result<(f64, LatencySummary, f64, bool, u32), String> {
        let mut dec = VtDecoder::new(VtDecoderConfig {
            codec: stream.codec,
            max_resolution: Resolution::new(4096, 4096),
            ..VtDecoderConfig::default()
        })
        .map_err(|e| format!("VtDecoder::new 失败: {e}"))?;

        let mut cold_idr_ms = 0.0;
        let mut p_latencies_us = Vec::with_capacity(stream.frames.len());
        let mut has_iosurface = false;
        let mut pixfmt = 0u32;

        // 跑两轮：首帧测冷启动建会话，后续全部 P 帧测稳态解码耗时与吞吐。
        let mut p_wall_start: Option<Instant> = None;
        for (idx, f) in stream.frames.iter().enumerate() {
            let t0 = Instant::now();
            let out = dec
                .decode(&f.annexb, f.kind, Timestamp(u32::MAX))
                .map_err(|e| format!("VtDecoder::decode 第 {idx} 帧失败: {e}"))?;
            let dt_us = t0.elapsed().as_secs_f64() * 1e6;
            match out {
                DecodeOutput::Frame(df) => {
                    if idx == 0 {
                        cold_idr_ms = dt_us / 1000.0;
                        if let FrameTexture::CVPixelBuffer(ptr) = df.texture {
                            // SAFETY: ptr 在下一次 decode 前由 VtDecoder::last_output 保活。
                            let surf = unsafe { raw::CVPixelBufferGetIOSurface(ptr.cast_const()) };
                            has_iosurface = !surf.is_null();
                            // SAFETY: 同上，读取像素格式。
                            let pb = unsafe { &*(ptr as *const CVPixelBuffer) };
                            pixfmt = CVPixelBufferGetPixelFormatType(pb);
                        }
                        p_wall_start = Some(Instant::now());
                    } else {
                        p_latencies_us.push(dt_us);
                    }
                }
                other => return Err(format!("第 {idx} 帧返回非 Frame: {other:?}")),
            }
        }
        let p_wall_s = p_wall_start
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(1e-6)
            .max(1e-9);
        let max_fps = p_latencies_us.len() as f64 / p_wall_s;
        Ok((
            cold_idr_ms,
            LatencySummary::from_us(p_latencies_us),
            max_fps,
            has_iosurface,
            pixfmt,
        ))
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Part 2A: Bitstream Annex-B → AVCC/HVCC 转换微基准
    // ─────────────────────────────────────────────────────────────────────────

    #[inline]
    fn start_code_len_at(buf: &[u8], pos: usize) -> Option<usize> {
        let rest = buf.get(pos..)?;
        if rest.len() >= 4 && rest[0] == 0 && rest[1] == 0 && rest[2] == 0 && rest[3] == 1 {
            Some(4)
        } else if rest.len() >= 3 && rest[0] == 0 && rest[1] == 0 && rest[2] == 1 {
            Some(3)
        } else {
            None
        }
    }

    /// 复刻现网 `rc-decode-vt` 的实现（每帧分配 `Vec<(u8, &[u8])>` + 标量 3 字节步进扫描 + 两遍写入 `avcc_scratch`）。
    fn current_annexb_to_avcc(input: &[u8], is_hevc: bool, out: &mut Vec<u8>) -> Option<usize> {
        if input.is_empty() {
            return None;
        }
        let mut nals = Vec::with_capacity(if is_hevc { 6 } else { 4 });
        let mut pos = 0;
        while pos < input.len() {
            let code_len = start_code_len_at(input, pos)?;
            let payload_start = pos + code_len;
            if payload_start >= input.len() {
                return None;
            }
            let mut next = payload_start;
            while next < input.len() {
                if next + 2 < input.len() && input[next + 2] > 1 {
                    next += 3;
                    continue;
                }
                if start_code_len_at(input, next).is_some() {
                    break;
                }
                next += 1;
            }
            let payload = input.get(payload_start..next)?;
            if payload.is_empty() {
                return None;
            }
            let nt = if is_hevc {
                (payload[0] >> 1) & 0x3f
            } else {
                payload[0] & 0x1f
            };
            nals.push((nt, payload));
            pos = next;
        }
        out.clear();
        let mut total = 0usize;
        for &(nt, p) in &nals {
            if (is_hevc && matches!(nt, 32..=34)) || (!is_hevc && (nt == 7 || nt == 8)) {
                continue;
            }
            total += 4 + p.len();
        }
        out.reserve(total);
        for &(nt, p) in &nals {
            if (is_hevc && matches!(nt, 32..=34)) || (!is_hevc && (nt == 7 || nt == 8)) {
                continue;
            }
            out.extend_from_slice(&(p.len() as u32).to_be_bytes());
            out.extend_from_slice(p);
        }
        Some(out.len())
    }

    /// 候选优化：零 `Vec` 分配 + 8 字节 SWAR 零字节探测快速定位 `0x00 0x00 0x01` + 单遍直接写入 `out`。
    #[inline]
    fn find_next_start_code_swar(buf: &[u8], mut pos: usize) -> usize {
        let len = buf.len();
        while pos + 3 <= len {
            // 8 字节 SWAR：若 8 字节块里没有任何 0x00，则前 6 字节不可能包含起始码首字节。
            while pos + 8 <= len {
                let word = u64::from_ne_bytes([
                    buf[pos],
                    buf[pos + 1],
                    buf[pos + 2],
                    buf[pos + 3],
                    buf[pos + 4],
                    buf[pos + 5],
                    buf[pos + 6],
                    buf[pos + 7],
                ]);
                let has_zero =
                    (word.wrapping_sub(0x0101_0101_0101_0101)) & !word & 0x8080_8080_8080_8080;
                if has_zero == 0 {
                    pos += 8;
                } else {
                    break;
                }
            }
            if pos + 3 > len {
                return len;
            }
            if buf[pos] == 0 && buf[pos + 1] == 0 {
                if buf[pos + 2] == 1 {
                    return pos;
                }
                if pos + 3 < len && buf[pos + 2] == 0 && buf[pos + 3] == 1 {
                    return pos;
                }
            }
            pos += 1;
        }
        len
    }

    fn fast_swar_annexb_to_avcc(input: &[u8], is_hevc: bool, out: &mut Vec<u8>) -> Option<usize> {
        if input.is_empty() {
            return None;
        }
        out.clear();
        out.reserve(input.len());
        let mut pos = 0;
        while pos < input.len() {
            let code_len = start_code_len_at(input, pos)?;
            let payload_start = pos + code_len;
            if payload_start >= input.len() {
                return None;
            }
            let next = find_next_start_code_swar(input, payload_start);
            let payload = &input[payload_start..next];
            if payload.is_empty() {
                return None;
            }
            let nt = if is_hevc {
                (payload[0] >> 1) & 0x3f
            } else {
                payload[0] & 0x1f
            };
            let is_ps = if is_hevc {
                matches!(nt, 32..=34)
            } else {
                nt == 7 || nt == 8
            };
            if !is_ps {
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                out.extend_from_slice(payload);
            }
            pos = next;
        }
        Some(out.len())
    }

    fn bench_bitstream_conversion(stream: &GeneratedStream) -> serde_json::Value {
        let is_hevc = stream.codec == Codec::Hevc;
        let p_frames: Vec<&[u8]> = stream
            .frames
            .iter()
            .skip(1)
            .map(|f| f.annexb.as_slice())
            .collect();
        let total_bytes: usize = p_frames.iter().map(|f| f.len()).sum();
        let iters = 20usize;
        let mut scratch = Vec::with_capacity(1024 * 1024);

        let t0 = Instant::now();
        let mut acc1 = 0usize;
        for _ in 0..iters {
            for f in &p_frames {
                acc1 = acc1.wrapping_add(
                    current_annexb_to_avcc(black_box(f), is_hevc, &mut scratch).unwrap_or(0),
                );
            }
        }
        let cur_us = t0.elapsed().as_secs_f64() * 1e6 / (iters * p_frames.len()) as f64;
        black_box(acc1);

        let t1 = Instant::now();
        let mut acc2 = 0usize;
        for _ in 0..iters {
            for f in &p_frames {
                acc2 = acc2.wrapping_add(
                    fast_swar_annexb_to_avcc(black_box(f), is_hevc, &mut scratch).unwrap_or(0),
                );
            }
        }
        let swar_us = t1.elapsed().as_secs_f64() * 1e6 / (iters * p_frames.len()) as f64;
        black_box(acc2);

        let avg_kb = (total_bytes as f64 / p_frames.len().max(1) as f64) / 1024.0;
        let cur_gbs =
            (total_bytes as f64 * iters as f64) / (cur_us * (iters * p_frames.len()) as f64 * 1e3);
        let swar_gbs =
            (total_bytes as f64 * iters as f64) / (swar_us * (iters * p_frames.len()) as f64 * 1e3);

        serde_json::json!({
            "avg_frame_kb": round2(avg_kb),
            "current_scalar_us": round2(cur_us),
            "current_gb_s": round2(cur_gbs),
            "swar_single_pass_us": round2(swar_us),
            "swar_gb_s": round2(swar_gbs),
            "speedup": round2(cur_us / swar_us.max(0.001)),
        })
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Part 2B & 2C & 2D: Raw VTDecompressionSession A/B 对比实验
    // ─────────────────────────────────────────────────────────────────────────

    fn create_format_desc(
        codec: Codec,
        ps: &[Vec<u8>],
    ) -> Result<CFRetained<CMFormatDescription>, String> {
        let mut desc_out: *const CMFormatDescription = std::ptr::null();
        let ptrs: Vec<NonNull<u8>> = ps
            .iter()
            .map(|p| NonNull::from(p.as_slice()).cast())
            .collect();
        let sizes: Vec<usize> = ps.iter().map(|p| p.len()).collect();
        // SAFETY: ptrs/sizes 在调用期间有效，desc_out 为合法栈出参。
        let st = unsafe {
            if codec == Codec::Hevc {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    ptrs.len(),
                    NonNull::from(ptrs.as_slice()).cast(),
                    NonNull::from(sizes.as_slice()).cast(),
                    4,
                    None,
                    NonNull::from(&mut desc_out),
                )
            } else {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    ptrs.len(),
                    NonNull::from(ptrs.as_slice()).cast(),
                    NonNull::from(sizes.as_slice()).cast(),
                    4,
                    NonNull::from(&mut desc_out),
                )
            }
        };
        if st != 0 || desc_out.is_null() {
            return Err(format!("创建 CMFormatDescription 失败: {st}"));
        }
        // SAFETY: Create 成功返回 +1 所有权引用。
        Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(desc_out.cast_mut())) })
    }

    fn bench_cmblockbuffer_copy(stream: &GeneratedStream) -> Result<serde_json::Value, String> {
        let p_frames: Vec<&[u8]> = stream
            .frames
            .iter()
            .skip(1)
            .map(|f| f.avcc.as_slice())
            .collect();
        // SAFETY: 读取全局静态常量。
        let null_alloc = unsafe { kCFAllocatorNull }.ok_or("kCFAllocatorNull 缺失")?;
        let iters = 20usize;

        // A: kCMBlockBufferAlwaysCopyDataFlag（现网行为）
        let t0 = Instant::now();
        for _ in 0..iters {
            for avcc in &p_frames {
                let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
                // SAFETY: avcc 在同步调用期间有效，AlwaysCopyDataFlag 拷入 CM 内存。
                let st = unsafe {
                    CMBlockBuffer::create_with_memory_block(
                        None,
                        avcc.as_ptr().cast_mut().cast(),
                        avcc.len(),
                        Some(null_alloc),
                        std::ptr::null(),
                        0,
                        avcc.len(),
                        kCMBlockBufferAlwaysCopyDataFlag,
                        NonNull::from(&mut block_out),
                    )
                };
                if st != 0 || block_out.is_null() {
                    return Err(format!("AlwaysCopy CMBlockBuffer 失败: {st}"));
                }
                // SAFETY: 释放 +1 引用。
                let blk = unsafe { CFRetained::from_raw(NonNull::new_unchecked(block_out)) };
                black_box(&blk);
            }
        }
        let copy_us = t0.elapsed().as_secs_f64() * 1e6 / (iters * p_frames.len()) as f64;

        // B: flags = 0（零拷贝，kCFAllocatorNull 承诺不释放，依赖调用方保活 avcc）
        let t1 = Instant::now();
        for _ in 0..iters {
            for avcc in &p_frames {
                let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
                // SAFETY: avcc 存活至 blk drop 之后，kCFAllocatorNull 保证不释放 avcc。
                let st = unsafe {
                    CMBlockBuffer::create_with_memory_block(
                        None,
                        avcc.as_ptr().cast_mut().cast(),
                        avcc.len(),
                        Some(null_alloc),
                        std::ptr::null(),
                        0,
                        avcc.len(),
                        0,
                        NonNull::from(&mut block_out),
                    )
                };
                if st != 0 || block_out.is_null() {
                    return Err(format!("ZeroCopy CMBlockBuffer 失败: {st}"));
                }
                // SAFETY: 释放 +1 引用。
                let blk = unsafe { CFRetained::from_raw(NonNull::new_unchecked(block_out)) };
                black_box(&blk);
            }
        }
        let nocopy_us = t1.elapsed().as_secs_f64() * 1e6 / (iters * p_frames.len()) as f64;

        Ok(serde_json::json!({
            "always_copy_us": round2(copy_us),
            "zero_copy_us": round2(nocopy_us),
            "saved_us": round2(copy_us - nocopy_us),
            "speedup": round2(copy_us / nocopy_us.max(0.001)),
        }))
    }

    // 同步内联解码（无 Mutex/Condvar，直接写调用栈槽）
    struct InlineSlot {
        called_inline: Cell<bool>,
        status: Cell<OSStatus>,
        width: Cell<u32>,
        height: Cell<u32>,
        last_buf: Cell<Option<CFRetained<CVPixelBuffer>>>,
    }

    unsafe extern "C" fn inline_decode_cb(
        refcon: *mut c_void,
        _src_refcon: *mut c_void,
        status: OSStatus,
        _info_flags: u32,
        image_buffer: *mut CVPixelBuffer,
        _pts: CMTime,
        _dur: CMTime,
    ) {
        // SAFETY: decodeFlags=0 同步路径下回调在 DecodeFrame 返回前于同线程触发，refcon 指向存活的 InlineSlot。
        let slot = unsafe { &*(refcon as *const InlineSlot) };
        slot.called_inline.set(true);
        slot.status.set(status);
        if let Some(pb) = NonNull::new(image_buffer) {
            // SAFETY: 回调期间 image_buffer 有效，retain +1。
            let retained = unsafe { CFRetained::retain(pb) };
            slot.width.set(CVPixelBufferGetWidth(&retained) as u32);
            slot.height.set(CVPixelBufferGetHeight(&retained) as u32);
            slot.last_buf.set(Some(retained));
        }
    }

    type AsyncDecItem = (u64, OSStatus, Option<CFRetained<CVPixelBuffer>>);

    struct AsyncDecQueue {
        done: Mutex<VecDeque<AsyncDecItem>>,
        cond: Condvar,
    }

    // SAFETY: 仅在 Mutex 内访问，CFRetained 线程安全。
    unsafe impl Send for AsyncDecQueue {}
    // SAFETY: 同上。
    unsafe impl Sync for AsyncDecQueue {}

    unsafe extern "C" fn async_decode_cb(
        refcon: *mut c_void,
        src_refcon: *mut c_void,
        status: OSStatus,
        _info_flags: u32,
        image_buffer: *mut CVPixelBuffer,
        _pts: CMTime,
        _dur: CMTime,
    ) {
        // SAFETY: refcon 指向存活的 Arc<AsyncDecQueue>。
        let q = unsafe { &*(refcon as *const AsyncDecQueue) };
        let id = src_refcon as usize as u64;
        // SAFETY: 回调契约保证 image_buffer 有效，retain +1。
        let buf = NonNull::new(image_buffer).map(|p| unsafe { CFRetained::retain(p) });
        let mut g = q.done.lock().unwrap_or_else(|e| e.into_inner());
        g.push_back((id, status, buf));
        drop(g);
        q.cond.notify_one();
    }

    fn make_sample_buffer(
        avcc: &[u8],
        desc: &CFRetained<CMFormatDescription>,
        frame_id: u64,
        always_copy: bool,
    ) -> Result<CFRetained<CMSampleBuffer>, String> {
        // SAFETY: 读取静态常量。
        let null_alloc = unsafe { kCFAllocatorNull }.ok_or("kCFAllocatorNull 缺失")?;
        let flags = if always_copy {
            kCMBlockBufferAlwaysCopyDataFlag
        } else {
            0
        };
        let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: 若 always_copy=false，调用方保证 avcc 存活至 sample 消费完成。
        let st = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                avcc.as_ptr().cast_mut().cast(),
                avcc.len(),
                Some(null_alloc),
                std::ptr::null(),
                0,
                avcc.len(),
                flags,
                NonNull::from(&mut block_out),
            )
        };
        if st != 0 || block_out.is_null() {
            return Err(format!("CMBlockBuffer 失败: {st}"));
        }
        // SAFETY: Create 成功返回 +1 引用。
        let block = unsafe { CFRetained::from_raw(NonNull::new_unchecked(block_out)) };

        let timing = [CMSampleTimingInfo {
            duration: CMTIME_INVALID,
            presentationTimeStamp: CMTime {
                epoch: 0,
                flags: CMTimeFlags::Valid,
                value: (frame_id as i64).saturating_mul(10),
                timescale: 600,
            },
            decodeTimeStamp: CMTIME_INVALID,
        }];
        let mut sample_out: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: block/desc 为有效引用，timing 指向栈数组，sample_out 为栈出参。
        let st = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(desc),
                1,
                1,
                timing.as_ptr(),
                0,
                std::ptr::null(),
                NonNull::from(&mut sample_out),
            )
        };
        if st != 0 || sample_out.is_null() {
            return Err(format!("CMSampleBuffer 失败: {st}"));
        }
        // SAFETY: Create 成功返回 +1 引用。
        Ok(unsafe { CFRetained::from_raw(NonNull::new_unchecked(sample_out)) })
    }

    fn create_vt_dec_session(
        desc: &CFRetained<CMFormatDescription>,
        cb_rec: &VtDecompressionOutputCallbackRecord,
        explicit_hw_and_metal: bool,
    ) -> Result<*mut c_void, String> {
        let mut dec_spec_holder: Option<CFRetained<CFDictionary>> = None;
        let mut img_attrs_holder: Option<CFRetained<CFDictionary>> = None;
        if explicit_hw_and_metal {
            // SAFETY: 读取 SDK 常量构造解码规格字典。
            let (en_k, req_k) = unsafe {
                (
                    raw::kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
                    raw::kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                )
            };
            let en_k = en_k.ok_or("缺少 EnableHardwareAcceleratedVideoDecoder")?;
            let req_k = req_k.ok_or("缺少 RequireHardwareAcceleratedVideoDecoder")?;
            let t = CFBoolean::new(true);
            let ks = [
                en_k as *const CFString as *const c_void,
                req_k as *const CFString as *const c_void,
            ];
            let vs = [
                t as *const CFBoolean as *const c_void,
                t as *const CFBoolean as *const c_void,
            ];
            // SAFETY: 栈上 2 个有效 CF 键值对指针。
            let spec = unsafe {
                CFDictionary::new(
                    None,
                    ks.as_ptr() as *mut *const c_void,
                    vs.as_ptr() as *mut *const c_void,
                    2,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
            }
            .ok_or("创建 decoder_spec 失败")?;
            dec_spec_holder = Some(spec);
            img_attrs_holder = Some(make_iosurface_metal_attrs(true)?);
        }

        let spec_ptr = dec_spec_holder
            .as_ref()
            .map(|d| &**d as *const CFDictionary as *const c_void)
            .unwrap_or(std::ptr::null());
        let attr_ptr = img_attrs_holder
            .as_ref()
            .map(|d| &**d as *const CFDictionary as *const c_void)
            .unwrap_or(std::ptr::null());

        let mut session_out: *mut c_void = std::ptr::null_mut();
        // SAFETY: desc、spec_ptr、attr_ptr、cb_rec 在 Create 期间存活，session_out 为栈出参。
        let st = unsafe {
            raw::VTDecompressionSessionCreate(
                std::ptr::null(),
                CFRetained::as_ptr(desc)
                    .as_ptr()
                    .cast::<c_void>()
                    .cast_const(),
                spec_ptr,
                attr_ptr,
                cb_rec as *const VtDecompressionOutputCallbackRecord,
                std::ptr::addr_of_mut!(session_out),
            )
        };
        if st != 0 || session_out.is_null() {
            return Err(format!("VTDecompressionSessionCreate 失败: {st}"));
        }
        // SAFETY: 设置 RealTime = true（D3）。
        let rt_st = unsafe {
            set_vt_prop(
                session_out,
                raw::kVTDecompressionPropertyKey_RealTime,
                CFBoolean::new(true),
            )?
        };
        if rt_st != 0 && rt_st != K_VT_PROPERTY_NOT_SUPPORTED_ERR {
            // SAFETY: 失败清理。
            unsafe {
                raw::VTDecompressionSessionInvalidate(session_out);
                raw::CFRelease(session_out.cast_const());
            }
            return Err(format!("设置 RealTime 失败: {rt_st}"));
        }
        Ok(session_out)
    }

    fn bench_decode_ab_experiments(stream: &GeneratedStream) -> Result<serde_json::Value, String> {
        let desc = create_format_desc(stream.codec, &stream.param_sets)?;

        // ── 1) Sync Inline（decodeFlags=0，无 Mutex/Condvar，直接 AVCC + zero-copy CMBlockBuffer）──
        let run_sync_inline = |always_copy: bool,
                               explicit_attrs: bool|
         -> Result<(LatencySummary, f64, bool, bool), String> {
            let slot = Box::new(InlineSlot {
                called_inline: Cell::new(false),
                status: Cell::new(0),
                width: Cell::new(0),
                height: Cell::new(0),
                last_buf: Cell::new(None),
            });
            let cb_rec = VtDecompressionOutputCallbackRecord {
                callback: inline_decode_cb,
                refcon: (&*slot as *const InlineSlot).cast_mut().cast(),
            };
            let session = create_vt_dec_session(&desc, &cb_rec, explicit_attrs)?;

            let mut all_inline = true;
            let mut saw_async_flag = false;
            let mut p_us = Vec::with_capacity(stream.frames.len());
            let mut wall_start: Option<Instant> = None;

            for (i, f) in stream.frames.iter().enumerate() {
                slot.called_inline.set(false);
                let t0 = Instant::now();
                let sample = make_sample_buffer(&f.avcc, &desc, i as u64, always_copy)?;
                let mut info_flags = 0u32;
                // SAFETY: session/sample 有效，decodeFlags=0 同步解码。
                let st = unsafe {
                    raw::VTDecompressionSessionDecodeFrame(
                        session,
                        CFRetained::as_ptr(&sample).as_ptr().cast(),
                        0,
                        i as *mut c_void,
                        std::ptr::addr_of_mut!(info_flags),
                    )
                };
                let dt_us = t0.elapsed().as_secs_f64() * 1e6;
                if st != 0 || slot.status.get() != 0 {
                    return Err(format!(
                        "sync inline decode 失败: st={st} cb={}",
                        slot.status.get()
                    ));
                }
                if !slot.called_inline.get() {
                    all_inline = false;
                }
                if info_flags & K_VT_DECODE_INFO_ASYNCHRONOUS != 0 {
                    saw_async_flag = true;
                }
                if i == 0 {
                    wall_start = Some(Instant::now());
                } else {
                    p_us.push(dt_us);
                }
            }
            let wall_s = wall_start
                .map(|t| t.elapsed().as_secs_f64())
                .unwrap_or(1e-6)
                .max(1e-9);
            let fps = p_us.len() as f64 / wall_s;
            // SAFETY: 按序释放会话。
            unsafe {
                raw::VTDecompressionSessionWaitForAsynchronousFrames(session);
                raw::VTDecompressionSessionInvalidate(session);
                raw::CFRelease(session.cast_const());
            }
            Ok((
                LatencySummary::from_us(p_us),
                fps,
                all_inline,
                saw_async_flag,
            ))
        };

        let (sync_copy_sum, sync_copy_fps, all_inline_a, saw_async_a) =
            run_sync_inline(true, false)?;
        let (sync_zcopy_sum, sync_zcopy_fps, all_inline_b, _) = run_sync_inline(false, false)?;
        let (sync_explicit_sum, sync_explicit_fps, _, _) = run_sync_inline(false, true)?;

        // ── 2) Async Pipelined（kVTDecodeFrame_EnableAsynchronousDecompression，Depth = 1 / 2 / 4）──
        let run_async_depth = |depth: usize| -> Result<(LatencySummary, f64), String> {
            let queue = Arc::new(AsyncDecQueue {
                done: Mutex::new(VecDeque::with_capacity(depth + 2)),
                cond: Condvar::new(),
            });
            let cb_rec = VtDecompressionOutputCallbackRecord {
                callback: async_decode_cb,
                refcon: Arc::as_ptr(&queue) as *mut c_void,
            };
            let session = create_vt_dec_session(&desc, &cb_rec, false)?;

            let mut submit_times: VecDeque<(u64, Instant)> = VecDeque::with_capacity(depth + 1);
            let mut p_lat_us = Vec::with_capacity(stream.frames.len());
            let mut _last_out: Option<CFRetained<CVPixelBuffer>> = None;

            // 先同步解首帧 IDR 预热，避免冷启动拉偏稳态吞吐。
            let first = &stream.frames[0];
            let s0 = make_sample_buffer(&first.avcc, &desc, 0, true)?;
            let mut flags0 = 0u32;
            // SAFETY: session/s0 有效。
            let st0 = unsafe {
                raw::VTDecompressionSessionDecodeFrame(
                    session,
                    CFRetained::as_ptr(&s0).as_ptr().cast(),
                    K_VT_DECODE_FRAME_ENABLE_ASYNC,
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(flags0),
                )
            };
            if st0 != 0 {
                return Err(format!("async IDR 失败: {st0}"));
            }
            {
                let mut g = queue.done.lock().unwrap_or_else(|e| e.into_inner());
                while g.is_empty() {
                    g = queue.cond.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                let (_, st, buf) = g.pop_front().unwrap();
                if st != 0 {
                    return Err(format!("async IDR cb 失败: {st}"));
                }
                _last_out = buf;
            }

            let wall_start = Instant::now();
            for (idx, f) in stream.frames.iter().enumerate().skip(1) {
                let id = idx as u64;
                let t_sub = Instant::now();
                // 异步解码时 avcc 在 stream.frames 中持续存活，故可用 zero-copy CMBlockBuffer！
                let sample = make_sample_buffer(&f.avcc, &desc, id, false)?;
                let mut info_flags = 0u32;
                // SAFETY: session/sample 有效，avcc 存活于整个函数周期。
                let st = unsafe {
                    raw::VTDecompressionSessionDecodeFrame(
                        session,
                        CFRetained::as_ptr(&sample).as_ptr().cast(),
                        K_VT_DECODE_FRAME_ENABLE_ASYNC,
                        id as usize as *mut c_void,
                        std::ptr::addr_of_mut!(info_flags),
                    )
                };
                if st != 0 {
                    return Err(format!("async decode 失败: {st}"));
                }
                submit_times.push_back((id, t_sub));

                while submit_times.len() >= depth {
                    let mut g = queue.done.lock().unwrap_or_else(|e| e.into_inner());
                    while g.is_empty() {
                        g = queue.cond.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                    let (done_id, cb_st, buf) = g.pop_front().unwrap();
                    drop(g);
                    if cb_st != 0 {
                        return Err(format!("async cb 错误: {cb_st}"));
                    }
                    _last_out = buf;
                    if let Some(pos) = submit_times.iter().position(|(sid, _)| *sid == done_id) {
                        let (_, st_t) = submit_times.remove(pos).unwrap();
                        p_lat_us.push(st_t.elapsed().as_secs_f64() * 1e6);
                    }
                }
            }
            // 排空尾部在途帧。
            while !submit_times.is_empty() {
                let mut g = queue.done.lock().unwrap_or_else(|e| e.into_inner());
                while g.is_empty() {
                    g = queue.cond.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                let (done_id, cb_st, buf) = g.pop_front().unwrap();
                drop(g);
                if cb_st != 0 {
                    return Err(format!("async tail cb 错误: {cb_st}"));
                }
                _last_out = buf;
                if let Some(pos) = submit_times.iter().position(|(sid, _)| *sid == done_id) {
                    let (_, st_t) = submit_times.remove(pos).unwrap();
                    p_lat_us.push(st_t.elapsed().as_secs_f64() * 1e6);
                }
            }
            let wall_s = wall_start.elapsed().as_secs_f64().max(1e-9);
            let fps = p_lat_us.len() as f64 / wall_s;

            // SAFETY: 按序释放会话。
            unsafe {
                raw::VTDecompressionSessionWaitForAsynchronousFrames(session);
                raw::VTDecompressionSessionInvalidate(session);
                drop(queue.done.lock().unwrap_or_else(|e| e.into_inner()));
                raw::CFRelease(session.cast_const());
            }
            Ok((LatencySummary::from_us(p_lat_us), fps))
        };

        let (async_d1_sum, async_d1_fps) = run_async_depth(1)?;
        let (async_d2_sum, async_d2_fps) = run_async_depth(2)?;
        let (async_d4_sum, async_d4_fps) = run_async_depth(4)?;

        Ok(serde_json::json!({
            "sync_flags_0_called_inline": all_inline_a && all_inline_b,
            "sync_flags_0_saw_async_info_flag": saw_async_a,
            "sync_inline_always_copy": { "fps": round2(sync_copy_fps), "latency": sync_copy_sum.to_json() },
            "sync_inline_zero_copy": { "fps": round2(sync_zcopy_fps), "latency": sync_zcopy_sum.to_json() },
            "sync_inline_zero_copy_explicit_metal": { "fps": round2(sync_explicit_fps), "latency": sync_explicit_sum.to_json() },
            "async_depth_1": { "fps": round2(async_d1_fps), "latency": async_d1_sum.to_json() },
            "async_depth_2": { "fps": round2(async_d2_fps), "latency": async_d2_sum.to_json() },
            "async_depth_4": { "fps": round2(async_d4_fps), "latency": async_d4_sum.to_json() },
        }))
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Part 3: `rc-media-apple` FramePumpRenderer & C ABI FFI 开销基准
    // ─────────────────────────────────────────────────────────────────────────

    #[repr(C)]
    #[derive(Default)]
    struct FastFrameOut {
        ptr: u64,
        width: u32,
        height: u32,
        seq: u64,
    }

    struct FastSlot {
        ptr: usize,
        width: u32,
        height: u32,
        seq: u64,
    }

    fn bench_frame_pump_ffi(pool: &PixelPool) -> serde_json::Value {
        let iters = 50_000usize;
        let sample_ptr = pool.ptr_at(0);

        // 1) 现网 rc_apple_media_frame_take() 空轮询（每帧构造 serde_json::json!({}) + String + CString + free）
        let t0 = Instant::now();
        for _ in 0..iters {
            let p: *mut c_char = rc_media_apple::rc_apple_media_frame_take();
            // SAFETY: p 为 rc_apple_media_frame_take 返回的非空 CString。
            unsafe {
                let s = CStr::from_ptr(p).to_bytes();
                black_box(s.len());
                rc_media_apple::rc_apple_media_string_free(p);
            }
        }
        let empty_json_ns = t0.elapsed().as_secs_f64() * 1e9 / iters as f64;

        // 2) 模拟现网有帧命中：serde_json::json!({ptr,w,h,seq}).to_string() + CString + parse + free
        let t1 = Instant::now();
        for i in 0..iters {
            let s = serde_json::json!({
                "ptr": sample_ptr as usize as u64,
                "width": 1920u32,
                "height": 1080u32,
                "seq": i as u64,
            })
            .to_string();
            let cs = std::ffi::CString::new(s).unwrap();
            let raw_p = cs.into_raw();
            // SAFETY: 刚由 CString::into_raw 构造。
            unsafe {
                let parsed: serde_json::Value =
                    serde_json::from_str(CStr::from_ptr(raw_p).to_str().unwrap()).unwrap();
                black_box(parsed["ptr"].as_u64());
                rc_media_apple::rc_apple_media_string_free(raw_p);
            }
        }
        let hit_json_ns = t1.elapsed().as_secs_f64() * 1e9 / iters as f64;

        // 3) 真实生产 `rc_apple_media_frame_take_raw`（原子标志 + #[repr(C)] struct）
        let t_raw_empty = Instant::now();
        for _ in 0..iters {
            let mut out = rc_media_apple::RcAppleFrame::default();
            // SAFETY: out 指向当前栈合法可写内存。
            let hit = unsafe { rc_media_apple::rc_apple_media_frame_take_raw(&mut out) };
            black_box(hit);
        }
        let empty_raw_prod_ns = t_raw_empty.elapsed().as_secs_f64() * 1e9 / iters as f64;

        // 4) 模拟零分配 C struct + CFRetain/CFRelease 锁外释放
        let fast_slot = Mutex::new(FastSlot {
            ptr: 0,
            width: 0,
            height: 0,
            seq: 0,
        });
        let fast_seq = AtomicU64::new(0);
        let fast_empty_flag = AtomicBool::new(false);

        let t2 = Instant::now();
        for _ in 0..iters {
            let mut out = FastFrameOut::default();
            if fast_empty_flag.load(Ordering::Acquire) {
                let mut g = fast_slot.lock().unwrap();
                if g.ptr != 0 {
                    out.ptr = g.ptr as u64;
                    out.width = g.width;
                    out.height = g.height;
                    out.seq = g.seq;
                    g.ptr = 0;
                    fast_empty_flag.store(false, Ordering::Release);
                }
            }
            black_box(out.seq);
        }
        let empty_cstruct_ns = t2.elapsed().as_secs_f64() * 1e9 / iters as f64;

        let t3 = Instant::now();
        for _ in 0..iters {
            // present (CFRetain + swap + release outside lock) + take (struct out-param + CFRelease)
            // SAFETY: sample_ptr 来自存活 pool，retain/release 成对平衡。
            let old = unsafe {
                raw::CFRetain(sample_ptr.cast_const());
                let mut g = fast_slot.lock().unwrap();
                let old = g.ptr;
                g.ptr = sample_ptr as usize;
                g.width = 1920;
                g.height = 1080;
                g.seq = fast_seq.fetch_add(1, Ordering::Relaxed) + 1;
                fast_empty_flag.store(true, Ordering::Release);
                old
            };
            if old != 0 {
                // SAFETY: 旧槽持有 +1 引用。
                unsafe { raw::CFRelease(old as *const c_void) };
            }
            let taken = {
                let mut g = fast_slot.lock().unwrap();
                let out = FastFrameOut {
                    ptr: g.ptr as u64,
                    width: g.width,
                    height: g.height,
                    seq: g.seq,
                };
                g.ptr = 0;
                fast_empty_flag.store(false, Ordering::Release);
                out
            };
            black_box(taken.seq);
            if taken.ptr != 0 {
                // SAFETY: 释放 take 转移的所有权。
                unsafe { raw::CFRelease(taken.ptr as *const c_void) };
            }
        }
        let hit_cstruct_ns = t3.elapsed().as_secs_f64() * 1e9 / iters as f64;

        serde_json::json!({
            "empty_poll_current_json_ns": round2(empty_json_ns),
            "empty_poll_prod_take_raw_ns": round2(empty_raw_prod_ns),
            "empty_poll_c_struct_ns": round2(empty_cstruct_ns),
            "empty_poll_speedup": round2(empty_json_ns / empty_raw_prod_ns.max(0.1)),
            "hit_current_json_roundtrip_ns": round2(hit_json_ns),
            "hit_c_struct_with_cf_retain_release_ns": round2(hit_cstruct_ns),
            "hit_speedup": round2(hit_json_ns / hit_cstruct_ns.max(0.1)),
        })
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 主入口
    // ─────────────────────────────────────────────────────────────────────────

    struct BenchArgs {
        frames: usize,
        json_out: Option<String>,
    }

    fn parse_args() -> BenchArgs {
        let mut frames = 150usize;
        let mut json_out = None;
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--frames" if i + 1 < args.len() => {
                    if let Ok(n) = args[i + 1].parse::<usize>() {
                        frames = n.max(20);
                    }
                    i += 2;
                }
                "--json" if i + 1 < args.len() => {
                    json_out = Some(args[i + 1].clone());
                    i += 2;
                }
                _ => i += 1,
            }
        }
        BenchArgs { frames, json_out }
    }

    pub fn run() -> Result<(), String> {
        let args = parse_args();
        println!("==========================================================================================");
        println!("RabbitControl Apple Media (`rc-decode-vt` + `rc-media-apple`) 硬件编解码性能与瓶颈基准");
        println!(
            "  每场景帧数: {} 帧（16 块高熵桌面 IOSurface '420v' 动态循环池）",
            args.frames
        );
        println!("==========================================================================================\n");

        let specs: &[(u32, u32, u32, u32)] = &[
            (1920, 1080, 120, 20_000_000),
            (2560, 1440, 120, 35_000_000),
            (3440, 1440, 120, 60_000_000),
            (3840, 2160, 120, 80_000_000),
        ];

        println!(
            "{:<15} {:<5} {:>6} {:>7} {:>6} | {:>13} {:>7} | {:>7} {:>13} {:>7} {:>6}",
            "Resolution",
            "Codec",
            "IDR KB",
            "P avgKB",
            "NAL/f",
            "Enc P50/P95",
            "EncFPS",
            "ColdIDR",
            "Dec P50/P95",
            "DecFPS",
            "IOSurf"
        );
        println!("{}", "-".repeat(108));

        let mut matrix_json = Vec::new();
        let mut ab_json = serde_json::Map::new();
        let mut saved_pool_1080: Option<PixelPool> = None;

        for &(w, h, fps, bps) in specs {
            let pool = PixelPool::new(w, h, 16)?;
            for &codec in &[Codec::H264, Codec::Hevc] {
                let stream = generate_real_stream(codec, w, h, fps, bps, args.frames, &pool)?;
                let (cold_idr_ms, dec_sum, dec_max_fps, has_iosurf, pixfmt) =
                    bench_vt_decoder(&stream)?;

                let idr_kb = stream.frames[0].annexb.len() as f64 / 1024.0;
                let p_count = stream.frames.len().saturating_sub(1).max(1);
                let p_avg_kb = stream
                    .frames
                    .iter()
                    .skip(1)
                    .map(|f| f.annexb.len() as f64)
                    .sum::<f64>()
                    / (p_count as f64 * 1024.0);
                let avg_nals = stream
                    .frames
                    .iter()
                    .map(|f| f.nal_count as f64)
                    .sum::<f64>()
                    / stream.frames.len() as f64;
                let enc_sum = LatencySummary::from_us(stream.enc_latencies_us.clone());
                let enc_max_fps = stream.frames.len() as f64 / stream.enc_wall_s.max(1e-9);

                let row = MatrixRow {
                    label: format!("{w}x{h}@{fps}"),
                    codec: codec.as_str(),
                    width: w,
                    height: h,
                    fps,
                    bitrate_mbps: f64::from(bps) / 1e6,
                    idr_kb,
                    p_avg_kb,
                    avg_nals,
                    enc_init_ms: stream.enc_init_us / 1000.0,
                    enc_summary: enc_sum,
                    enc_max_fps,
                    dec_cold_idr_ms: cold_idr_ms,
                    dec_summary: dec_sum,
                    dec_max_fps,
                    dec_out_has_iosurface: has_iosurf,
                    dec_out_pixfmt: pixfmt,
                };

                println!(
                    "{:<15} {:<5} {:>6.1} {:>7.1} {:>6.1} | {:>5.2}/{:<5.2}ms {:>6.1} | {:>5.2}ms {:>5.2}/{:<5.2}ms {:>6.1} {:>6}",
                    row.label,
                    row.codec,
                    row.idr_kb,
                    row.p_avg_kb,
                    row.avg_nals,
                    row.enc_summary.p50_us / 1000.0,
                    row.enc_summary.p95_us / 1000.0,
                    row.enc_max_fps,
                    row.dec_cold_idr_ms,
                    row.dec_summary.p50_us / 1000.0,
                    row.dec_summary.p95_us / 1000.0,
                    row.dec_max_fps,
                    if row.dec_out_has_iosurface { "yes" } else { "NO" },
                );

                matrix_json.push(serde_json::json!({
                    "resolution": format!("{}x{}", row.width, row.height),
                    "fps": row.fps,
                    "codec": row.codec,
                    "target_mbps": row.bitrate_mbps,
                    "idr_kb": round2(row.idr_kb),
                    "p_avg_kb": round2(row.p_avg_kb),
                    "avg_nals": round2(row.avg_nals),
                    "enc_init_ms": round2(row.enc_init_ms),
                    "enc_max_fps": round2(row.enc_max_fps),
                    "enc_latency": row.enc_summary.to_json(),
                    "dec_cold_idr_ms": round2(row.dec_cold_idr_ms),
                    "dec_max_fps": round2(row.dec_max_fps),
                    "dec_latency": row.dec_summary.to_json(),
                    "dec_out_has_iosurface": row.dec_out_has_iosurface,
                    "dec_out_pixfmt_hex": format!("0x{:08x}", row.dec_out_pixfmt),
                }));

                // 对 1080p 与 4K 跑全套解码瓶颈 A/B 归因微基准
                if (w == 1920 && h == 1080) || (w == 3840 && h == 2160) {
                    let key = format!("{w}x{h}_{}", codec.as_str());
                    let bs = bench_bitstream_conversion(&stream);
                    let blk = bench_cmblockbuffer_copy(&stream)?;
                    let ab = bench_decode_ab_experiments(&stream)?;
                    ab_json.insert(
                        key,
                        serde_json::json!({
                            "bitstream_annexb_to_avcc": bs,
                            "cmblockbuffer_copy": blk,
                            "vt_decode_modes": ab,
                        }),
                    );
                }
            }
            if w == 1920 && h == 1080 {
                saved_pool_1080 = Some(pool);
            }
        }

        println!("\n=== Part 2: 解码热路径逐段瓶颈归因（1080p & 4K，真实硬编帧 A/B 对照）===");
        for (k, v) in &ab_json {
            let bs = &v["bitstream_annexb_to_avcc"];
            let blk = &v["cmblockbuffer_copy"];
            let vt = &v["vt_decode_modes"];
            println!(
                "\n[{k}] (P 帧均值 {:.1} KB)",
                bs["avg_frame_kb"].as_f64().unwrap_or(0.0)
            );
            println!(
                "  2A. Annex-B→AVCC/HVCC: 现网标量+Vec分配 = {:>6.2} µs ({:.2} GB/s)  vs  单遍SWAR零分配 = {:>6.2} µs ({:.2} GB/s)  [提速 {:.2}x]",
                bs["current_scalar_us"].as_f64().unwrap_or(0.0),
                bs["current_gb_s"].as_f64().unwrap_or(0.0),
                bs["swar_single_pass_us"].as_f64().unwrap_or(0.0),
                bs["swar_gb_s"].as_f64().unwrap_or(0.0),
                bs["speedup"].as_f64().unwrap_or(0.0),
            );
            println!(
                "  2B. CMBlockBuffer 封装: AlwaysCopyDataFlag = {:>6.2} µs  vs  ZeroCopy(flags=0) = {:>6.2} µs  [省 {:.2} µs, {:.2}x]",
                blk["always_copy_us"].as_f64().unwrap_or(0.0),
                blk["zero_copy_us"].as_f64().unwrap_or(0.0),
                blk["saved_us"].as_f64().unwrap_or(0.0),
                blk["speedup"].as_f64().unwrap_or(0.0),
            );
            println!(
                "  2C. decodeFlags=0 实证: 回调同步内联={}  infoFlags含Async={}",
                vt["sync_flags_0_called_inline"], vt["sync_flags_0_saw_async_info_flag"],
            );
            for (name, field) in [
                (
                    "Sync Inline (AlwaysCopy, 无邮箱锁)",
                    "sync_inline_always_copy",
                ),
                (
                    "Sync Inline (ZeroCopy,   无邮箱锁)",
                    "sync_inline_zero_copy",
                ),
                (
                    "Sync Inline (ZeroCopy + 显式Metal)",
                    "sync_inline_zero_copy_explicit_metal",
                ),
                ("Async Decode (Depth=1)", "async_depth_1"),
                ("Async Decode (Depth=2, 符合铁律1)", "async_depth_2"),
                ("Async Decode (Depth=4, 硬件上限)", "async_depth_4"),
            ] {
                let m = &vt[field];
                let lat = &m["latency"];
                println!(
                    "      {:<36}: {:>6.1} fps | P50={:>5.2}ms P95={:>5.2}ms P99={:>5.2}ms",
                    name,
                    m["fps"].as_f64().unwrap_or(0.0),
                    lat["p50_us"].as_f64().unwrap_or(0.0) / 1000.0,
                    lat["p95_us"].as_f64().unwrap_or(0.0) / 1000.0,
                    lat["p99_us"].as_f64().unwrap_or(0.0) / 1000.0,
                );
            }
        }

        let pump_json = bench_frame_pump_ffi(saved_pool_1080.as_ref().unwrap());
        println!("\n=== Part 3: `rc-media-apple` FramePumpRenderer & C ABI FFI 开销对比 ===");
        println!(
            "  空轮询 take: 现网 JSON+CString = {:>7.1} ns/op  vs  原子标志+C struct = {:>5.1} ns/op  [提速 {:.1}x]",
            pump_json["empty_poll_current_json_ns"].as_f64().unwrap_or(0.0),
            pump_json["empty_poll_c_struct_ns"].as_f64().unwrap_or(0.0),
            pump_json["empty_poll_speedup"].as_f64().unwrap_or(0.0),
        );
        println!(
            "  有帧命中:   现网 JSON 序列化+解析 = {:>7.1} ns/op  vs  C struct+CFRetain/Release = {:>5.1} ns/op  [提速 {:.1}x]",
            pump_json["hit_current_json_roundtrip_ns"].as_f64().unwrap_or(0.0),
            pump_json["hit_c_struct_with_cf_retain_release_ns"].as_f64().unwrap_or(0.0),
            pump_json["hit_speedup"].as_f64().unwrap_or(0.0),
        );

        let full_report = serde_json::json!({
            "frames_per_scenario": args.frames,
            "matrix": matrix_json,
            "decode_ab_attribution": ab_json,
            "frame_pump_ffi": pump_json,
        });

        if let Some(path) = args.json_out {
            let body = serde_json::to_string_pretty(&full_report)
                .map_err(|e| format!("序列化 JSON 失败: {e}"))?;
            fs::write(&path, body).map_err(|e| format!("写入 {path} 失败: {e}"))?;
            println!("\nJSON 报告已保存至: {path}");
        }

        Ok(())
    }
}
