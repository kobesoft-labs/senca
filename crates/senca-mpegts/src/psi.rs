//! PSI（PAT・PMT）の組み立てと解析。

use crate::crc::crc32_mpeg2;

/// PSI セクションの最大長（PAT/PMT は section_length ≤ 1021）。
pub(crate) const MAX_SECTION: usize = 1024;

pub(crate) const TID_PAT: u8 = 0x00;
pub(crate) const TID_PMT: u8 = 0x02;

pub(crate) const DESC_REGISTRATION: u8 = 0x05;
pub(crate) const DESC_EXTENSION: u8 = 0x7F;
/// DVB の extension descriptor 内の Opus 用タグ（"Encapsulation of Opus in MPEG-2 TS"）。
pub(crate) const EXT_OPUS: u8 = 0x80;

fn finish_section(mut s: Vec<u8>) -> Vec<u8> {
    // section_length は CRC を含む、section_length フィールドの後ろの長さ
    let len = s.len() - 3 + 4;
    s[1] = (s[1] & 0xF0) | ((len >> 8) as u8 & 0x0F);
    s[2] = len as u8;
    let crc = crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

pub(crate) fn build_pat(ts_id: u16, program_number: u16, pmt_pid: u16, version: u8) -> Vec<u8> {
    let mut s = vec![TID_PAT, 0xB0, 0];
    s.extend_from_slice(&ts_id.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1));
    s.extend_from_slice(&[0, 0]);
    s.extend_from_slice(&program_number.to_be_bytes());
    s.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());
    finish_section(s)
}

pub(crate) struct PmtStream<'a> {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptors: &'a [u8],
}

pub(crate) fn build_pmt(program_number: u16, pcr_pid: u16, version: u8, streams: &[PmtStream<'_>]) -> Vec<u8> {
    let mut s = vec![TID_PMT, 0xB0, 0];
    s.extend_from_slice(&program_number.to_be_bytes());
    s.push(0xC1 | ((version & 0x1F) << 1));
    s.extend_from_slice(&[0, 0]);
    s.extend_from_slice(&(0xE000 | pcr_pid).to_be_bytes());
    s.extend_from_slice(&0xF000u16.to_be_bytes());
    for st in streams {
        s.push(st.stream_type);
        s.extend_from_slice(&(0xE000 | st.pid).to_be_bytes());
        s.extend_from_slice(&(0xF000 | st.descriptors.len() as u16).to_be_bytes());
        s.extend_from_slice(st.descriptors);
    }
    finish_section(s)
}

/// 構文の共通部分を検査して (table_id, table_id_extension, version, 本体) を返す。
/// 本体は last_section_number の後ろから CRC の前まで。
pub(crate) struct Section<'a> {
    pub table_id: u8,
    pub ext: u16,
    pub version: u8,
    pub current: bool,
    pub body: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SectionError {
    Syntax,
    Crc,
}

pub(crate) fn parse_section(s: &[u8]) -> Result<Section<'_>, SectionError> {
    if s.len() < 12 || s[1] & 0x80 == 0 {
        return Err(SectionError::Syntax);
    }
    let len = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
    if len + 3 != s.len() || len < 9 {
        return Err(SectionError::Syntax);
    }
    if crc32_mpeg2(s) != 0 {
        return Err(SectionError::Crc);
    }
    Ok(Section {
        table_id: s[0],
        ext: u16::from_be_bytes([s[3], s[4]]),
        version: (s[5] >> 1) & 0x1F,
        current: s[5] & 1 == 1,
        body: &s[8..s.len() - 4],
    })
}

/// PAT の本体から (program_number, pid) を列挙する。
pub(crate) fn pat_programs(body: &[u8]) -> Vec<(u16, u16)> {
    body.as_chunks::<4>()
        .0
        .iter()
        .map(|c| (u16::from_be_bytes([c[0], c[1]]), u16::from_be_bytes([c[2], c[3]]) & 0x1FFF))
        .collect()
}

pub(crate) struct PmtInfo<'a> {
    pub pcr_pid: u16,
    pub program_descriptors: &'a [u8],
    pub streams: Vec<PmtStream<'a>>,
}

pub(crate) fn parse_pmt(body: &[u8]) -> Option<PmtInfo<'_>> {
    if body.len() < 4 {
        return None;
    }
    let pcr_pid = u16::from_be_bytes([body[0], body[1]]) & 0x1FFF;
    let pil = (u16::from_be_bytes([body[2], body[3]]) & 0x0FFF) as usize;
    let program_descriptors = body.get(4..4 + pil)?;
    let mut p = 4 + pil;
    let mut streams = Vec::new();
    while p < body.len() {
        let h = body.get(p..p + 5)?;
        let stream_type = h[0];
        let pid = u16::from_be_bytes([h[1], h[2]]) & 0x1FFF;
        let el = (u16::from_be_bytes([h[3], h[4]]) & 0x0FFF) as usize;
        let descriptors = body.get(p + 5..p + 5 + el)?;
        streams.push(PmtStream { stream_type, pid, descriptors });
        p += 5 + el;
    }
    Some(PmtInfo { pcr_pid, program_descriptors, streams })
}

/// 記述子列から (tag, 中身) を列挙する。壊れた末尾は無視する。
pub(crate) fn descriptors(mut d: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        if d.len() < 2 {
            return None;
        }
        let tag = d[0];
        let len = d[1] as usize;
        let body = d.get(2..2 + len)?;
        d = &d[2 + len..];
        Some((tag, body))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_roundtrip() {
        let s = build_pat(1, 1, 0x1000, 3);
        // ffmpeg の PAT（tsid 1, program 1, PMT 0x1000, version 0）と同じ形
        let sec = parse_section(&s).unwrap();
        assert_eq!((sec.table_id, sec.ext, sec.version, sec.current), (0, 1, 3, true));
        assert_eq!(pat_programs(sec.body), vec![(1, 0x1000)]);
        let mut bad = s.clone();
        bad[9] ^= 1;
        assert_eq!(parse_section(&bad).err(), Some(SectionError::Crc));
    }

    #[test]
    fn ffmpeg_pat_crc() {
        // ffmpeg -f mpegts が出す PAT（CRC 2AB104B2）
        let s = [0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0x2A, 0xB1, 0x04, 0xB2];
        assert!(parse_section(&s).is_ok());
    }

    #[test]
    fn pmt_roundtrip() {
        let d = [DESC_REGISTRATION, 4, b'O', b'p', b'u', b's'];
        let s = build_pmt(
            1,
            0x100,
            0,
            &[
                PmtStream { stream_type: 0x1B, pid: 0x100, descriptors: &[] },
                PmtStream { stream_type: 0x06, pid: 0x101, descriptors: &d },
            ],
        );
        let sec = parse_section(&s).unwrap();
        let p = parse_pmt(sec.body).unwrap();
        assert_eq!(p.pcr_pid, 0x100);
        assert_eq!(p.streams.len(), 2);
        assert_eq!(p.streams[1].pid, 0x101);
        let ds: Vec<_> = descriptors(p.streams[1].descriptors).collect();
        assert_eq!(ds, vec![(DESC_REGISTRATION, &b"Opus"[..])]);
    }
}
