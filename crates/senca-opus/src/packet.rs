//! Opus パケットの TOC と枠組み（RFC 6716 3.1〜3.4）。音声の復号はしない。

use crate::error::{Error, Result};

/// 1 フレームの最大バイト数（RFC 6716 3.2.1）。
pub const MAX_FRAME_BYTES: usize = 1275;
/// 1 パケットの最大の長さ（120 ms、48 kHz のサンプル数）。
pub const MAX_PACKET_SAMPLES: u32 = 5760;
/// code 3 のフレーム数の上限。
pub const MAX_FRAMES: usize = 48;

/// 符号化モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Silk,
    Hybrid,
    Celt,
}

/// 音声帯域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    Narrowband,
    Mediumband,
    Wideband,
    SuperWideband,
    Fullband,
}

/// TOC バイト（RFC 6716 3.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toc {
    pub config: u8,
    pub stereo: bool,
    /// フレームの数え方（0〜3）。
    pub code: u8,
}

impl Toc {
    pub fn from_byte(b: u8) -> Toc {
        Toc {
            config: b >> 3,
            stereo: b & 4 != 0,
            code: b & 3,
        }
    }

    pub fn to_byte(self) -> u8 {
        ((self.config & 31) << 3) | ((self.stereo as u8) << 2) | (self.code & 3)
    }

    pub fn mode(self) -> Mode {
        match self.config {
            0..=11 => Mode::Silk,
            12..=15 => Mode::Hybrid,
            _ => Mode::Celt,
        }
    }

    pub fn bandwidth(self) -> Bandwidth {
        match self.config {
            0..=3 => Bandwidth::Narrowband,
            4..=7 => Bandwidth::Mediumband,
            8..=11 => Bandwidth::Wideband,
            12..=13 => Bandwidth::SuperWideband,
            14..=15 => Bandwidth::Fullband,
            16..=19 => Bandwidth::Narrowband,
            20..=23 => Bandwidth::Wideband,
            24..=27 => Bandwidth::SuperWideband,
            _ => Bandwidth::Fullband,
        }
    }

    /// 1 フレームのサンプル数（48 kHz）。2.5 ms = 120、60 ms = 2880。
    pub fn frame_samples(self) -> u32 {
        match self.config {
            0..=11 => [480, 960, 1920, 2880][(self.config % 4) as usize],
            12..=15 => [480, 960][(self.config % 2) as usize],
            _ => [120, 240, 480, 960][(self.config % 4) as usize],
        }
    }
}

/// 解析したパケット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<'a> {
    pub toc: Toc,
    /// 各フレームの符号化データ（長さ 0 は DTX・欠落）。
    pub frames: Vec<&'a [u8]>,
    /// code 3 のパディングのバイト数。
    pub padding: usize,
}

impl Packet<'_> {
    /// パケットの長さ（48 kHz のサンプル数）。
    pub fn samples(&self) -> u32 {
        self.toc.frame_samples() * self.frames.len() as u32
    }
}

/// フレーム長（1 または 2 バイト、RFC 6716 3.2.1）を読む。戻り値は（長さ, 使ったバイト数）。
fn read_frame_len(data: &[u8]) -> Result<(usize, usize)> {
    let b0 = *data.first().ok_or(Error::Truncated)? as usize;
    if b0 < 252 {
        return Ok((b0, 1));
    }
    let b1 = *data.get(1).ok_or(Error::Truncated)? as usize;
    Ok((b1 * 4 + b0, 2))
}

fn check_len(len: usize) -> Result<()> {
    if len > MAX_FRAME_BYTES {
        return Err(Error::Invalid("フレームが 1275 バイトを超える"));
    }
    Ok(())
}

/// パケットを解析し、RFC 6716 3.4 の条件 [R1]〜[R7] を確かめる。
pub fn parse_packet(data: &[u8]) -> Result<Packet<'_>> {
    let toc = Toc::from_byte(*data.first().ok_or(Error::Truncated)?); // [R1]
    let body = &data[1..];
    let mut frames = Vec::new();
    let mut padding = 0;
    match toc.code {
        0 => {
            check_len(body.len())?; // [R2]
            frames.push(body);
        }
        1 => {
            if !body.len().is_multiple_of(2) {
                return Err(Error::Invalid("code 1 のパケット長が奇数でない")); // [R3]
            }
            let half = body.len() / 2;
            check_len(half)?;
            frames.push(&body[..half]);
            frames.push(&body[half..]);
        }
        2 => {
            let (len, n) = read_frame_len(body)?; // [R4]
            let rest = &body[n..];
            if len > rest.len() {
                return Err(Error::Invalid("code 2 の最初のフレームがパケットを超える"));
            }
            check_len(len)?;
            check_len(rest.len() - len)?;
            frames.push(&rest[..len]);
            frames.push(&rest[len..]);
        }
        _ => {
            let fc = *body.first().ok_or(Error::Truncated)?; // [R6][R7]: 2 バイト以上
            let vbr = fc & 0x80 != 0;
            let has_padding = fc & 0x40 != 0;
            let m = (fc & 0x3f) as usize;
            if m == 0 {
                return Err(Error::Invalid("code 3 のフレーム数が 0")); // [R5]
            }
            if toc.frame_samples() * m as u32 > MAX_PACKET_SAMPLES {
                return Err(Error::Invalid("パケットが 120 ms を超える")); // [R5]
            }
            let mut pos = 1;
            if has_padding {
                loop {
                    let b = *body.get(pos).ok_or(Error::Truncated)?;
                    pos += 1;
                    if b == 255 {
                        padding += 254;
                    } else {
                        padding += b as usize;
                        break;
                    }
                }
            }
            let avail = body
                .len()
                .checked_sub(pos)
                .and_then(|v| v.checked_sub(padding))
                .ok_or(Error::Invalid("パディングがパケットを超える"))?;
            let payload = &body[pos..pos + avail];
            if vbr {
                let mut lens = Vec::with_capacity(m);
                let mut p = 0;
                for _ in 0..m - 1 {
                    let (len, n) = read_frame_len(&payload[p..])?;
                    p += n;
                    check_len(len)?;
                    lens.push(len);
                }
                let mut data_pos = p;
                for len in lens {
                    let end = data_pos + len;
                    if end > payload.len() {
                        return Err(Error::Invalid("code 3 のフレームがパケットを超える")); // [R7]
                    }
                    frames.push(&payload[data_pos..end]);
                    data_pos = end;
                }
                let last = &payload[data_pos..];
                check_len(last.len())?;
                frames.push(last);
            } else {
                if !avail.is_multiple_of(m) {
                    return Err(Error::Invalid(
                        "code 3 CBR の長さがフレーム数で割り切れない",
                    )); // [R6]
                }
                let len = avail / m;
                check_len(len)?;
                if len == 0 {
                    frames.resize(m, &payload[..0]);
                } else {
                    frames.extend(payload.chunks_exact(len));
                }
            }
        }
    }
    Ok(Packet {
        toc,
        frames,
        padding,
    })
}

/// パケットの長さ（48 kHz のサンプル数）。枠組みが壊れていれば `Err`。
pub fn packet_samples(data: &[u8]) -> Result<u32> {
    parse_packet(data).map(|p| p.samples())
}

/// フレーム数だけを素早く数える（opus_packet_get_nb_frames 相当。長さの検査はしない）。
pub fn frame_count(data: &[u8]) -> Result<usize> {
    let toc = Toc::from_byte(*data.first().ok_or(Error::Truncated)?);
    Ok(match toc.code {
        0 => 1,
        1 | 2 => 2,
        _ => (*data.get(1).ok_or(Error::Truncated)? & 0x3f) as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_table() {
        // config 0: SILK NB 10 ms、config 3: 60 ms、config 13: Hybrid SWB 20 ms、config 16: CELT NB 2.5 ms
        let cases = [
            (0u8, Mode::Silk, Bandwidth::Narrowband, 480),
            (3, Mode::Silk, Bandwidth::Narrowband, 2880),
            (6, Mode::Silk, Bandwidth::Mediumband, 1920),
            (9, Mode::Silk, Bandwidth::Wideband, 960),
            (12, Mode::Hybrid, Bandwidth::SuperWideband, 480),
            (15, Mode::Hybrid, Bandwidth::Fullband, 960),
            (16, Mode::Celt, Bandwidth::Narrowband, 120),
            (21, Mode::Celt, Bandwidth::Wideband, 240),
            (26, Mode::Celt, Bandwidth::SuperWideband, 480),
            (31, Mode::Celt, Bandwidth::Fullband, 960),
        ];
        for (config, mode, bw, samples) in cases {
            let t = Toc::from_byte(config << 3 | 4 | 1);
            assert_eq!(t.config, config);
            assert!(t.stereo);
            assert_eq!(t.code, 1);
            assert_eq!(t.mode(), mode);
            assert_eq!(t.bandwidth(), bw);
            assert_eq!(t.frame_samples(), samples, "config {config}");
            assert_eq!(Toc::from_byte(t.to_byte()), t);
        }
    }

    const FB20: u8 = 31 << 3; // CELT FB 20 ms

    #[test]
    fn code0_1_2() {
        let p = parse_packet(&[FB20, 1, 2, 3]).unwrap();
        assert_eq!(p.frames, vec![&[1u8, 2, 3][..]]);
        assert_eq!(p.samples(), 960);
        // DTX（長さ 0）
        assert_eq!(parse_packet(&[FB20]).unwrap().frames.len(), 1);

        let p = parse_packet(&[FB20 | 1, 1, 2, 3, 4]).unwrap();
        assert_eq!(p.frames, vec![&[1u8, 2][..], &[3, 4][..]]);
        assert_eq!(p.samples(), 1920);
        assert!(parse_packet(&[FB20 | 1, 1, 2, 3]).is_err()); // [R3]

        let p = parse_packet(&[FB20 | 2, 1, 9, 8, 7]).unwrap();
        assert_eq!(p.frames, vec![&[9u8][..], &[8, 7][..]]);
        assert!(parse_packet(&[FB20 | 2, 5, 9]).is_err()); // [R4]
        assert_eq!(parse_packet(&[FB20 | 2]), Err(Error::Truncated));
        assert_eq!(parse_packet(&[FB20 | 2, 252]), Err(Error::Truncated));
        // 2 バイトの長さ: 252 + 4 * 1 = 256
        let mut v = vec![FB20 | 2, 252, 1];
        v.extend(vec![0xAA; 256 + 3]);
        let p = parse_packet(&v).unwrap();
        assert_eq!(p.frames[0].len(), 256);
        assert_eq!(p.frames[1].len(), 3);
    }

    #[test]
    fn frame_size_limit() {
        let mut v = vec![FB20];
        v.extend(vec![0; 1275]);
        assert!(parse_packet(&v).is_ok());
        v.push(0);
        assert!(parse_packet(&v).is_err()); // [R2]
        let mut v = vec![FB20 | 1];
        v.extend(vec![0; 2 * 1276]);
        assert!(parse_packet(&v).is_err());
        let mut v = vec![FB20 | 2, 0];
        v.extend(vec![0; 1276]);
        assert!(parse_packet(&v).is_err());
    }

    #[test]
    fn code3_cbr_vbr_padding() {
        // CBR 3 フレーム × 2 バイト
        let p = parse_packet(&[FB20 | 3, 3, 1, 1, 2, 2, 3, 3]).unwrap();
        assert_eq!(p.frames.len(), 3);
        assert_eq!(p.frames[2], &[3, 3]);
        assert_eq!(p.samples(), 2880);
        assert!(parse_packet(&[FB20 | 3, 3, 1, 1, 2, 2, 3]).is_err()); // [R6]
        // CBR で全フレーム長さ 0
        let p = parse_packet(&[FB20 | 3, 2]).unwrap();
        assert_eq!(p.frames, vec![&[][..], &[][..]]);
        // パディング（255 → 254 + 次、合計 254 + 2 = 256）
        let mut v = vec![FB20 | 3, 0x40 | 1, 255, 2, 7, 7];
        v.extend(vec![0; 256]);
        let p = parse_packet(&v).unwrap();
        assert_eq!(p.padding, 256);
        assert_eq!(p.frames, vec![&[7u8, 7][..]]);
        v.truncate(v.len() - 3); // パディングがパケットを超える
        assert!(parse_packet(&v).is_err());
        // VBR: 3 フレーム、長さ 1・2、残り 3
        let p = parse_packet(&[FB20 | 3, 0x80 | 3, 1, 2, 9, 8, 8, 7, 7, 7]).unwrap();
        assert_eq!(p.frames, vec![&[9u8][..], &[8, 8][..], &[7, 7, 7][..]]);
        assert!(parse_packet(&[FB20 | 3, 0x80 | 3, 1, 9, 9]).is_err()); // [R7]
        assert!(parse_packet(&[FB20 | 3, 0x80 | 3, 1]).is_err());
        // VBR ＋ パディング
        let p = parse_packet(&[FB20 | 3, 0xC0 | 2, 1, 1, 5, 6, 0]).unwrap();
        assert_eq!(p.frames, vec![&[5u8][..], &[6][..]]);
        assert_eq!(p.padding, 1);
    }

    #[test]
    fn code3_limits() {
        assert_eq!(parse_packet(&[FB20 | 3]), Err(Error::Truncated));
        assert!(parse_packet(&[FB20 | 3, 0]).is_err()); // [R5] フレーム数 0
        // 20 ms × 6 = 120 ms は可、7 は不可
        assert!(parse_packet(&[FB20 | 3, 6]).is_ok());
        assert!(parse_packet(&[FB20 | 3, 7]).is_err());
        // 2.5 ms × 48 = 120 ms は可、49 は不可
        let c = 16 << 3 | 3;
        assert!(parse_packet(&[c, 48]).is_ok());
        assert!(parse_packet(&[c, 49]).is_err());
        assert_eq!(packet_samples(&[c, 48]).unwrap(), 5760);
        assert_eq!(frame_count(&[c, 48]).unwrap(), 48);
        assert_eq!(frame_count(&[FB20 | 1]).unwrap(), 2);
        assert!(frame_count(&[]).is_err());
        assert!(parse_packet(&[]).is_err());
    }

    #[test]
    fn exhaustive_small_inputs_never_panic() {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let _ = parse_packet(&[a, b]);
                let _ = parse_packet(&[a, b, 0xFF, 0x01]);
                let _ = parse_packet(&[a, b, 0xFC, 0x01, 0x00]);
            }
        }
    }
}
