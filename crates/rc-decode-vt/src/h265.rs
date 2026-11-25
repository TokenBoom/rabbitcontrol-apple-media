//! H.265 (HEVC) Annex-B 解析、Annex-B → HVCC 转换与参数集提取。
//!
//! 纯函数、无 unsafe、无平台依赖 —— 在任何目标上都可以单元测试。

/// HEVC NAL 单元类型：VPS / SPS / PPS（ISO/IEC 23008-2 表 7-1）。
pub(crate) const NAL_VPS: u8 = 32;
/// HEVC SPS NAL 类型。
pub(crate) const NAL_SPS: u8 = 33;
/// HEVC PPS NAL 类型。
pub(crate) const NAL_PPS: u8 = 34;

/// HEVC IRAP 随机接入帧 NAL 类型判定（BLA 16..=18 / IDR 19..=20 / CRA 21）。
#[inline]
pub(crate) const fn is_irap_nal(nal_type: u8) -> bool {
    nal_type >= 16 && nal_type <= 21
}

/// `pos` 处是否是 Annex-B 起始码；是则返回其长度（4 或 3 字节形式）。
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

/// HEVC NAL 头首字节提取 6 位 `nal_unit_type`（bits 1..=6）。
#[inline]
const fn nal_unit_type(header_first_byte: u8) -> u8 {
    (header_first_byte >> 1) & 0x3f
}

/// 严格切分 HEVC Annex-B 流：返回 `(nal_unit_type, 载荷)` 序列。
///
/// HEVC NAL 头固定 2 字节，载荷长度小于 2 字节或结构不以起始码开头时拒绝（返回 `None`）。
pub(crate) fn split_nals(input: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    if input.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(6);
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
        if payload.len() < 2 {
            // HEVC NAL 头必须至少 2 字节。
            return None;
        }
        if (payload[0] & 0x80) != 0 {
            // forbidden_zero_bit 必须为 0。
            return None;
        }
        out.push((nal_unit_type(payload[0]), payload));
        pos = next;
    }
    Some(out)
}

/// HEVC 参数集切片集合 `(VPS, SPS, PPS)`。
pub(crate) struct HevcParamSets<'a> {
    pub(crate) vps: Option<&'a [u8]>,
    pub(crate) sps: Option<&'a [u8]>,
    pub(crate) pps: Option<&'a [u8]>,
}

/// 从 HEVC NAL 序列中提取首组参数集 `(VPS, SPS, PPS)`。
pub(crate) fn parameter_sets<'a>(nals: &'a [(u8, &'a [u8])]) -> HevcParamSets<'a> {
    let vps = nals
        .iter()
        .find(|(t, _)| *t == NAL_VPS)
        .map(|(_, payload)| *payload);
    let sps = nals
        .iter()
        .find(|(t, _)| *t == NAL_SPS)
        .map(|(_, payload)| *payload);
    let pps = nals
        .iter()
        .find(|(t, _)| *t == NAL_PPS)
        .map(|(_, payload)| *payload);
    HevcParamSets { vps, sps, pps }
}

/// 将 HEVC NAL 序列转为 4 字节大端长度前缀 HVCC，剥离 VPS/SPS/PPS，写入可复用缓冲 `out`。
pub(crate) fn nals_to_hvcc_into(nals: &[(u8, &[u8])], out: &mut Vec<u8>) -> Option<()> {
    out.clear();
    let mut total_bytes = 0usize;
    let mut slice_count = 0usize;
    for &(nal_type, payload) in nals {
        if nal_type == NAL_VPS || nal_type == NAL_SPS || nal_type == NAL_PPS {
            continue;
        }
        if payload.len() > u32::MAX as usize {
            return None;
        }
        slice_count += 1;
        total_bytes = total_bytes.checked_add(4)?.checked_add(payload.len())?;
    }
    if slice_count == 0 {
        return None;
    }
    out.reserve(total_bytes);
    for &(nal_type, payload) in nals {
        if nal_type == NAL_VPS || nal_type == NAL_SPS || nal_type == NAL_PPS {
            continue;
        }
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hevc_nal_hdr(nal_type: u8) -> [u8; 2] {
        [(nal_type & 0x3f) << 1, 0x01]
    }

    #[test]
    fn split_extract_and_hvcc_roundtrip() {
        let mut stream = Vec::new();
        // VPS (32)
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&hevc_nal_hdr(NAL_VPS));
        stream.extend_from_slice(&[0xaa, 0xbb]);
        // SPS (33)
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(&hevc_nal_hdr(NAL_SPS));
        stream.extend_from_slice(&[0xcc, 0xdd, 0xee]);
        // PPS (34)
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&hevc_nal_hdr(NAL_PPS));
        stream.extend_from_slice(&[0xff]);
        // IDR_W_RADL (19)
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&hevc_nal_hdr(19));
        stream.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);

        let nals = split_nals(&stream).expect("valid HEVC Annex-B");
        assert_eq!(nals.len(), 4);
        assert_eq!(nals[0].0, NAL_VPS);
        assert_eq!(nals[1].0, NAL_SPS);
        assert_eq!(nals[2].0, NAL_PPS);
        assert!(is_irap_nal(nals[3].0));

        let params = parameter_sets(&nals);
        assert!(params.vps.is_some() && params.sps.is_some() && params.pps.is_some());

        let mut hvcc = Vec::new();
        nals_to_hvcc_into(&nals, &mut hvcc).expect("HVCC output");
        assert_eq!(hvcc.len(), 4 + 6);
        assert_eq!(&hvcc[..4], &6u32.to_be_bytes());
        assert_eq!(&hvcc[4..6], &hevc_nal_hdr(19));
    }

    #[test]
    fn rejects_short_forbidden_and_params_only() {
        // Empty & non-AnnexB rejected
        assert!(split_nals(&[]).is_none());
        assert!(split_nals(&[0, 0, 0, 2, 0x26, 0x01]).is_none());
        // Single-byte NAL rejected (HEVC header is 2 bytes)
        assert!(split_nals(&[0, 0, 0, 1, 0x02]).is_none());
        // forbidden_zero_bit = 1 rejected
        assert!(split_nals(&[0, 0, 0, 1, 0x82, 0x01]).is_none());

        // Parameter sets only without slice -> nals_to_hvcc_into returns None
        let mut params_only = vec![0, 0, 0, 1];
        params_only.extend_from_slice(&hevc_nal_hdr(NAL_VPS));
        params_only.push(0x00);
        let nals = split_nals(&params_only).expect("valid VPS NAL");
        let mut hvcc = Vec::new();
        assert!(nals_to_hvcc_into(&nals, &mut hvcc).is_none());
    }
}
