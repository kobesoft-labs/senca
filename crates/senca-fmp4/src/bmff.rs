//! 箱の書き出しの小道具。

/// 箱を始める。大きさは [`end_box`] で埋める。
pub(crate) fn begin_box(buf: &mut Vec<u8>, fourcc: &[u8; 4]) -> usize {
    let p = buf.len();
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(fourcc);
    p
}

pub(crate) fn begin_full_box(buf: &mut Vec<u8>, fourcc: &[u8; 4], version: u8, flags: u32) -> usize {
    let p = begin_box(buf, fourcc);
    buf.extend_from_slice(&((version as u32) << 24 | (flags & 0xFF_FFFF)).to_be_bytes());
    p
}

/// 箱を閉じる。4 GiB を超える箱はここでは作らない（mdat だけ別に扱う）。
pub(crate) fn end_box(buf: &mut [u8], p: usize) {
    let size = (buf.len() - p) as u32;
    buf[p..p + 4].copy_from_slice(&size.to_be_bytes());
}

pub(crate) fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// tkhd/mvhd の単位行列。
pub(crate) const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

pub(crate) fn ftyp(buf: &mut Vec<u8>, major: &[u8; 4], minor: u32, compat: &[&[u8; 4]]) {
    let p = begin_box(buf, b"ftyp");
    buf.extend_from_slice(major);
    put_u32(buf, minor);
    for c in compat {
        buf.extend_from_slice(*c);
    }
    end_box(buf, p);
}
