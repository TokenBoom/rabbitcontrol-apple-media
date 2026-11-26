//! AV1 OBU（Open Bitstream Unit）解析、Sequence Header 提取与 ISO BMFF `av1C` 构造。
//!
//! 纯安全 Rust（继承 crate 级 `#![deny(unsafe_code)]`）、无平台依赖——在任何目标上
//! 均可离线单元测试。
//!
//! # 规范依据
//! - AV1 Bitstream & Decoding Process Specification v1.0.0（§4.10.3 `uvlc`、§4.10.5 `leb128`、
//!   §5.3 OBU 语法、§5.5 Sequence Header / Color Config、§5.9.2 Frame Header）；
//! - AV1 Codec ISO Media File Format Binding（§2.3.3 `AV1CodecConfigurationRecord` / `av1C`、
//!   §2.4 AV1 Sample Format）。
//!
//! # 防御式解析
//! 所有来自网络的 OBU 字节在进入 VideoToolbox FFI 之前均须通过本模块严格校验：
//! 任何 `obu_forbidden_bit != 0`、LEB128 溢出或截断、越界长度、非法 `seq_profile`
//! 或缺失帧 OBU 的输入均返回明确的 [`Av1ObuError`]，绝不 panic，绝不向 FFI 投喂畸形数据。

use std::fmt;
use std::ops::Range;

/// AV1 OBU 类型常量（AV1 Spec §6.2.2）。
pub(crate) const OBU_SEQUENCE_HEADER: u8 = 1;
/// 时间分隔符 OBU（AV1-ISOBMFF §2.4 要求在 sample 中剥离）。
pub(crate) const OBU_TEMPORAL_DELIMITER: u8 = 2;
/// 帧头 OBU。
pub(crate) const OBU_FRAME_HEADER: u8 = 3;
/// Tile 组 OBU。
pub(crate) const OBU_TILE_GROUP: u8 = 4;
/// 元数据 OBU。
pub(crate) const OBU_METADATA: u8 = 5;
/// 完整帧 OBU（Frame Header + Tile Group）。
pub(crate) const OBU_FRAME: u8 = 6;
/// 冗余帧头 OBU。
pub(crate) const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
/// Tile 列表 OBU（静态图像大图列表，非标准视频 sample 负载）。
pub(crate) const OBU_TILE_LIST: u8 = 8;
/// 填充 OBU（送入 VideoToolbox 前剥离）。
pub(crate) const OBU_PADDING: u8 = 15;

/// `AV1CodecConfigurationRecord` 固定首字节：`marker(1)=1 | version(7)=1`（`0x81`）。
pub(crate) const AV1C_MARKER_VERSION: u8 = 0x81;

/// AV1 Color Primaries / Transfer Characteristics / Matrix Coefficients 常量（AV1 Spec §6.4.2）。
const CP_BT_709: u8 = 1;
const CP_UNSPECIFIED: u8 = 2;
const TC_SRGB: u8 = 13;
const TC_UNSPECIFIED: u8 = 2;
const MC_IDENTITY: u8 = 0;
const MC_UNSPECIFIED: u8 = 2;
const CSP_UNKNOWN: u8 = 0;

/// 单条解析后的 OBU 索引范围（零拷贝引用原始输入切片）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObuRange {
    /// 4-bit `obu_type`。
    pub(crate) obu_type: u8,
    /// `obu_extension_flag`（存在 1 字节扩展头）。
    pub(crate) has_extension: bool,
    /// `obu_has_size_field`（存在 LEB128 长度字段）。
    pub(crate) has_size_field: bool,
    /// 原始输入中整条 OBU 的字节范围（含头、可选扩展头、可选 LEB128 与载荷）。
    pub(crate) raw_range: Range<usize>,
    /// 原始输入中 OBU 载荷的字节范围（不含头、扩展头与 LEB128）。
    pub(crate) payload_range: Range<usize>,
}

/// 从 `OBU_SEQUENCE_HEADER` 解析出的关键序列头参数（用于构造 `av1C` 与分辨率校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Av1SequenceHeader {
    /// AV1 Profile（`0` = Main, `1` = High, `2` = Professional）。
    pub(crate) seq_profile: u8,
    /// 是否静态图像流。
    pub(crate) still_picture: bool,
    /// 是否精简静态头模式（决定后续 Frame Header 是否省略前导标志位）。
    pub(crate) reduced_still_picture_header: bool,
    /// Operating point 0 的 level index（5 bit）。
    pub(crate) seq_level_idx_0: u8,
    /// Operating point 0 的 tier（1 bit，`0` = Main, `1` = High）。
    pub(crate) seq_tier_0: u8,
    /// 最大帧宽（像素，`max_frame_width_minus_1 + 1`）。
    pub(crate) max_frame_width: u32,
    /// 最大帧高（像素，`max_frame_height_minus_1 + 1`）。
    pub(crate) max_frame_height: u32,
    /// 位深（`8` / `10` / `12`）。
    pub(crate) bit_depth: u8,
    /// `high_bitdepth` 标志（10/12 bit 时为 `true`）。
    pub(crate) high_bitdepth: bool,
    /// `twelve_bit` 标志（仅 `seq_profile == 2 && high_bitdepth` 时可能为 `true`）。
    pub(crate) twelve_bit: bool,
    /// 是否单色流（`mono_chrome`）。
    pub(crate) monochrome: bool,
    /// 水平色度子采样标志（4:2:0/4:2:2 为 `1`，4:4:4 为 `0`）。
    pub(crate) chroma_subsampling_x: u8,
    /// 垂直色度子采样标志（4:2:0 为 `1`，4:2:2/4:4:4 为 `0`）。
    pub(crate) chroma_subsampling_y: u8,
    /// 色度采样位置（2 bit，仅 4:2:0 有显式值，其余为 `0`）。
    pub(crate) chroma_sample_position: u8,
    /// Operating point 0 是否携带初始显示延迟。
    pub(crate) initial_presentation_delay_present: bool,
    /// Operating point 0 的 `initial_display_delay_minus_1`（4 bit）。
    pub(crate) initial_presentation_delay_minus_1: u8,
}

/// AV1 OBU / Sequence Header 解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Av1ObuError {
    /// 输入为空。
    EmptyInput,
    /// `obu_forbidden_bit` 非零。
    ForbiddenBitSet { offset: usize },
    /// 声明了 `obu_extension_flag`，但缺少扩展头字节。
    TruncatedExtensionHeader { offset: usize },
    /// LEB128 长度字段提前截断。
    Leb128Truncated { offset: usize },
    /// LEB128 长度超过 AV1 规范上限（> 8 字节或值超过 `u32::MAX`）。
    Leb128Overflow { offset: usize },
    /// OBU 声明长度超出剩余输入缓冲。
    PayloadTruncated {
        offset: usize,
        declared: usize,
        available: usize,
    },
    /// 必须非空的 OBU 载荷为空。
    EmptyPayload { obu_type: u8, offset: usize },
    /// `OBU_SEQUENCE_HEADER` 语法非法或字段截断。
    InvalidSequenceHeader(&'static str),
    /// 输入 OBU 序列不含任何可解码帧 OBU（`OBU_FRAME` / `OBU_FRAME_HEADER`）。
    NoDecodableFrame,
    /// 单条 OBU 载荷长度超过 `u32::MAX`（无法编码为合法 AV1 LEB128）。
    PayloadTooLarge,
}

impl fmt::Display for Av1ObuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput => write!(f, "AV1 OBU 输入为空"),
            Self::ForbiddenBitSet { offset } => {
                write!(f, "AV1 OBU 偏移 {offset} 处 obu_forbidden_bit 非零")
            }
            Self::TruncatedExtensionHeader { offset } => {
                write!(f, "AV1 OBU 偏移 {offset} 处缺少 obu_extension_header 字节")
            }
            Self::Leb128Truncated { offset } => {
                write!(f, "AV1 OBU 偏移 {offset} 处 LEB128 长度截断")
            }
            Self::Leb128Overflow { offset } => {
                write!(
                    f,
                    "AV1 OBU 偏移 {offset} 处 LEB128 长度溢出（超过 8 字节或 u32::MAX）"
                )
            }
            Self::PayloadTruncated {
                offset,
                declared,
                available,
            } => write!(
                f,
                "AV1 OBU 偏移 {offset} 处载荷截断：声明 {declared} 字节，实际仅剩 {available} 字节"
            ),
            Self::EmptyPayload { obu_type, offset } => {
                write!(f, "AV1 OBU(type={obu_type}) 偏移 {offset} 处载荷为空")
            }
            Self::InvalidSequenceHeader(reason) => {
                write!(f, "AV1 OBU_SEQUENCE_HEADER 非法: {reason}")
            }
            Self::NoDecodableFrame => {
                write!(
                    f,
                    "AV1 OBU 序列不含可解码帧（缺少 OBU_FRAME / OBU_FRAME_HEADER）"
                )
            }
            Self::PayloadTooLarge => write!(f, "AV1 OBU 载荷超过 u32::MAX 上限"),
        }
    }
}

/// 按 AV1 Spec §4.10.5 解析无符号 `leb128()`，返回 `(value, bytes_consumed)`。
///
/// 规范要求：
/// - 编码字节数最多 8 字节（第 8 字节最高位仍为 1 时判溢出）；
/// - 解码后的数值必须 `< 2^32`（即 fits in `u32`）。
pub(crate) fn read_leb128(data: &[u8]) -> Result<(u32, usize), Av1ObuError> {
    let mut value: u64 = 0;
    for i in 0..8usize {
        let Some(&byte) = data.get(i) else {
            return Err(Av1ObuError::Leb128Truncated { offset: 0 });
        };
        value |= u64::from(byte & 0x7f) << (i * 7);
        if (byte & 0x80) == 0 {
            if value > u64::from(u32::MAX) {
                return Err(Av1ObuError::Leb128Overflow { offset: 0 });
            }
            return Ok((value as u32, i + 1));
        }
    }
    Err(Av1ObuError::Leb128Overflow { offset: 0 })
}

/// 将 `u32` 编码为最小长度无符号 LEB128 追加写入 `out`。
pub(crate) fn write_leb128(mut value: u32, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
            out.push(byte);
        } else {
            out.push(byte);
            break;
        }
    }
}

/// 零堆分配解析 AV1 OBU 序列，结果写入可复用缓冲 `out`。
pub(crate) fn parse_obus_into(input: &[u8], out: &mut Vec<ObuRange>) -> Result<(), Av1ObuError> {
    out.clear();
    if input.is_empty() {
        return Err(Av1ObuError::EmptyInput);
    }
    let mut pos = 0usize;
    while pos < input.len() {
        let header = input[pos];
        if (header & 0x80) != 0 {
            return Err(Av1ObuError::ForbiddenBitSet { offset: pos });
        }
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = (header & 0x04) != 0;
        let has_size_field = (header & 0x02) != 0;

        let mut cursor = pos + 1;
        if has_extension {
            if cursor >= input.len() {
                return Err(Av1ObuError::TruncatedExtensionHeader { offset: pos });
            }
            cursor += 1;
        }

        let (payload_start, payload_end) = if has_size_field {
            let (size_u32, leb_len) = read_leb128(&input[cursor..]).map_err(|e| match e {
                Av1ObuError::Leb128Truncated { .. } => {
                    Av1ObuError::Leb128Truncated { offset: cursor }
                }
                Av1ObuError::Leb128Overflow { .. } => {
                    Av1ObuError::Leb128Overflow { offset: cursor }
                }
                other => other,
            })?;
            let start = cursor + leb_len;
            let declared = size_u32 as usize;
            let available = input.len().saturating_sub(start);
            let Some(end) = start.checked_add(declared) else {
                return Err(Av1ObuError::PayloadTruncated {
                    offset: pos,
                    declared,
                    available,
                });
            };
            if end > input.len() {
                return Err(Av1ObuError::PayloadTruncated {
                    offset: pos,
                    declared,
                    available,
                });
            }
            (start, end)
        } else {
            (cursor, input.len())
        };

        out.push(ObuRange {
            obu_type,
            has_extension,
            has_size_field,
            raw_range: pos..payload_end,
            payload_range: payload_start..payload_end,
        });
        pos = payload_end;
    }

    if out.is_empty() {
        return Err(Av1ObuError::EmptyInput);
    }
    Ok(())
}

/// 纯 Rust 安全位流读取器（大端 MSB-first，零 `unsafe`，防移位溢出）。
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte_idx = self.bit_pos >> 3;
        let bit_idx = 7 - (self.bit_pos & 7);
        let byte = *self.data.get(byte_idx)?;
        self.bit_pos += 1;
        Some((byte >> bit_idx) & 1)
    }

    fn read_bool(&mut self) -> Option<bool> {
        self.read_bit().map(|b| b != 0)
    }

    /// 读取 `n` 位（`0..=32`），返回大端无符号整数；剩余位不足返回 `None`。
    fn read_bits(&mut self, n: u8) -> Option<u32> {
        debug_assert!(n <= 32);
        let mut acc = 0u32;
        for _ in 0..n {
            acc = (acc << 1) | u32::from(self.read_bit()?);
        }
        Some(acc)
    }

    /// AV1 Spec §4.10.3 `uvlc()`。
    fn read_uvlc(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        loop {
            let done = self.read_bit()?;
            if done != 0 {
                break;
            }
            leading_zeros = leading_zeros.saturating_add(1);
        }
        if leading_zeros >= 32 {
            return Some(u32::MAX);
        }
        let value = self.read_bits(leading_zeros as u8)?;
        let base = (1u32 << leading_zeros) - 1;
        value.checked_add(base)
    }
}

/// 严格按 AV1 Spec §5.5.1 + §5.5.2 解析 `OBU_SEQUENCE_HEADER` 载荷。
pub(crate) fn parse_sequence_header(payload: &[u8]) -> Result<Av1SequenceHeader, Av1ObuError> {
    if payload.is_empty() {
        return Err(Av1ObuError::InvalidSequenceHeader("载荷为空"));
    }
    let mut br = BitReader::new(payload);
    let eof = || Av1ObuError::InvalidSequenceHeader("位流意外截断");

    let seq_profile = br.read_bits(3).ok_or_else(eof)? as u8;
    if seq_profile > 2 {
        return Err(Av1ObuError::InvalidSequenceHeader(
            "seq_profile 超过 AV1 合法范围 (0..=2)",
        ));
    }
    let still_picture = br.read_bool().ok_or_else(eof)?;
    let reduced_still_picture_header = br.read_bool().ok_or_else(eof)?;

    let mut seq_level_idx_0 = 0u8;
    let mut seq_tier_0 = 0u8;
    let mut initial_presentation_delay_present = false;
    let mut initial_presentation_delay_minus_1 = 0u8;

    if reduced_still_picture_header {
        seq_level_idx_0 = br.read_bits(5).ok_or_else(eof)? as u8;
    } else {
        let timing_info_present_flag = br.read_bool().ok_or_else(eof)?;
        let mut decoder_model_info_present_flag = false;
        let mut buffer_delay_length_minus_1 = 0u8;
        if timing_info_present_flag {
            // timing_info() (§5.5.3)
            let _num_units_in_display_tick = br.read_bits(32).ok_or_else(eof)?;
            let _time_scale = br.read_bits(32).ok_or_else(eof)?;
            let equal_picture_interval = br.read_bool().ok_or_else(eof)?;
            if equal_picture_interval {
                let _num_ticks_per_picture_minus_1 = br.read_uvlc().ok_or_else(eof)?;
            }
            decoder_model_info_present_flag = br.read_bool().ok_or_else(eof)?;
            if decoder_model_info_present_flag {
                // decoder_model_info() (§5.5.4)
                buffer_delay_length_minus_1 = br.read_bits(5).ok_or_else(eof)? as u8;
                let _num_units_in_decoding_tick = br.read_bits(32).ok_or_else(eof)?;
                let _buffer_removal_time_length_minus_1 = br.read_bits(5).ok_or_else(eof)?;
                let _frame_presentation_time_length_minus_1 = br.read_bits(5).ok_or_else(eof)?;
            }
        }
        let initial_display_delay_present_flag = br.read_bool().ok_or_else(eof)?;
        let operating_points_cnt_minus_1 = br.read_bits(5).ok_or_else(eof)? as u8;
        for i in 0..=operating_points_cnt_minus_1 {
            let _operating_point_idc = br.read_bits(12).ok_or_else(eof)?;
            let level_idx = br.read_bits(5).ok_or_else(eof)? as u8;
            let tier = if level_idx > 7 {
                br.read_bits(1).ok_or_else(eof)? as u8
            } else {
                0
            };
            if decoder_model_info_present_flag {
                let decoder_model_present_for_this_op = br.read_bool().ok_or_else(eof)?;
                if decoder_model_present_for_this_op {
                    // operating_parameters_info(i) (§5.5.5)
                    let n = buffer_delay_length_minus_1 + 1;
                    let _decoder_buffer_delay = br.read_bits(n).ok_or_else(eof)?;
                    let _encoder_buffer_delay = br.read_bits(n).ok_or_else(eof)?;
                    let _low_delay_mode_flag = br.read_bool().ok_or_else(eof)?;
                }
            }
            let mut op_delay_present = false;
            let mut op_delay_minus_1 = 0u8;
            if initial_display_delay_present_flag {
                op_delay_present = br.read_bool().ok_or_else(eof)?;
                if op_delay_present {
                    op_delay_minus_1 = br.read_bits(4).ok_or_else(eof)? as u8;
                }
            }
            if i == 0 {
                seq_level_idx_0 = level_idx;
                seq_tier_0 = tier;
                initial_presentation_delay_present = op_delay_present;
                initial_presentation_delay_minus_1 = op_delay_minus_1;
            }
        }
    }

    let frame_width_bits_minus_1 = br.read_bits(4).ok_or_else(eof)? as u8;
    let frame_height_bits_minus_1 = br.read_bits(4).ok_or_else(eof)? as u8;
    let max_frame_width_minus_1 = br.read_bits(frame_width_bits_minus_1 + 1).ok_or_else(eof)?;
    let max_frame_height_minus_1 = br
        .read_bits(frame_height_bits_minus_1 + 1)
        .ok_or_else(eof)?;
    let max_frame_width = max_frame_width_minus_1
        .checked_add(1)
        .ok_or(Av1ObuError::InvalidSequenceHeader("max_frame_width 溢出"))?;
    let max_frame_height = max_frame_height_minus_1
        .checked_add(1)
        .ok_or(Av1ObuError::InvalidSequenceHeader("max_frame_height 溢出"))?;

    let frame_id_numbers_present_flag = if reduced_still_picture_header {
        false
    } else {
        br.read_bool().ok_or_else(eof)?
    };
    if frame_id_numbers_present_flag {
        let _delta_frame_id_length_minus_2 = br.read_bits(4).ok_or_else(eof)?;
        let _additional_frame_id_length_minus_1 = br.read_bits(3).ok_or_else(eof)?;
    }

    let _use_128x128_superblock = br.read_bool().ok_or_else(eof)?;
    let _enable_filter_intra = br.read_bool().ok_or_else(eof)?;
    let _enable_intra_edge_filter = br.read_bool().ok_or_else(eof)?;

    if !reduced_still_picture_header {
        let _enable_interintra_compound = br.read_bool().ok_or_else(eof)?;
        let _enable_masked_compound = br.read_bool().ok_or_else(eof)?;
        let _enable_warped_motion = br.read_bool().ok_or_else(eof)?;
        let _enable_dual_filter = br.read_bool().ok_or_else(eof)?;
        let enable_order_hint = br.read_bool().ok_or_else(eof)?;
        if enable_order_hint {
            let _enable_jnt_comp = br.read_bool().ok_or_else(eof)?;
            let _enable_ref_frame_mvs = br.read_bool().ok_or_else(eof)?;
        }
        let seq_choose_screen_content_tools = br.read_bool().ok_or_else(eof)?;
        let seq_force_screen_content_tools = if seq_choose_screen_content_tools {
            2u8
        } else {
            br.read_bits(1).ok_or_else(eof)? as u8
        };
        if seq_force_screen_content_tools > 0 {
            let seq_choose_integer_mv = br.read_bool().ok_or_else(eof)?;
            if !seq_choose_integer_mv {
                let _seq_force_integer_mv = br.read_bits(1).ok_or_else(eof)?;
            }
        }
        if enable_order_hint {
            let _order_hint_bits_minus_1 = br.read_bits(3).ok_or_else(eof)?;
        }
    }

    let _enable_superres = br.read_bool().ok_or_else(eof)?;
    let _enable_cdef = br.read_bool().ok_or_else(eof)?;
    let _enable_restoration = br.read_bool().ok_or_else(eof)?;

    // color_config() (§5.5.2)
    let high_bitdepth = br.read_bool().ok_or_else(eof)?;
    let (twelve_bit, bit_depth) = if seq_profile == 2 && high_bitdepth {
        let tb = br.read_bool().ok_or_else(eof)?;
        (tb, if tb { 12u8 } else { 10u8 })
    } else {
        (false, if high_bitdepth { 10u8 } else { 8u8 })
    };
    let monochrome = if seq_profile == 1 {
        false
    } else {
        br.read_bool().ok_or_else(eof)?
    };
    let color_description_present_flag = br.read_bool().ok_or_else(eof)?;
    let (color_primaries, transfer_characteristics, matrix_coefficients) =
        if color_description_present_flag {
            (
                br.read_bits(8).ok_or_else(eof)? as u8,
                br.read_bits(8).ok_or_else(eof)? as u8,
                br.read_bits(8).ok_or_else(eof)? as u8,
            )
        } else {
            (CP_UNSPECIFIED, TC_UNSPECIFIED, MC_UNSPECIFIED)
        };

    let (chroma_subsampling_x, chroma_subsampling_y, chroma_sample_position) = if monochrome {
        let _color_range = br.read_bool().ok_or_else(eof)?;
        (1u8, 1u8, CSP_UNKNOWN)
    } else if color_primaries == CP_BT_709
        && transfer_characteristics == TC_SRGB
        && matrix_coefficients == MC_IDENTITY
    {
        let _separate_uv_delta_q = br.read_bool().ok_or_else(eof)?;
        (0u8, 0u8, CSP_UNKNOWN)
    } else {
        let _color_range = br.read_bool().ok_or_else(eof)?;
        let (sub_x, sub_y) = match seq_profile {
            0 => (1u8, 1u8),
            1 => (0u8, 0u8),
            _ => {
                if bit_depth == 12 {
                    let sx = br.read_bits(1).ok_or_else(eof)? as u8;
                    let sy = if sx != 0 {
                        br.read_bits(1).ok_or_else(eof)? as u8
                    } else {
                        0
                    };
                    (sx, sy)
                } else {
                    (1u8, 0u8)
                }
            }
        };
        let csp = if sub_x != 0 && sub_y != 0 {
            br.read_bits(2).ok_or_else(eof)? as u8
        } else {
            CSP_UNKNOWN
        };
        let _separate_uv_delta_q = br.read_bool().ok_or_else(eof)?;
        (sub_x, sub_y, csp)
    };

    let _film_grain_params_present = br.read_bool().ok_or_else(eof)?;

    Ok(Av1SequenceHeader {
        seq_profile,
        still_picture,
        reduced_still_picture_header,
        seq_level_idx_0,
        seq_tier_0,
        max_frame_width,
        max_frame_height,
        bit_depth,
        high_bitdepth,
        twelve_bit,
        monochrome,
        chroma_subsampling_x,
        chroma_subsampling_y,
        chroma_sample_position,
        initial_presentation_delay_present,
        initial_presentation_delay_minus_1,
    })
}

/// 将单条 OBU 规范化写入 `out`，确保 `obu_has_size_field = 1` 并带合规 LEB128 长度。
fn append_sized_obu(input: &[u8], obu: &ObuRange, out: &mut Vec<u8>) -> Result<(), Av1ObuError> {
    if obu.has_size_field {
        out.extend_from_slice(&input[obu.raw_range.clone()]);
        return Ok(());
    }
    let payload = &input[obu.payload_range.clone()];
    let payload_len = u32::try_from(payload.len()).map_err(|_| Av1ObuError::PayloadTooLarge)?;
    let header_byte = input[obu.raw_range.start] | 0x02;
    out.push(header_byte);
    if obu.has_extension {
        out.push(input[obu.raw_range.start + 1]);
    }
    write_leb128(payload_len, out);
    out.extend_from_slice(payload);
    Ok(())
}

/// 构造合规的 ISO BMFF `av1C` (`AV1CodecConfigurationRecord`) 配置字节：
/// 4 字节头 + 带 `obu_has_size_field = 1` 的 `OBU_SEQUENCE_HEADER` 原始字节。
pub(crate) fn build_av1c(
    seq: &Av1SequenceHeader,
    input: &[u8],
    seq_obu: &ObuRange,
) -> Result<Vec<u8>, Av1ObuError> {
    if seq_obu.obu_type != OBU_SEQUENCE_HEADER {
        return Err(Av1ObuError::InvalidSequenceHeader(
            "输入 OBU 类型不是 OBU_SEQUENCE_HEADER",
        ));
    }
    let byte0 = AV1C_MARKER_VERSION;
    let byte1 = ((seq.seq_profile & 0x07) << 5) | (seq.seq_level_idx_0 & 0x1f);
    let byte2 = ((seq.seq_tier_0 & 0x01) << 7)
        | (u8::from(seq.high_bitdepth) << 6)
        | (u8::from(seq.twelve_bit) << 5)
        | (u8::from(seq.monochrome) << 4)
        | ((seq.chroma_subsampling_x & 0x01) << 3)
        | ((seq.chroma_subsampling_y & 0x01) << 2)
        | (seq.chroma_sample_position & 0x03);
    let byte3 = if seq.initial_presentation_delay_present {
        0x10 | (seq.initial_presentation_delay_minus_1 & 0x0f)
    } else {
        0x00
    };

    let mut av1c = Vec::with_capacity(4 + seq_obu.raw_range.len() + 5);
    av1c.extend_from_slice(&[byte0, byte1, byte2, byte3]);
    append_sized_obu(input, seq_obu, &mut av1c)?;
    Ok(av1c)
}

/// 判定已解析的 AV1 OBU 序列是否包含可解码帧，并识别其是否为 `KEY_FRAME`（IDR 等价）。
///
/// 按 AV1 Spec §5.9.2：
/// - `reduced_still_picture_header == true` 时隐含 `frame_type = KEY_FRAME (0)`；
/// - 否则读取首字节 bit 7 (`show_existing_frame`)：为 `1` 时非关键帧；
///   为 `0` 时读 bits 6..5 (`frame_type`)，`0` 即 `KEY_FRAME`。
pub(crate) fn classify_frame_is_keyframe(
    input: &[u8],
    obus: &[ObuRange],
    reduced_still_picture_header: bool,
) -> Result<bool, Av1ObuError> {
    let frame_obu = obus
        .iter()
        .find(|o| o.obu_type == OBU_FRAME || o.obu_type == OBU_FRAME_HEADER)
        .ok_or(Av1ObuError::NoDecodableFrame)?;
    let payload = &input[frame_obu.payload_range.clone()];
    let Some(&first) = payload.first() else {
        return Err(Av1ObuError::EmptyPayload {
            obu_type: frame_obu.obu_type,
            offset: frame_obu.raw_range.start,
        });
    };
    if reduced_still_picture_header {
        return Ok(true);
    }
    let show_existing_frame = (first >> 7) & 1;
    if show_existing_frame != 0 {
        return Ok(false);
    }
    let frame_type = (first >> 5) & 0x03;
    Ok(frame_type == 0)
}

/// 按 AV1-ISOBMFF §2.4 与 VideoToolbox AV1 Sample 契约组装送入 `CMSampleBuffer` 的 OBU 载荷：
/// - 剥离 `OBU_TEMPORAL_DELIMITER (2)`、`OBU_PADDING (15)`、`OBU_TILE_LIST (8)` 与未知保留类型；
/// - 保留 `OBU_SEQUENCE_HEADER` / `OBU_METADATA` / `OBU_FRAME_HEADER` / `OBU_TILE_GROUP` /
///   `OBU_FRAME` / `OBU_REDUNDANT_FRAME_HEADER`；
/// - 确保所有保留 OBU 具备 `obu_has_size_field = 1` + 合规 LEB128 长度。
pub(crate) fn build_vt_sample_into(
    input: &[u8],
    obus: &[ObuRange],
    out: &mut Vec<u8>,
) -> Result<(), Av1ObuError> {
    out.clear();
    let mut has_frame = false;
    let mut est_len = 0usize;
    for obu in obus {
        match obu.obu_type {
            OBU_FRAME | OBU_FRAME_HEADER => {
                has_frame = true;
                est_len = est_len.saturating_add(obu.raw_range.len() + 5);
            }
            OBU_SEQUENCE_HEADER | OBU_TILE_GROUP | OBU_METADATA | OBU_REDUNDANT_FRAME_HEADER => {
                est_len = est_len.saturating_add(obu.raw_range.len() + 5);
            }
            OBU_TEMPORAL_DELIMITER | OBU_PADDING | OBU_TILE_LIST => continue,
            _ => continue,
        }
    }
    if !has_frame {
        return Err(Av1ObuError::NoDecodableFrame);
    }
    out.reserve(est_len);
    for obu in obus {
        match obu.obu_type {
            OBU_SEQUENCE_HEADER
            | OBU_FRAME_HEADER
            | OBU_TILE_GROUP
            | OBU_METADATA
            | OBU_FRAME
            | OBU_REDUNDANT_FRAME_HEADER => append_sized_obu(input, obu, out)?,
            OBU_TEMPORAL_DELIMITER | OBU_PADDING | OBU_TILE_LIST => continue,
            _ => continue,
        }
    }
    if out.is_empty() {
        return Err(Av1ObuError::NoDecodableFrame);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn tests_helper_seq_payload(
    width: u32,
    height: u32,
    level_idx_0: u8,
    tier_0: u8,
    high_bitdepth: bool,
) -> Vec<u8> {
    tests::make_main_420_seq_payload(width, height, level_idx_0, tier_0, high_bitdepth)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用大端位写入器（纯 Rust，用于精确构造各种 AV1 Sequence Header 用例）。
    struct BitWriter {
        buf: Vec<u8>,
        cur: u8,
        bits_in_cur: u8,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                buf: Vec::new(),
                cur: 0,
                bits_in_cur: 0,
            }
        }

        fn write_bit(&mut self, bit: u8) {
            self.cur = (self.cur << 1) | (bit & 1);
            self.bits_in_cur += 1;
            if self.bits_in_cur == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.bits_in_cur = 0;
            }
        }

        fn write_bits(&mut self, val: u32, n: u8) {
            for shift in (0..n).rev() {
                self.write_bit(((val >> shift) & 1) as u8);
            }
        }

        fn write_uvlc(&mut self, val: u32) {
            let plus1 = u64::from(val) + 1;
            let lz = 63 - plus1.leading_zeros();
            for _ in 0..lz {
                self.write_bit(0);
            }
            self.write_bit(1);
            let rem = plus1 - (1u64 << lz);
            self.write_bits(rem as u32, lz as u8);
        }

        fn finish_with_trailing_bits(mut self) -> Vec<u8> {
            // AV1 trailing_bits(): 1 bit '1' + 0-padding to byte boundary.
            self.write_bit(1);
            if self.bits_in_cur > 0 {
                self.cur <<= 8 - self.bits_in_cur;
                self.buf.push(self.cur);
            }
            self.buf
        }
    }

    fn wrap_obu(obu_type: u8, has_size: bool, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let hdr = ((obu_type & 0x0f) << 3) | if has_size { 0x02 } else { 0x00 };
        out.push(hdr);
        if has_size {
            write_leb128(payload.len() as u32, &mut out);
        }
        out.extend_from_slice(payload);
        out
    }

    /// 构造标准 8-bit 4:2:0 Main Profile (`seq_profile = 0`) Sequence Header 载荷。
    pub(crate) fn make_main_420_seq_payload(
        width: u32,
        height: u32,
        level_idx_0: u8,
        tier_0: u8,
        high_bitdepth: bool,
    ) -> Vec<u8> {
        let mut bw = BitWriter::new();
        bw.write_bits(0, 3); // seq_profile = 0 (Main)
        bw.write_bit(0); // still_picture = 0
        bw.write_bit(0); // reduced_still_picture_header = 0
        bw.write_bit(0); // timing_info_present_flag = 0
        bw.write_bit(0); // initial_display_delay_present_flag = 0
        bw.write_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bw.write_bits(0, 12); // operating_point_idc[0] = 0
        bw.write_bits(u32::from(level_idx_0), 5);
        if level_idx_0 > 7 {
            bw.write_bit(tier_0);
        }
        bw.write_bits(15, 4); // frame_width_bits_minus_1 = 15 (16 bits)
        bw.write_bits(15, 4); // frame_height_bits_minus_1 = 15 (16 bits)
        bw.write_bits(width - 1, 16);
        bw.write_bits(height - 1, 16);
        bw.write_bit(0); // frame_id_numbers_present_flag = 0
        bw.write_bit(0); // use_128x128_superblock = 0
        bw.write_bit(1); // enable_filter_intra = 1
        bw.write_bit(1); // enable_intra_edge_filter = 1
        bw.write_bit(0); // enable_interintra_compound = 0
        bw.write_bit(0); // enable_masked_compound = 0
        bw.write_bit(0); // enable_warped_motion = 0
        bw.write_bit(0); // enable_dual_filter = 0
        bw.write_bit(1); // enable_order_hint = 1
        bw.write_bit(0); // enable_jnt_comp = 0
        bw.write_bit(0); // enable_ref_frame_mvs = 0
        bw.write_bit(1); // seq_choose_screen_content_tools = 1
        bw.write_bit(1); // seq_choose_integer_mv = 1
        bw.write_bits(6, 3); // order_hint_bits_minus_1 = 6
        bw.write_bit(0); // enable_superres = 0
        bw.write_bit(1); // enable_cdef = 1
        bw.write_bit(0); // enable_restoration = 0
                         // color_config()
        bw.write_bit(u8::from(high_bitdepth)); // high_bitdepth
        bw.write_bit(0); // mono_chrome = 0
        bw.write_bit(1); // color_description_present_flag = 1
        bw.write_bits(1, 8); // BT.709 primaries
        bw.write_bits(1, 8); // BT.709 transfer
        bw.write_bits(1, 8); // BT.709 matrix
        bw.write_bit(0); // color_range = 0 (studio)
        bw.write_bits(2, 2); // chroma_sample_position = 2 (colocated)
        bw.write_bit(0); // separate_uv_delta_q = 0
                         // film_grain_params_present
        bw.write_bit(0);
        bw.finish_with_trailing_bits()
    }

    #[test]
    fn parses_main_420_sequence_header_and_builds_compliant_av1c() {
        let payload = make_main_420_seq_payload(1920, 1080, 8, 1, false);
        let seq_obu_bytes = wrap_obu(OBU_SEQUENCE_HEADER, true, &payload);

        let mut obus = Vec::new();
        parse_obus_into(&seq_obu_bytes, &mut obus).expect("valid sequence header OBU");
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].obu_type, OBU_SEQUENCE_HEADER);
        assert!(obus[0].has_size_field);

        let seq = parse_sequence_header(&seq_obu_bytes[obus[0].payload_range.clone()])
            .expect("valid sequence header payload");
        assert_eq!(seq.seq_profile, 0);
        assert_eq!(seq.seq_level_idx_0, 8);
        assert_eq!(seq.seq_tier_0, 1);
        assert_eq!(seq.max_frame_width, 1920);
        assert_eq!(seq.max_frame_height, 1080);
        assert_eq!(seq.bit_depth, 8);
        assert!(!seq.high_bitdepth);
        assert!(!seq.twelve_bit);
        assert!(!seq.monochrome);
        assert_eq!(seq.chroma_subsampling_x, 1);
        assert_eq!(seq.chroma_subsampling_y, 1);
        assert_eq!(seq.chroma_sample_position, 2);
        assert!(!seq.initial_presentation_delay_present);

        let av1c = build_av1c(&seq, &seq_obu_bytes, &obus[0]).expect("av1C build");
        assert_eq!(av1c.len(), 4 + seq_obu_bytes.len());
        assert_eq!(av1c[0], 0x81, "marker=1, version=1");
        assert_eq!(av1c[1], 0x08, "profile=0, level_idx_0=8");
        // byte2: tier=1 (0x80) | hbd=0 | 12b=0 | mono=0 | sub_x=1 (0x08) | sub_y=1 (0x04) | csp=2 (0x02) = 0x8e
        assert_eq!(av1c[2], 0x8e);
        assert_eq!(av1c[3], 0x00, "no initial_presentation_delay");
        assert_eq!(&av1c[4..], seq_obu_bytes.as_slice());
    }

    #[test]
    fn parses_10bit_timing_multi_op_and_initial_delay_av1c() {
        let mut bw = BitWriter::new();
        bw.write_bits(0, 3); // seq_profile = 0
        bw.write_bit(0); // still_picture
        bw.write_bit(0); // reduced_still_picture_header
        bw.write_bit(1); // timing_info_present_flag = 1
        bw.write_bits(1, 32); // num_units_in_display_tick
        bw.write_bits(60, 32); // time_scale
        bw.write_bit(1); // equal_picture_interval = 1
        bw.write_uvlc(0); // num_ticks_per_picture_minus_1 = 0
        bw.write_bit(1); // decoder_model_info_present_flag = 1
        bw.write_bits(9, 5); // buffer_delay_length_minus_1 = 9 (10 bits)
        bw.write_bits(1, 32); // num_units_in_decoding_tick
        bw.write_bits(9, 5); // buffer_removal_time_length_minus_1
        bw.write_bits(9, 5); // frame_presentation_time_length_minus_1
        bw.write_bit(1); // initial_display_delay_present_flag = 1
        bw.write_bits(1, 5); // operating_points_cnt_minus_1 = 1 (2 operating points)
                             // op 0:
        bw.write_bits(0x0101, 12); // operating_point_idc[0]
        bw.write_bits(13, 5); // seq_level_idx[0] = 13 (Level 5.1)
        bw.write_bit(0); // seq_tier[0] = 0
        bw.write_bit(1); // decoder_model_present_for_this_op[0] = 1
        bw.write_bits(123, 10); // decoder_buffer_delay[0]
        bw.write_bits(456, 10); // encoder_buffer_delay[0]
        bw.write_bit(1); // low_delay_mode_flag[0]
        bw.write_bit(1); // initial_display_delay_present_for_this_op[0] = 1
        bw.write_bits(3, 4); // initial_display_delay_minus_1[0] = 3
                             // op 1:
        bw.write_bits(0x0001, 12);
        bw.write_bits(5, 5); // level_idx <= 7 -> no tier bit
        bw.write_bit(0); // decoder_model_present_for_this_op[1] = 0
        bw.write_bit(0); // initial_display_delay_present_for_this_op[1] = 0
                         // dimensions 3840x2160:
        bw.write_bits(11, 4); // 12 bits width
        bw.write_bits(11, 4); // 12 bits height
        bw.write_bits(3839, 12);
        bw.write_bits(2159, 12);
        bw.write_bit(1); // frame_id_numbers_present_flag = 1
        bw.write_bits(12, 4); // delta_frame_id_length_minus_2
        bw.write_bits(5, 3); // additional_frame_id_length_minus_1
        bw.write_bit(1); // use_128x128_superblock
        bw.write_bit(0); // enable_filter_intra
        bw.write_bit(0); // enable_intra_edge_filter
        bw.write_bit(0); // enable_interintra_compound
        bw.write_bit(0); // enable_masked_compound
        bw.write_bit(0); // enable_warped_motion
        bw.write_bit(0); // enable_dual_filter
        bw.write_bit(0); // enable_order_hint = 0
        bw.write_bit(0); // seq_choose_screen_content_tools = 0
        bw.write_bit(0); // seq_force_screen_content_tools = 0 (skip integer_mv!)
        bw.write_bit(0); // enable_superres
        bw.write_bit(1); // enable_cdef
        bw.write_bit(0); // enable_restoration
                         // color_config: 10-bit 4:2:0
        bw.write_bit(1); // high_bitdepth = 1
        bw.write_bit(0); // mono_chrome = 0
        bw.write_bit(0); // color_description_present_flag = 0
        bw.write_bit(0); // color_range = 0
        bw.write_bits(1, 2); // chroma_sample_position = 1 (vertical)
        bw.write_bit(0); // separate_uv_delta_q
        bw.write_bit(1); // film_grain_params_present
        let payload = bw.finish_with_trailing_bits();

        // 测试无 size_field 的 OBU 被 build_av1c 自动规范化为 has_size_field = 1。
        let unsized_obu = wrap_obu(OBU_SEQUENCE_HEADER, false, &payload);
        let mut obus = Vec::new();
        parse_obus_into(&unsized_obu, &mut obus).expect("unsized trailing OBU");
        assert!(!obus[0].has_size_field);

        let seq = parse_sequence_header(&unsized_obu[obus[0].payload_range.clone()])
            .expect("valid 10-bit seq hdr");
        assert_eq!(seq.max_frame_width, 3840);
        assert_eq!(seq.max_frame_height, 2160);
        assert_eq!(seq.bit_depth, 10);
        assert!(seq.high_bitdepth);
        assert_eq!(seq.seq_level_idx_0, 13);
        assert!(seq.initial_presentation_delay_present);
        assert_eq!(seq.initial_presentation_delay_minus_1, 3);

        let av1c = build_av1c(&seq, &unsized_obu, &obus[0]).expect("av1C");
        assert_eq!(av1c[0], 0x81);
        assert_eq!(av1c[1], 13); // profile 0, level 13
                                 // tier=0, hbd=1 (0x40), 12b=0, mono=0, sub_x=1 (0x08), sub_y=1 (0x04), csp=1 (0x01) = 0x4d
        assert_eq!(av1c[2], 0x4d);
        // initial_presentation_delay_present=1 (0x10) | minus_1=3 = 0x13
        assert_eq!(av1c[3], 0x13);
        // configOBUs 必须带 obu_has_size_field = 1 且能被 parse_obus_into 还原。
        let mut reparsed = Vec::new();
        parse_obus_into(&av1c[4..], &mut reparsed).expect("configOBUs must be valid sized OBU");
        assert_eq!(reparsed.len(), 1);
        assert!(reparsed[0].has_size_field);
        assert_eq!(&av1c[4..][reparsed[0].payload_range.clone()], payload);
    }

    #[test]
    fn parses_profile2_12bit_422_and_reduced_still_picture_header() {
        let mut bw = BitWriter::new();
        bw.write_bits(2, 3); // seq_profile = 2 (Professional)
        bw.write_bit(1); // still_picture = 1
        bw.write_bit(1); // reduced_still_picture_header = 1
        bw.write_bits(4, 5); // seq_level_idx_0 = 4
        bw.write_bits(9, 4); // 10 bits width
        bw.write_bits(9, 4); // 10 bits height
        bw.write_bits(639, 10);
        bw.write_bits(479, 10);
        bw.write_bit(0); // use_128x128_superblock
        bw.write_bit(0); // enable_filter_intra
        bw.write_bit(0); // enable_intra_edge_filter
        bw.write_bit(0); // enable_superres
        bw.write_bit(0); // enable_cdef
        bw.write_bit(0); // enable_restoration
                         // color_config: 12-bit 4:2:2
        bw.write_bit(1); // high_bitdepth = 1
        bw.write_bit(1); // twelve_bit = 1
        bw.write_bit(0); // mono_chrome = 0
        bw.write_bit(0); // color_description_present_flag = 0
        bw.write_bit(1); // color_range = 1
        bw.write_bit(1); // subsampling_x = 1
        bw.write_bit(0); // subsampling_y = 0 (4:2:2)
        bw.write_bit(0); // separate_uv_delta_q
        bw.write_bit(0); // film_grain_params_present
        let payload = bw.finish_with_trailing_bits();

        let seq = parse_sequence_header(&payload).expect("valid profile 2 reduced still header");
        assert_eq!(seq.seq_profile, 2);
        assert!(seq.reduced_still_picture_header);
        assert_eq!(seq.seq_level_idx_0, 4);
        assert_eq!(seq.max_frame_width, 640);
        assert_eq!(seq.max_frame_height, 480);
        assert_eq!(seq.bit_depth, 12);
        assert!(seq.high_bitdepth);
        assert!(seq.twelve_bit);
        assert_eq!(seq.chroma_subsampling_x, 1);
        assert_eq!(seq.chroma_subsampling_y, 0);
        assert_eq!(seq.chroma_sample_position, 0);
    }

    #[test]
    fn classifies_keyframe_vs_inter_and_strips_delimiters_and_padding() {
        let seq_payload = make_main_420_seq_payload(1280, 720, 4, 0, false);
        let mut idr_tu = Vec::new();
        // OBU_TEMPORAL_DELIMITER (empty payload, sized)
        idr_tu.extend_from_slice(&wrap_obu(OBU_TEMPORAL_DELIMITER, true, &[]));
        // OBU_SEQUENCE_HEADER
        idr_tu.extend_from_slice(&wrap_obu(OBU_SEQUENCE_HEADER, true, &seq_payload));
        // OBU_FRAME: show_existing_frame=0 (bit7=0), frame_type=KEY_FRAME (bits6..5=00) -> 0x10
        idr_tu.extend_from_slice(&wrap_obu(OBU_FRAME, true, &[0x10, 0xaa, 0xbb, 0xcc]));
        // OBU_PADDING
        idr_tu.extend_from_slice(&wrap_obu(OBU_PADDING, true, &[0x00, 0x00]));

        let mut obus = Vec::new();
        parse_obus_into(&idr_tu, &mut obus).expect("valid temporal unit");
        assert_eq!(obus.len(), 4);
        assert!(classify_frame_is_keyframe(&idr_tu, &obus, false).expect("classify keyframe"));

        let mut sample = Vec::new();
        build_vt_sample_into(&idr_tu, &obus, &mut sample).expect("sample build");
        let mut sample_obus = Vec::new();
        parse_obus_into(&sample, &mut sample_obus).expect("valid VT sample");
        assert_eq!(sample_obus.len(), 2, "TD 与 PADDING 必须被剥离");
        assert_eq!(sample_obus[0].obu_type, OBU_SEQUENCE_HEADER);
        assert_eq!(sample_obus[1].obu_type, OBU_FRAME);

        // INTER_FRAME: show_existing_frame=0, frame_type=1 (01) -> byte0 = 0x20
        let inter = wrap_obu(OBU_FRAME, true, &[0x20, 0x11, 0x22]);
        parse_obus_into(&inter, &mut obus).expect("valid inter OBU");
        assert!(!classify_frame_is_keyframe(&inter, &obus, false).expect("classify inter"));

        // show_existing_frame=1 -> byte0 = 0x80 -> 非关键帧
        let show_existing = wrap_obu(OBU_FRAME_HEADER, true, &[0x80, 0x00]);
        parse_obus_into(&show_existing, &mut obus).expect("valid show_existing OBU");
        assert!(!classify_frame_is_keyframe(&show_existing, &obus, false).expect("classify show"));

        // reduced_still_picture_header = true -> 隐含 KEY_FRAME
        assert!(classify_frame_is_keyframe(&inter, &obus, true).expect("reduced still is key"));
    }

    #[test]
    fn rejects_corrupted_truncated_and_leb128_overflow_inputs() {
        let mut obus = Vec::new();
        // 空输入
        assert_eq!(
            parse_obus_into(&[], &mut obus),
            Err(Av1ObuError::EmptyInput)
        );
        // obu_forbidden_bit = 1
        assert_eq!(
            parse_obus_into(&[0x8a, 0x00], &mut obus),
            Err(Av1ObuError::ForbiddenBitSet { offset: 0 })
        );
        // 声明扩展头但截断
        assert_eq!(
            parse_obus_into(&[0x0e], &mut obus),
            Err(Av1ObuError::TruncatedExtensionHeader { offset: 0 })
        );
        // LEB128 截断
        assert_eq!(
            parse_obus_into(&[0x0a, 0x80], &mut obus),
            Err(Av1ObuError::Leb128Truncated { offset: 1 })
        );
        // LEB128 超过 8 字节（第 8 字节仍带 continuation bit 0x80）
        assert_eq!(
            parse_obus_into(
                &[0x0a, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
                &mut obus
            ),
            Err(Av1ObuError::Leb128Overflow { offset: 1 })
        );
        // LEB128 <= 8 字节但值 > u32::MAX（第 5 字节携带 0x10 -> 1 << 32）
        assert_eq!(
            parse_obus_into(&[0x0a, 0x80, 0x80, 0x80, 0x80, 0x10], &mut obus),
            Err(Av1ObuError::Leb128Overflow { offset: 1 })
        );
        // 载荷截断：声明 10 字节但只剩 2 字节
        assert_eq!(
            parse_obus_into(&[0x32, 0x0a, 0x01, 0x02], &mut obus),
            Err(Av1ObuError::PayloadTruncated {
                offset: 0,
                declared: 10,
                available: 2,
            })
        );
        // 非法 seq_profile (3..=7)
        assert!(matches!(
            parse_sequence_header(&[0x60, 0x00, 0x00]),
            Err(Av1ObuError::InvalidSequenceHeader(_))
        ));
        // 截断的 Sequence Header
        let full_seq = make_main_420_seq_payload(1920, 1080, 8, 0, false);
        assert!(matches!(
            parse_sequence_header(&full_seq[..full_seq.len() / 2]),
            Err(Av1ObuError::InvalidSequenceHeader(_))
        ));
        // 仅含 TD / Sequence Header、不含帧 OBU
        let td_only = wrap_obu(OBU_TEMPORAL_DELIMITER, true, &[]);
        parse_obus_into(&td_only, &mut obus).expect("TD itself parses");
        assert_eq!(
            classify_frame_is_keyframe(&td_only, &obus, false),
            Err(Av1ObuError::NoDecodableFrame)
        );
        let mut out = Vec::new();
        assert_eq!(
            build_vt_sample_into(&td_only, &obus, &mut out),
            Err(Av1ObuError::NoDecodableFrame)
        );
    }
}
