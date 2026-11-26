//! RabbitControl 的 VideoToolbox 硬件解码后端（任务卡 T05，D1–D5 解码侧）。
//!
//! # 职责
//! 用 `VTDecompressionSession` 实现 [`rc_decode::Decoder`]（H.264），消费
//! `rc-encode-vt` 产出的 **Annex-B** 码流，交出零拷贝的
//! [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)。
//!
//! # Annex-B → AVCC（坑 #7）
//! VideoToolbox 的解码输入是 AVCC 风格（4 字节长度前缀 NAL + 参数集经
//! `CMVideoFormatDescription` 传递），而本项目管线统一消费 Annex-B
//!（编码侧在输出侧重排并在 `EncoderCaps::bitstream_format` 里声明）。
//! 解码端因此在入口做一次 Annex-B → AVCC 转换（纯逻辑在 [`h264`]，
//! 任何畸形输入在进 FFI 之前被拒绝）；[`VtDecoderConfig::bitstream_format`]
//! 必须与编码端声明一致，不匹配在构造期报错——两种封装不能混用。
//!
//! # 延迟红线（契约 D1–D5 的落点）
//! | 契约 | 落点 |
//! |---|---|
//! | D1（零拷贝） | 输出 `CVPixelBuffer` 句柄直通（回调里 retain，会话保活到下一次 decode）；生产路径不 LockBaseAddress、无 CPU 回读 |
//! | D3（低延迟） | `kVTDecompressionPropertyKey_RealTime = true` + 严格单帧在途（无解码器内部缓冲可观感） |
//! | D4（缺参考不 panic） | [`RefChainGate`] 纯状态机：首个 IDR 前 / 断链 → `DecodeOutput::NeedKeyframe`；任何解码错误后断链（低延迟优先于完整，恢复靠下一个 IDR） |
//! | D5（进一出一） | VT 回调异步 → 帧号邮箱（[`mailbox`]）同步收集：提交带帧号、等到**该帧**回调才返回，顺序由结构保证 |
//!
//! # 会话生命周期
//! 会话**惰性**建立：解码器构造不碰 FFI，首个携带 SPS/PPS 的 IDR 到达才
//! 从参数集构建格式描述并创建会话；之后 SPS 变化（分辨率切换）自动重建。
//!
//! # 时间注入
//! 与 [`rc_decode`] 的契约一致：内部维护逻辑时钟（上次被接受调用的完成
//! 时刻），预计完成时刻（时钟 + 估算耗时）超过 `deadline_ts` 时直接返回
//! [`Error::Timeout`](rc_types::Error::Timeout)（不阻塞、不改变内部状态）；
//! 实际等待的墙钟预算 = `deadline_ts − 逻辑时钟`，到点回调未到达同样超时
//!（迟到输出在下一次调用开头丢弃，且因跳帧而断链——恢复靠 IDR）。
//!
//! 非 macOS 目标上本 crate 编译为空壳（仅保留纯逻辑模块），保证 workspace
//! 在任何平台都能构建与测试。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

/// Annex-B 解析 / Annex-B → AVCC 转换 / SPS 尺寸解析（纯函数，跨平台可测）。
///
/// 调用方只有 macOS 的 `VtDecoder` / ffi 模块；非 macOS 空壳构建里它没有
/// 调用者但仍然编译 + 跑单测（保持纯逻辑在任何目标上可验证）。
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
pub(crate) mod h264;

/// HEVC Annex-B 解析 / Annex-B → HVCC 转换与参数集提取（纯函数，跨平台可测）。
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
pub(crate) mod h265;

/// AV1 OBU 解析 / Sequence Header 提取与 ISO BMFF `av1C` 构造（纯函数，跨平台可测）。
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
pub(crate) mod av1_obu;

/// 帧序邮箱：解码回调 → 等待线程的单帧结果收集（D5 顺序核心，纯 Rust）。
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
pub(crate) mod mailbox;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) mod ffi;

use rc_encode::{BitstreamFormat, Codec};
use rc_types::Resolution;

/// 预估单帧解码耗时（微秒）：deadline 预检用（与 rc-decode 假解码器的
/// 默认 3ms 一致，即任务卡的解码延迟预算）。
pub const ESTIMATED_DECODE_COST_US: u32 = 3_000;

/// 参考链状态机（D4）：是否持有可用的前序参考。
///
/// 语义与 `rc-decode` 假解码器一致：
/// - 初始即断裂（「入会后从未解出过 IDR」）——[`needs_keyframe`](rc_decode::Decoder::needs_keyframe)
///   起始为 `true`；
/// - 成功解出 IDR 重建链；断链后 P 帧一律 `NeedKeyframe`，直到下一个 IDR；
/// - 解码失败 / 超时跳帧都会断链（错误恢复统一走「请求关键帧」路径）。
///
/// 纯逻辑、无 FFI——D4 契约因此可以在无 GPU 的 CI 上直接断言。
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct RefChainGate {
    chain_ok: bool,
}

#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
impl RefChainGate {
    /// 新建：链断裂（首个 IDR 之前）。
    pub(crate) fn new() -> Self {
        Self { chain_ok: false }
    }

    /// 链是否完整。
    pub(crate) fn is_ok(&self) -> bool {
        self.chain_ok
    }

    /// 成功解出一个 IDR：链重建。
    pub(crate) fn mark_idr_decoded(&mut self) {
        self.chain_ok = true;
    }

    /// 断链（丢帧 / 解码失败 / 超时跳帧）。
    pub(crate) fn break_chain(&mut self) {
        self.chain_ok = false;
    }
}

/// 动态探测当前 Apple 设备是否支持 AV1 硬件解码（`VTIsHardwareDecodeSupported('av01')`）。
///
/// - Apple Silicon M1/M2 及旧款设备返回 `false`；
/// - Apple Silicon M3/M4、iPhone 15 Pro (A17 Pro) 及更新支持 AV1 硬解的设备返回 `true`；
/// - 非 macOS/iOS 平台恒返回 `false`。
pub fn supports_hardware_av1_decode() -> bool {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        ffi::is_hardware_av1_decode_supported()
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        false
    }
}

/// 解码器配置。
///
/// `bitstream_format` 必须与发送端 `EncoderCaps::bitstream_format` 的声明
/// 一致（H.264/HEVC 必须为 `AnnexB`，AV1 必须为 `Obu`，坑 #7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtDecoderConfig {
    /// 目标编解码器（支持 H.264、HEVC/H.265 与 AV1）。
    pub codec: Codec,
    /// 输入码流封装格式：
    /// - H.264 / HEVC **必须为 [`BitstreamFormat::AnnexB`]**（内部转 AVCC/HVCC 喂 VT）；
    /// - AV1 **必须为 [`BitstreamFormat::Obu`]**（内部解析 `OBU_SEQUENCE_HEADER` 构造 `av1C`）。
    ///
    /// 不匹配在构造期报错而不是运行期花屏。
    pub bitstream_format: BitstreamFormat,
    /// 接受的最大帧尺寸（SPS / Sequence Header 声明超限的码流在进 FFI 前被拒绝，恶意/损坏
    /// 码流防御）。默认 4096×4096，与编码侧 caps 的保守上限一致。
    pub max_resolution: Resolution,
}

impl Default for VtDecoderConfig {
    /// Annex-B 输入、H.264 默认编解码器、4096×4096 上限——与 rc-encode-vt 的 caps 声明对齐。
    fn default() -> Self {
        VtDecoderConfig {
            codec: Codec::H264,
            bitstream_format: BitstreamFormat::AnnexB,
            max_resolution: Resolution::new(4096, 4096),
        }
    }
}

impl VtDecoderConfig {
    /// 校验配置。非法项以 [`Error::InvalidConfig`](rc_types::Error::InvalidConfig)
    /// 返回，信息含字段与期望值。
    pub fn validate(&self) -> rc_types::Result<()> {
        match (self.codec, self.bitstream_format) {
            (Codec::H264 | Codec::Hevc, BitstreamFormat::AnnexB)
            | (Codec::Av1, BitstreamFormat::Obu) => {}
            (Codec::H264 | Codec::Hevc, other) => {
                return Err(rc_types::Error::InvalidConfig(format!(
                    "rc-decode-vt 只接受 Annex-B 输入（EncoderCaps::bitstream_format 必须为 annexb，当前 {}）；\
                     VideoToolbox 走 AVCC（长度前缀 + 格式描述），解码端在内部转换——两种封装不能混用（坑 #7）",
                    other.as_str()
                )));
            }
            (Codec::Av1, other) => {
                return Err(rc_types::Error::InvalidConfig(format!(
                    "rc-decode-vt AV1 解码只接受 OBU 输入（EncoderCaps::bitstream_format 必须为 obu，当前 {}）",
                    other.as_str()
                )));
            }
        }
        if self.max_resolution.width == 0 || self.max_resolution.height == 0 {
            return Err(rc_types::Error::InvalidConfig(
                "rc-decode-vt: max_resolution must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    use super::{ffi, RefChainGate, VtDecoderConfig, ESTIMATED_DECODE_COST_US};
    use rc_capture::FrameTexture;
    use rc_decode::{DecodeOutput, DecodeSubmitStatus, DecodedFrame, Decoder};
    use rc_encode::Codec;
    use rc_types::{Error, FrameKind, Result, Timestamp};
    use std::collections::VecDeque;
    use std::fmt;
    use std::ops::Range;
    use std::time::{Duration, Instant};

    /// `prepare_frame` 结果：校验与 Annex-B→AVCC/HVCC 准备完成，或需要请求关键帧。
    enum PrepareOutcome {
        Ready { content_is_idr: bool },
        NeedKeyframe,
    }

    /// VideoToolbox 硬件 H.264/HEVC 解码器（实现 [`Decoder`](rc_decode::Decoder)）。
    ///
    /// - 构造不碰 FFI：会话惰性建立（首个带 SPS/PPS 的 IDR），
    ///   [`VtDecoderConfig::validate`] 的失败在构造期暴露；
    /// - 输出 [`FrameTexture::CVPixelBuffer`] 零拷贝句柄（D1），存活期与
    ///   采集侧句柄语义对称：**下一次 `decode` / `recv_decoded` / 解码器销毁之前有效**，
    ///   渲染器要跨帧持有必须自行 retain；
    /// - 支持 2-deep 异步流水线解码（[`Decoder::submit_decode`] / [`Decoder::recv_decoded`]，
    ///   严格遵守铁律 1 `in_flight ≤ 2`）。
    pub struct VtDecoder {
        cfg: VtDecoderConfig,
        gate: RefChainGate,
        /// 惰性建立的解码会话；参数集变化时先构造新会话成功后再排空替换。
        session: Option<ffi::DecompressionSession>,
        /// 会话重建期间从旧会话排空保留的已完成输出（FIFO，容量 ≤ 2），附带该帧在旧会话上的
        /// 参考链健康态 `old_chain_ok`，确保每个已提交 token 均能由 `recv_decoded` 交付，
        /// 既不与上层失步，也不放行已断链的旧会话 P 帧（D4）。
        drained_outputs: VecDeque<(ffi::PipelinedDecodeOutput, bool)>,
        /// 上一帧交付给调用方的输出缓冲保活引用（下一次 `decode` / `recv_decoded` / 销毁前有效）。
        last_output: Option<ffi::OwnedDecodeOutput>,
        /// 当前会话对应的参数集键（H.264 为 SPS；HEVC 为 VPS||SPS||PPS，检测变化与分辨率切换）。
        session_params: Option<Vec<u8>>,
        /// 会话内单调帧号（邮箱匹配 + PTS 单调；提交前递增永不复用）。
        frame_id: u64,
        /// 逻辑时钟（微秒，与 `Timestamp` 同为 u32 域）：按非重叠墙钟解码区间推进。
        clock_us: u32,
        /// 上一次推进逻辑时钟的墙钟完成点，用于消除 2-deep 并行重叠双算与渲染等待污染。
        last_clock_advance: Option<Instant>,
        /// 可复用的 NAL 范围暂存区（避免每帧分配 `Vec<(u8, &[u8])>`）。
        nals_scratch: Vec<(u8, Range<usize>)>,
        /// 可复用的 AV1 OBU 范围暂存区（零拷贝切分 OBU 序列，不动 H.264/HEVC 热路径）。
        obu_scratch: Vec<crate::av1_obu::ObuRange>,
        /// 当前 AV1 会话 Sequence Header 的 `reduced_still_picture_header` 标志。
        av1_reduced_still_header: bool,
        /// 可复用的 AVCC/HVCC/AV1 sample 拼装暂存区（避免每帧重复分配 `Vec<u8>`）。
        avcc_scratch: Vec<u8>,
    }

    impl fmt::Debug for VtDecoder {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // session 是不透明 FFI 句柄，不展开（打印指针地址无诊断价值）。
            f.debug_struct("VtDecoder")
                .field("cfg", &self.cfg)
                .field("chain_ok", &self.gate.is_ok())
                .field("has_session", &self.session.is_some())
                .field("frame_id", &self.frame_id)
                .finish_non_exhaustive()
        }
    }

    impl VtDecoder {
        /// 创建解码器：仅校验配置（不碰 FFI，任何平台/无 GPU 会话可构造）。
        ///
        /// 真正的 VideoToolbox 会话在首个带 SPS/PPS 的 IDR 到达时建立；
        /// 建不了（如无可用解码器）在那一步报 [`Error::Decoder`]。
        pub fn new(cfg: VtDecoderConfig) -> Result<Self> {
            cfg.validate()?;
            Ok(VtDecoder {
                cfg,
                gate: RefChainGate::new(),
                session: None,
                drained_outputs: VecDeque::with_capacity(ffi::PIPELINED_MAX_IN_FLIGHT),
                last_output: None,
                session_params: None,
                frame_id: 0,
                clock_us: 0,
                last_clock_advance: None,
                nals_scratch: Vec::with_capacity(8),
                obu_scratch: Vec::new(),
                av1_reduced_still_header: false,
                avcc_scratch: Vec::new(),
            })
        }

        /// 已提交给 VideoToolbox 的帧数（测试断言「进一出一」用）。
        pub fn submitted_frames(&self) -> u64 {
            self.frame_id
        }

        /// 当前活跃会话中是否存在尚未回调的 IDR（不计入旧销毁会话的 `drained_outputs`）。
        fn has_idr_in_flight(&self) -> bool {
            self.session
                .as_ref()
                .is_some_and(ffi::DecompressionSession::has_idr_in_flight)
        }

        /// 推进逻辑时钟（按实测墙钟耗时，饱和到 u32::MAX）并返回完成时刻。
        fn advance_clock(&mut self, elapsed: Duration) -> Timestamp {
            let us = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX);
            self.clock_us = self.clock_us.saturating_add(us);
            Timestamp(self.clock_us)
        }

        /// 按非重叠硬件解码区间推进逻辑时钟：在 2-deep 并行重叠或调用方 VSync 阻塞场景下，
        /// 仅计入 `[max(started, last_clock_advance), completed_at]` 增量，防止双算导致假超时。
        pub(crate) fn advance_clock_span(
            &mut self,
            started: Instant,
            completed_at: Instant,
        ) -> Timestamp {
            let start_bound = match self.last_clock_advance {
                Some(prev) if prev > started => prev,
                _ => started,
            };
            let delta = completed_at.saturating_duration_since(start_bound);
            let should_update = match self.last_clock_advance {
                None => true,
                Some(prev) => completed_at > prev,
            };
            if should_update {
                self.last_clock_advance = Some(completed_at);
            }
            self.advance_clock(delta)
        }
    }

    impl Decoder for VtDecoder {
        fn decode(
            &mut self,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
        ) -> Result<DecodeOutput> {
            match self.prepare_frame(data, kind, deadline_ts, false)? {
                PrepareOutcome::NeedKeyframe => Ok(DecodeOutput::NeedKeyframe),
                PrepareOutcome::Ready { content_is_idr } => {
                    self.finish_sync_decode(content_is_idr, deadline_ts)
                }
            }
        }

        fn needs_keyframe(&self) -> bool {
            // 参考链断裂（且无在途 IDR 即将重建链）即需要关键帧。
            !self.gate.is_ok() && !self.has_idr_in_flight()
        }

        fn flush(&mut self) -> Result<()> {
            self.drained_outputs.clear();
            if let Some(session) = self.session.as_mut() {
                session.wait_and_clear();
            }
            Ok(())
        }

        fn supports_pipelined_decode(&self) -> bool {
            true
        }

        fn submit_decode(
            &mut self,
            token: u64,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
        ) -> Result<DecodeSubmitStatus> {
            if self.in_flight_count() >= ffi::PIPELINED_MAX_IN_FLIGHT {
                return Err(Error::Decoder(format!(
                    "pipelined decode in_flight limit ({}) reached",
                    ffi::PIPELINED_MAX_IN_FLIGHT
                )));
            }
            match self.prepare_frame(data, kind, deadline_ts, true)? {
                PrepareOutcome::NeedKeyframe => Ok(DecodeSubmitStatus::NeedKeyframe),
                PrepareOutcome::Ready { content_is_idr } => {
                    let Some(session) = self.session.as_mut() else {
                        return Ok(DecodeSubmitStatus::NeedKeyframe);
                    };
                    let submit_id = self
                        .frame_id
                        .checked_add(1)
                        .ok_or_else(|| Error::Decoder("会话帧号耗尽 u64".to_string()))?;
                    // 提交前即推进 frame_id，即使 VT 返回非零错误也绝不复用 submit_id（防邮箱碰撞）。
                    self.frame_id = submit_id;
                    if let Err(e) = session.submit_decode_frame(
                        &self.avcc_scratch,
                        submit_id,
                        token,
                        content_is_idr,
                    ) {
                        // D4：提交失败导致该帧丢失，立即打断参考链并请求关键帧。
                        self.gate.break_chain();
                        return Err(Error::Decoder(e.to_string()));
                    }
                    Ok(DecodeSubmitStatus::Submitted)
                }
            }
        }

        fn recv_decoded(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<(u64, Result<DecodeOutput>)>> {
            let (item, drained_chain_ok) =
                if let Some((drained, old_chain_ok)) = self.drained_outputs.pop_front() {
                    (drained, Some(old_chain_ok))
                } else {
                    let Some(session) = self.session.as_mut() else {
                        return Ok(None);
                    };
                    let Some(item) = session.recv_decoded_frame(timeout) else {
                        return Ok(None);
                    };
                    (item, None)
                };
            let from_drained = drained_chain_ok.is_some();
            let elapsed = item.completed_at.saturating_duration_since(item.started);
            match item.result {
                Ok(out) => {
                    let done_ts = self.advance_clock_span(item.started, item.completed_at);
                    if let Some(old_chain_ok) = drained_chain_ok {
                        // 旧会话排空帧不修改新会话门控；若旧会话在该帧处参考链已断（D4），仍丢弃旧 P 帧防花屏。
                        if !item.content_is_idr && !old_chain_ok {
                            return Ok(Some((item.token, Ok(DecodeOutput::NeedKeyframe))));
                        }
                    } else if item.content_is_idr {
                        self.gate.mark_idr_decoded();
                    } else if !self.gate.is_ok() {
                        // D4：当前会话前序在途帧已打断参考链，丢弃本在途 P 帧防花屏。
                        return Ok(Some((item.token, Ok(DecodeOutput::NeedKeyframe))));
                    }
                    let ptr = out.as_ptr();
                    let width = out.width;
                    let height = out.height;
                    self.last_output = Some(out);
                    Ok(Some((
                        item.token,
                        Ok(DecodeOutput::Frame(DecodedFrame {
                            texture: FrameTexture::CVPixelBuffer(ptr),
                            decode_done_ts: done_ts,
                            width,
                            height,
                        })),
                    )))
                }
                Err(ffi::DecodeFailure::Timeout) => {
                    if !from_drained {
                        self.gate.break_chain();
                    }
                    self.advance_clock_span(item.started, item.completed_at);
                    Ok(Some((item.token, Err(Error::Timeout(elapsed)))))
                }
                Err(ffi::DecodeFailure::Callback(_)) if !item.content_is_idr => {
                    if !from_drained {
                        self.gate.break_chain();
                    }
                    self.advance_clock_span(item.started, item.completed_at);
                    Ok(Some((item.token, Ok(DecodeOutput::NeedKeyframe))))
                }
                Err(e) => {
                    if !from_drained {
                        self.gate.break_chain();
                    }
                    Ok(Some((item.token, Err(Error::Decoder(e.to_string())))))
                }
            }
        }

        fn in_flight_count(&self) -> usize {
            self.drained_outputs.len()
                + self
                    .session
                    .as_ref()
                    .map_or(0, ffi::DecompressionSession::in_flight_count)
        }
    }

    impl VtDecoder {
        fn finish_sync_decode(
            &mut self,
            content_is_idr: bool,
            deadline_ts: Timestamp,
        ) -> Result<DecodeOutput> {
            self.drained_outputs.clear();
            let Some(session) = self.session.as_mut() else {
                return Ok(DecodeOutput::NeedKeyframe);
            };
            let frame_id = self.frame_id;
            self.frame_id = frame_id
                .checked_add(1)
                .ok_or_else(|| Error::Decoder("会话帧号耗尽 u64".to_string()))?;
            let max_wait_us = if content_is_idr { 100_000 } else { 25_000 };
            let raw_wait_us = deadline_ts.0.saturating_sub(self.clock_us).max(1);
            let wait = Duration::from_micros(u64::from(raw_wait_us.min(max_wait_us)));
            let started = Instant::now();
            match session.decode_frame(&self.avcc_scratch, frame_id, wait) {
                Ok(out) => {
                    let done_ts = self.advance_clock_span(started, Instant::now());
                    if content_is_idr {
                        self.gate.mark_idr_decoded();
                    }
                    let ptr = out.as_ptr();
                    let width = out.width;
                    let height = out.height;
                    self.last_output = Some(out);
                    Ok(DecodeOutput::Frame(DecodedFrame {
                        texture: FrameTexture::CVPixelBuffer(ptr),
                        decode_done_ts: done_ts,
                        width,
                        height,
                    }))
                }
                Err(ffi::DecodeFailure::Timeout) => {
                    self.gate.break_chain();
                    let now = Instant::now();
                    let elapsed = now.saturating_duration_since(started);
                    self.advance_clock_span(started, now);
                    Err(Error::Timeout(elapsed))
                }
                Err(ffi::DecodeFailure::Callback(_)) if !content_is_idr => {
                    self.gate.break_chain();
                    self.advance_clock_span(started, Instant::now());
                    Ok(DecodeOutput::NeedKeyframe)
                }
                Err(e) => {
                    self.gate.break_chain();
                    Err(Error::Decoder(e.to_string()))
                }
            }
        }

        fn prepare_frame(
            &mut self,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
            pipelined: bool,
        ) -> Result<PrepareOutcome> {
            // 若调用方会话时间戳从高值回绕或被重置，归零逻辑时钟以防假超时。
            if deadline_ts.0 < self.clock_us
                && self.clock_us.wrapping_sub(deadline_ts.0) > u32::MAX / 2
            {
                self.clock_us = 0;
                self.last_clock_advance = None;
            }
            match self.cfg.codec {
                Codec::H264 => self.prepare_h264(data, kind, deadline_ts, pipelined),
                Codec::Hevc => self.prepare_hevc(data, kind, deadline_ts, pipelined),
                Codec::Av1 => self.prepare_av1(data, kind, deadline_ts, pipelined),
            }
        }

        fn replace_session(
            &mut self,
            new_session: ffi::DecompressionSession,
            new_params: Vec<u8>,
            pipelined: bool,
        ) {
            if let Some(mut old) = self.session.take() {
                if pipelined {
                    let mut old_chain_ok = self.gate.is_ok();
                    for out in old.drain_all_in_flight() {
                        match &out.result {
                            Ok(_) if out.content_is_idr => old_chain_ok = true,
                            Err(_) => old_chain_ok = false,
                            Ok(_) => {}
                        }
                        self.drained_outputs.push_back((out, old_chain_ok));
                    }
                } else {
                    old.wait_and_clear();
                    self.drained_outputs.clear();
                }
            }
            // 新会话 DPB 为空，须在其自身解出首张 IDR 后再恢复参考链门控。
            self.gate.break_chain();
            self.session = Some(new_session);
            self.session_params = Some(new_params);
        }

        fn prepare_h264(
            &mut self,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
            pipelined: bool,
        ) -> Result<PrepareOutcome> {
            crate::h264::split_nals_into(data, &mut self.nals_scratch).ok_or_else(|| {
                Error::InvalidFrame(format!(
                    "rc-decode-vt: 码流不是合法 Annex-B（配置声明 bitstream_format={}）：{} 字节输入",
                    self.cfg.bitstream_format.as_str(),
                    data.len()
                ))
            })?;

            let content_is_idr = self.nals_scratch.iter().any(|(t, _)| *t == 5);
            match (kind, content_is_idr) {
                (FrameKind::Idr, false) => {
                    return Err(Error::InvalidFrame(
                        "声明为 IDR 但码流不含 IDR slice（nal_unit_type 5）".to_string(),
                    ))
                }
                (FrameKind::P, true) => {
                    return Err(Error::InvalidFrame(
                        "声明为 P 但码流含 IDR slice（nal_unit_type 5）".to_string(),
                    ))
                }
                _ => {}
            }

            let est_done_us = self.clock_us.saturating_add(ESTIMATED_DECODE_COST_US);
            if est_done_us > deadline_ts.0 {
                return Err(Error::Timeout(Duration::from_micros(u64::from(
                    est_done_us - deadline_ts.0,
                ))));
            }

            let chain_available = self.gate.is_ok() || (pipelined && self.has_idr_in_flight());
            if !chain_available && !content_is_idr {
                return Ok(PrepareOutcome::NeedKeyframe);
            }

            let pending_rebuild = if content_is_idr {
                let (sps, pps) = crate::h264::parameter_sets_from_ranges(data, &self.nals_scratch);
                match (sps, pps) {
                    (Some(sps), Some(pps)) => {
                        let needs_rebuild = match self.session_params.as_deref() {
                            Some(cur) => cur != sps,
                            None => true,
                        };
                        if needs_rebuild {
                            if let Some((w, h)) = crate::h264::sps_dimensions(sps) {
                                if w > self.cfg.max_resolution.width
                                    || h > self.cfg.max_resolution.height
                                {
                                    return Err(Error::InvalidFrame(format!(
                                        "SPS 声明 {w}x{h} 超过解码上限 {}x{}",
                                        self.cfg.max_resolution.width,
                                        self.cfg.max_resolution.height
                                    )));
                                }
                            }
                            Some((sps, pps))
                        } else {
                            None
                        }
                    }
                    _ if self.session.is_none() => return Ok(PrepareOutcome::NeedKeyframe),
                    _ => None,
                }
            } else {
                None
            };

            if pending_rebuild.is_none() && self.session.is_none() {
                return Ok(PrepareOutcome::NeedKeyframe);
            }

            // 先完成 Annex-B → AVCC 转换校验，确保全部可能失败步骤都在 replace_session 之前完成。
            if crate::h264::nals_ranges_to_avcc_into(
                data,
                &self.nals_scratch,
                &mut self.avcc_scratch,
            )
            .is_none()
            {
                return Err(Error::InvalidFrame(
                    "帧不含任何 slice NAL（无法解码出画面）".to_string(),
                ));
            }

            if let Some((sps, pps)) = pending_rebuild {
                let new_session =
                    ffi::DecompressionSession::new(sps, pps).map_err(Error::Decoder)?;
                self.replace_session(new_session, sps.to_vec(), pipelined);
            }

            Ok(PrepareOutcome::Ready { content_is_idr })
        }

        fn prepare_hevc(
            &mut self,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
            pipelined: bool,
        ) -> Result<PrepareOutcome> {
            crate::h265::split_nals_into(data, &mut self.nals_scratch).ok_or_else(|| {
                Error::InvalidFrame(format!(
                    "rc-decode-vt: HEVC 码流不是合法 Annex-B（配置声明 bitstream_format={}）：{} 字节输入",
                    self.cfg.bitstream_format.as_str(),
                    data.len()
                ))
            })?;

            let content_is_idr = self
                .nals_scratch
                .iter()
                .any(|(t, _)| crate::h265::is_irap_nal(*t));
            match (kind, content_is_idr) {
                (FrameKind::Idr, false) => {
                    return Err(Error::InvalidFrame(
                        "声明为 IDR 但 HEVC 码流不含 IRAP/IDR slice".to_string(),
                    ))
                }
                (FrameKind::P, true) => {
                    return Err(Error::InvalidFrame(
                        "声明为 P 但 HEVC 码流含 IRAP/IDR slice".to_string(),
                    ))
                }
                _ => {}
            }

            let est_done_us = self.clock_us.saturating_add(ESTIMATED_DECODE_COST_US);
            if est_done_us > deadline_ts.0 {
                return Err(Error::Timeout(Duration::from_micros(u64::from(
                    est_done_us - deadline_ts.0,
                ))));
            }

            let chain_available = self.gate.is_ok() || (pipelined && self.has_idr_in_flight());
            if !chain_available && !content_is_idr {
                return Ok(PrepareOutcome::NeedKeyframe);
            }

            let pending_rebuild = if content_is_idr {
                let params = crate::h265::parameter_sets_from_ranges(data, &self.nals_scratch);
                match (params.vps, params.sps, params.pps) {
                    (Some(vps), Some(sps), Some(pps)) => {
                        let needs_rebuild = match self.session_params.as_deref() {
                            Some(cur) => !crate::h265::params_key_matches(cur, vps, sps, pps),
                            None => true,
                        };
                        if needs_rebuild {
                            let vps_len = u16::try_from(vps.len()).map_err(|_| {
                                Error::InvalidFrame("HEVC VPS 长度超过 u16::MAX".to_string())
                            })?;
                            let sps_len = u16::try_from(sps.len()).map_err(|_| {
                                Error::InvalidFrame("HEVC SPS 长度超过 u16::MAX".to_string())
                            })?;
                            let pps_len = u16::try_from(pps.len()).map_err(|_| {
                                Error::InvalidFrame("HEVC PPS 长度超过 u16::MAX".to_string())
                            })?;
                            Some((vps, sps, pps, vps_len, sps_len, pps_len))
                        } else {
                            None
                        }
                    }
                    _ if self.session.is_none() => return Ok(PrepareOutcome::NeedKeyframe),
                    _ => None,
                }
            } else {
                None
            };

            if pending_rebuild.is_none() && self.session.is_none() {
                return Ok(PrepareOutcome::NeedKeyframe);
            }

            // 先完成 Annex-B → HVCC 转换校验，确保 replace_session 为最终不可逆步骤。
            if crate::h265::nals_ranges_to_hvcc_into(
                data,
                &self.nals_scratch,
                &mut self.avcc_scratch,
            )
            .is_none()
            {
                return Err(Error::InvalidFrame(
                    "HEVC 帧不含任何 slice NAL（无法解码出画面）".to_string(),
                ));
            }

            if let Some((vps, sps, pps, vps_len, sps_len, pps_len)) = pending_rebuild {
                let new_session = ffi::DecompressionSession::new_hevc(
                    vps,
                    sps,
                    pps,
                    self.cfg.max_resolution.width,
                    self.cfg.max_resolution.height,
                )
                .map_err(|e| match e {
                    Ok(dec_err) => Error::Decoder(dec_err),
                    Err(frame_err) => Error::InvalidFrame(frame_err),
                })?;
                let mut key = Vec::with_capacity(vps.len() + sps.len() + pps.len() + 6);
                key.extend_from_slice(&vps_len.to_be_bytes());
                key.extend_from_slice(vps);
                key.extend_from_slice(&sps_len.to_be_bytes());
                key.extend_from_slice(sps);
                key.extend_from_slice(&pps_len.to_be_bytes());
                key.extend_from_slice(pps);
                self.replace_session(new_session, key, pipelined);
            }

            Ok(PrepareOutcome::Ready { content_is_idr })
        }

        fn prepare_av1(
            &mut self,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
            pipelined: bool,
        ) -> Result<PrepareOutcome> {
            crate::av1_obu::parse_obus_into(data, &mut self.obu_scratch).map_err(|e| {
                Error::InvalidFrame(format!(
                    "rc-decode-vt: AV1 OBU 码流非法（配置声明 bitstream_format={}，{} 字节输入）: {e}",
                    self.cfg.bitstream_format.as_str(),
                    data.len()
                ))
            })?;

            // 若本帧携带 OBU_SEQUENCE_HEADER，先行解析校验（以便准确读取 reduced_still_picture_header）。
            let parsed_seq = if let Some(seq_idx) = self
                .obu_scratch
                .iter()
                .position(|o| o.obu_type == crate::av1_obu::OBU_SEQUENCE_HEADER)
            {
                let payload = &data[self.obu_scratch[seq_idx].payload_range.clone()];
                let seq = crate::av1_obu::parse_sequence_header(payload)
                    .map_err(|e| Error::InvalidFrame(e.to_string()))?;
                Some((seq, seq_idx))
            } else {
                None
            };

            let reduced_still = parsed_seq
                .as_ref()
                .map(|(s, _)| s.reduced_still_picture_header)
                .unwrap_or(self.av1_reduced_still_header);
            let content_is_idr =
                crate::av1_obu::classify_frame_is_keyframe(data, &self.obu_scratch, reduced_still)
                    .map_err(|e| Error::InvalidFrame(e.to_string()))?;

            match (kind, content_is_idr) {
                (FrameKind::Idr, false) => {
                    return Err(Error::InvalidFrame(
                        "声明为 IDR 但 AV1 码流不含 KEY_FRAME OBU".to_string(),
                    ))
                }
                (FrameKind::P, true) => {
                    return Err(Error::InvalidFrame(
                        "声明为 P 但 AV1 码流含 KEY_FRAME OBU".to_string(),
                    ))
                }
                _ => {}
            }

            let est_done_us = self.clock_us.saturating_add(ESTIMATED_DECODE_COST_US);
            if est_done_us > deadline_ts.0 {
                return Err(Error::Timeout(Duration::from_micros(u64::from(
                    est_done_us - deadline_ts.0,
                ))));
            }

            let chain_available = self.gate.is_ok() || (pipelined && self.has_idr_in_flight());
            if !chain_available && !content_is_idr {
                return Ok(PrepareOutcome::NeedKeyframe);
            }

            let pending_rebuild = if content_is_idr {
                match parsed_seq {
                    Some((seq, seq_idx)) => {
                        let av1c =
                            crate::av1_obu::build_av1c(&seq, data, &self.obu_scratch[seq_idx])
                                .map_err(|e| Error::InvalidFrame(e.to_string()))?;
                        let needs_rebuild = match self.session_params.as_deref() {
                            Some(cur) => cur != av1c.as_slice() || self.session.is_none(),
                            None => true,
                        };
                        if needs_rebuild {
                            if seq.max_frame_width > self.cfg.max_resolution.width
                                || seq.max_frame_height > self.cfg.max_resolution.height
                            {
                                return Err(Error::InvalidFrame(format!(
                                    "AV1 Sequence Header 声明 {}x{} 超过解码上限 {}x{}",
                                    seq.max_frame_width,
                                    seq.max_frame_height,
                                    self.cfg.max_resolution.width,
                                    self.cfg.max_resolution.height
                                )));
                            }
                            Some((av1c, seq))
                        } else {
                            None
                        }
                    }
                    None if self.session.is_none() => return Ok(PrepareOutcome::NeedKeyframe),
                    None => None,
                }
            } else {
                None
            };

            if pending_rebuild.is_none() && self.session.is_none() {
                return Ok(PrepareOutcome::NeedKeyframe);
            }

            // 先按 VideoToolbox AV1 sample 契约剥离 TD/Padding 并校验，确保 replace_session 为最终步骤。
            crate::av1_obu::build_vt_sample_into(data, &self.obu_scratch, &mut self.avcc_scratch)
                .map_err(|e| Error::InvalidFrame(e.to_string()))?;

            if let Some((av1c, seq)) = pending_rebuild {
                let new_session = ffi::DecompressionSession::new_av1(
                    &av1c,
                    seq.max_frame_width,
                    seq.max_frame_height,
                    self.cfg.max_resolution.width,
                    self.cfg.max_resolution.height,
                )
                .map_err(|e| match e {
                    Ok(dec_err) => Error::Decoder(dec_err),
                    Err(frame_err) => Error::InvalidFrame(frame_err),
                })?;
                self.av1_reduced_still_header = seq.reduced_still_picture_header;
                self.replace_session(new_session, av1c, pipelined);
            }

            Ok(PrepareOutcome::Ready { content_is_idr })
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use platform::VtDecoder;

/// 工厂：按配置创建 VideoToolbox 解码器（[`VtDecoder::new`] 的别名入口）。
///
/// ```no_run
/// # #[cfg(any(target_os = "macos", target_os = "ios"))]
/// # fn main() -> rc_types::Result<()> {
/// use rc_decode::Decoder as _;
/// let mut dec = rc_decode_vt::new_decoder(rc_decode_vt::VtDecoderConfig::default())?;
/// assert!(dec.needs_keyframe(), "首个 IDR 之前必然缺关键帧");
/// # Ok(())
/// # }
/// # #[cfg(not(any(target_os = "macos", target_os = "ios")))]
/// # fn main() {}
/// ```
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn new_decoder(config: VtDecoderConfig) -> rc_types::Result<VtDecoder> {
    VtDecoder::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 坑 #7：bitstream_format 与编码端声明不符时，构造期报错且信息指明
    /// 两种封装（AVCC 输入不能喂给本解码器）。
    #[test]
    fn config_rejects_mismatched_bitstream_format() {
        let err = VtDecoderConfig {
            bitstream_format: BitstreamFormat::Avcc,
            ..VtDecoderConfig::default()
        }
        .validate()
        .expect_err("AVCC 声明必须被拒");
        assert!(
            matches!(&err, rc_types::Error::InvalidConfig(msg)
                if msg.contains("annexb") && msg.contains("avcc")),
            "错误信息必须同时点出期望（annexb）与实际（avcc），实际 {err:?}"
        );
        // 合法配置（默认）通过；零上限被拒。
        assert!(VtDecoderConfig::default().validate().is_ok());
        let zero = VtDecoderConfig {
            max_resolution: Resolution::new(0, 1080),
            ..VtDecoderConfig::default()
        }
        .validate()
        .expect_err("零分辨率必须被拒");
        assert!(matches!(zero, rc_types::Error::InvalidConfig(_)));
    }

    /// D4 语义（纯状态机）：初始断链；IDR 重建；断链后粘住直到下一个 IDR。
    #[test]
    fn ref_chain_gate_need_keyframe_semantics() {
        let mut gate = RefChainGate::new();
        assert!(!gate.is_ok(), "首个 IDR 之前链必然断裂");
        gate.break_chain();
        assert!(!gate.is_ok(), "断链是幂等的");
        gate.mark_idr_decoded();
        assert!(gate.is_ok(), "解出 IDR 后链重建");
        gate.break_chain();
        assert!(!gate.is_ok(), "解码失败/跳帧重新断链");
        gate.mark_idr_decoded();
        assert!(gate.is_ok(), "恢复路径：下一个 IDR");
    }
}

/// 无 GPU 的 macOS 单元测试：构造不碰 FFI、链断早退不碰 FFI——整个测试
/// 不创建任何 VideoToolbox 会话（纯逻辑路径全覆盖）。
#[cfg(all(test, any(target_os = "macos", target_os = "ios")))]
mod gates_no_gpu {
    use super::*;
    use rc_decode::{DecodeOutput, Decoder};
    use rc_types::{Error, FrameKind, Timestamp};

    /// 拼一段合法 Annex-B 单 NAL 帧（头字节由调用方给）。
    fn frame_of(header: u8) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        v.push(header);
        v.extend_from_slice(&[0x42; 8]);
        v
    }

    /// D4（入口段）+ 畸形拒绝 + deadline 语义：首个 IDR 之前 P 帧返回
    /// NeedKeyframe 并置位标志；不带参数集的 IDR 无法引导 → 同样
    /// NeedKeyframe；畸形输入与 kind 谎报在进任何 FFI 之前被拒；预计完成
    /// 超过截止 → Timeout 且不消耗帧号。
    #[test]
    fn decoder_gates_before_first_idr_and_malformed_input() {
        let mut dec = VtDecoder::new(VtDecoderConfig::default()).expect("默认配置合法");
        assert!(dec.needs_keyframe(), "从未解出 IDR：初始即缺关键帧");

        // P 帧（合法 Annex-B、非 IDR slice）：NeedKeyframe，不 panic（D4）。
        match dec
            .decode(&frame_of(0x41), FrameKind::P, Timestamp(10_000_000))
            .expect("缺参考不是错误")
        {
            DecodeOutput::NeedKeyframe => {}
            other => panic!("缺参考的 P 帧应返回 NeedKeyframe，实际 {other:?}"),
        }
        assert!(dec.needs_keyframe());

        // 畸形输入（非起始码开头 / 空输入）：InvalidFrame，绝不进 FFI。
        for garbage in [vec![0x00, 0x65, 1, 2], Vec::new()] {
            let err = dec
                .decode(&garbage, FrameKind::P, Timestamp(10_000_000))
                .expect_err("畸形输入必须报错");
            assert!(
                matches!(err, Error::InvalidFrame(ref msg) if msg.contains("Annex-B")),
                "应报 InvalidFrame 并点出 Annex-B，实际 {err:?}"
            );
        }

        // kind 谎报：声明 IDR 但无 IDR slice / 声明 P 但含 IDR slice。
        let err = dec
            .decode(&frame_of(0x41), FrameKind::Idr, Timestamp(10_000_000))
            .expect_err("谎报必须被拒");
        assert!(matches!(err, Error::InvalidFrame(_)));
        let err = dec
            .decode(&frame_of(0x65), FrameKind::P, Timestamp(10_000_000))
            .expect_err("谎报必须被拒");
        assert!(matches!(err, Error::InvalidFrame(_)));

        // 不带参数集的 IDR（无会话可沿用）：无法引导 → NeedKeyframe。
        match dec
            .decode(&frame_of(0x65), FrameKind::Idr, Timestamp(10_000_000))
            .expect("无法引导不是错误（请求关键帧即可恢复）")
        {
            DecodeOutput::NeedKeyframe => {}
            other => panic!("无从建会话的 IDR 应返回 NeedKeyframe，实际 {other:?}"),
        }
        assert!(dec.needs_keyframe());

        // deadline 预检：截止时刻小于预估完成时刻 → Timeout，且不消耗帧号。
        let before = dec.submitted_frames();
        let err = dec
            .decode(&frame_of(0x41), FrameKind::P, Timestamp(1_000))
            .expect_err("预计完成超过截止必须超时");
        assert!(matches!(err, Error::Timeout(_)), "实际 {err:?}");
        assert_eq!(dec.submitted_frames(), before, "超时路径不消耗帧号");

        // flush：无缓冲，恒 Ok。
        dec.flush().expect("进一出一的实现没有缓冲");
    }

    #[test]
    fn hevc_decoder_gates_and_config_validation() {
        assert!(VtDecoderConfig {
            codec: Codec::Hevc,
            ..VtDecoderConfig::default()
        }
        .validate()
        .is_ok());
        assert!(VtDecoderConfig {
            codec: Codec::Av1,
            ..VtDecoderConfig::default()
        }
        .validate()
        .is_err());

        let mut dec = VtDecoder::new(VtDecoderConfig {
            codec: Codec::Hevc,
            ..VtDecoderConfig::default()
        })
        .expect("HEVC 配置合法");
        assert!(dec.needs_keyframe());

        // HEVC TRAIL_R (nal_type 1 -> byte0 = 0x02) + layer/tid byte1 = 0x01
        let hevc_p = vec![0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0x42, 0x43];
        // HEVC IDR_W_RADL (nal_type 19 -> byte0 = 0x26) + byte1 = 0x01
        let hevc_idr = vec![0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0x42, 0x43];

        assert!(matches!(
            dec.decode(&hevc_p, FrameKind::P, Timestamp(10_000_000)),
            Ok(DecodeOutput::NeedKeyframe)
        ));
        assert!(matches!(
            dec.decode(&hevc_idr, FrameKind::Idr, Timestamp(10_000_000)),
            Ok(DecodeOutput::NeedKeyframe)
        ));
        assert!(dec
            .decode(&hevc_p, FrameKind::Idr, Timestamp(10_000_000))
            .is_err());
        assert!(dec
            .decode(&hevc_idr, FrameKind::P, Timestamp(10_000_000))
            .is_err());
        assert!(dec
            .decode(
                &[0x00, 0x00, 0x00, 0x01, 0x26],
                FrameKind::Idr,
                Timestamp(10_000_000)
            )
            .is_err());

        // 2-deep 流水线解码门禁：支持流水线、未见 IDR 时 submit_decode 返回 NeedKeyframe。
        assert!(dec.supports_pipelined_decode());
        assert_eq!(dec.in_flight_count(), 0);
        assert!(matches!(
            dec.submit_decode(1, &hevc_p, FrameKind::P, Timestamp(10_000_000)),
            Ok(rc_decode::DecodeSubmitStatus::NeedKeyframe)
        ));
        assert!(matches!(
            dec.submit_decode(2, &hevc_idr, FrameKind::Idr, Timestamp(10_000_000)),
            Ok(rc_decode::DecodeSubmitStatus::NeedKeyframe)
        ));
        assert_eq!(dec.in_flight_count(), 0);
        assert!(dec
            .recv_decoded(std::time::Duration::ZERO)
            .expect("无在途帧返回 Ok(None)")
            .is_none());
    }

    #[test]
    fn advance_clock_span_does_not_double_count_overlapping_pipelined_frames() {
        let mut dec = VtDecoder::new(VtDecoderConfig::default()).expect("配置合法");
        let t0 = std::time::Instant::now();
        let t1 = t0 + std::time::Duration::from_micros(1_500);
        let t2_start = t0 + std::time::Duration::from_micros(200);
        let t2_done = t0 + std::time::Duration::from_micros(2_000);

        let ts1 = dec.advance_clock_span(t0, t1);
        let ts2 = dec.advance_clock_span(t2_start, t2_done);
        assert_eq!(ts1.0, 1_500, "首帧应计入完整 1500us");
        assert_eq!(
            ts2.0, 2_000,
            "重叠帧仅计入非重叠增量 500us（总计 2000us），不得双算为 3300us"
        );
    }

    #[test]
    fn av1_config_validation_probe_and_decoder_gates() {
        // 1) 动态硬件 AV1 探测可安全调用（M1/M2 返回 false，M3/M4/A17 Pro+ 返回 true，绝不崩溃）。
        let hw_av1 = supports_hardware_av1_decode();

        // 2) 配置校验：AV1 必须匹配 BitstreamFormat::Obu；H264/HEVC 拒绝 Obu。
        assert!(VtDecoderConfig {
            codec: Codec::Av1,
            bitstream_format: BitstreamFormat::Obu,
            ..VtDecoderConfig::default()
        }
        .validate()
        .is_ok());
        assert!(VtDecoderConfig {
            codec: Codec::Av1,
            bitstream_format: BitstreamFormat::AnnexB,
            ..VtDecoderConfig::default()
        }
        .validate()
        .is_err());
        assert!(VtDecoderConfig {
            codec: Codec::Av1,
            bitstream_format: BitstreamFormat::Avcc,
            ..VtDecoderConfig::default()
        }
        .validate()
        .is_err());
        assert!(VtDecoderConfig {
            codec: Codec::H264,
            bitstream_format: BitstreamFormat::Obu,
            ..VtDecoderConfig::default()
        }
        .validate()
        .is_err());

        // 3) 门禁与无 GPU/不支持硬件行为验证。
        let mut dec = VtDecoder::new(VtDecoderConfig {
            codec: Codec::Av1,
            bitstream_format: BitstreamFormat::Obu,
            ..VtDecoderConfig::default()
        })
        .expect("AV1 OBU 配置合法");
        assert!(dec.needs_keyframe());
        assert!(dec.supports_pipelined_decode());

        // 构造单 OBU 帧：OBU_FRAME (type=6, has_size=1 -> header=0x32)
        // INTER_FRAME: show_existing=0, frame_type=1 -> payload[0] = 0x20
        let av1_p = vec![0x32, 0x03, 0x20, 0x11, 0x22];
        // KEY_FRAME（无 Sequence Header）: show_existing=0, frame_type=0 -> payload[0] = 0x10
        let av1_idr_no_seq = vec![0x32, 0x03, 0x10, 0x33, 0x44];

        // 首个 IDR 之前 P 帧 -> NeedKeyframe
        assert!(matches!(
            dec.decode(&av1_p, FrameKind::P, Timestamp(10_000_000)),
            Ok(DecodeOutput::NeedKeyframe)
        ));
        // 无 Sequence Header 的关键帧（无会话可沿用）-> NeedKeyframe
        assert!(matches!(
            dec.decode(&av1_idr_no_seq, FrameKind::Idr, Timestamp(10_000_000)),
            Ok(DecodeOutput::NeedKeyframe)
        ));
        // kind 谎报拒绝
        assert!(matches!(
            dec.decode(&av1_p, FrameKind::Idr, Timestamp(10_000_000)),
            Err(Error::InvalidFrame(_))
        ));
        assert!(matches!(
            dec.decode(&av1_idr_no_seq, FrameKind::P, Timestamp(10_000_000)),
            Err(Error::InvalidFrame(_))
        ));
        // 畸形 OBU（forbidden bit = 1）拒绝
        assert!(matches!(
            dec.decode(&[0xb2, 0x01, 0x00], FrameKind::Idr, Timestamp(10_000_000)),
            Err(Error::InvalidFrame(_))
        ));
        // deadline 预检超时
        assert!(matches!(
            dec.decode(&av1_p, FrameKind::P, Timestamp(1_000)),
            Err(Error::Timeout(_))
        ));

        // 带合法 Sequence Header + KEY_FRAME：
        // - 超出 max_resolution 在进 FFI 前报 InvalidFrame；
        // - 在不支持 AV1 硬解的机器（如 M1/M2）上明确返回 Error::Decoder 且不崩溃。
        let seq_payload = crate::av1_obu::tests_helper_seq_payload(1920, 1080, 8, 0, false);
        let mut idr_with_seq = Vec::new();
        idr_with_seq.push((crate::av1_obu::OBU_SEQUENCE_HEADER << 3) | 0x02);
        crate::av1_obu::write_leb128(seq_payload.len() as u32, &mut idr_with_seq);
        idr_with_seq.extend_from_slice(&seq_payload);
        idr_with_seq.extend_from_slice(&av1_idr_no_seq);

        let mut small_dec = VtDecoder::new(VtDecoderConfig {
            codec: Codec::Av1,
            bitstream_format: BitstreamFormat::Obu,
            max_resolution: Resolution::new(1280, 720),
        })
        .expect("small AV1 config valid");
        assert!(matches!(
            small_dec.decode(&idr_with_seq, FrameKind::Idr, Timestamp(10_000_000)),
            Err(Error::InvalidFrame(ref msg)) if msg.contains("1920x1080")
        ));

        if !hw_av1 {
            let err = dec
                .decode(&idr_with_seq, FrameKind::Idr, Timestamp(10_000_000))
                .expect_err("不支持 AV1 硬解的设备必须明确返回错误");
            assert!(
                matches!(err, Error::Decoder(ref msg) if msg.contains("AV1")),
                "期望 Error::Decoder(AV1 unsupported)，实际 {err:?}"
            );
        }
    }
}
