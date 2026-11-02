//! ADTS（ISO/IEC 14496-3 1.A.2.2）のヘッダーの解析・組み立てと、ADTS 列の分割。

use crate::asc::{AudioSpecificConfig, SAMPLING_FREQUENCIES};
use crate::error::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};

/// CRC なしの ADTS ヘッダーの長さ。
pub const ADTS_HEADER_LEN: usize = 7;
/// frame_length の上限（13 ビット）。
pub const MAX_FRAME_LEN: usize = 8191;

/// ADTS ヘッダー（adts_fixed_header ＋ adts_variable_header）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// ID ビットが 1（MPEG-2 AAC）。
    pub mpeg2: bool,
    /// 1 なら CRC なし（ヘッダー 7 バイト）、0 なら CRC あり（9 バイト）。
    pub protection_absent: bool,
    /// audioObjectType（profile + 1。1〜4）。
    pub object_type: u8,
    pub sampling_frequency_index: u8,
    pub channel_configuration: u8,
    /// ヘッダーを含むフレームの長さ。
    pub frame_length: u16,
    pub buffer_fullness: u16,
    /// number_of_raw_data_blocks_in_frame + 1。
    pub raw_data_blocks: u8,
}

impl AdtsHeader {
    /// 先頭 7 バイトを解析する。
    pub fn parse(data: &[u8]) -> Result<AdtsHeader> {
        let h: &[u8; 7] = data
            .get(..ADTS_HEADER_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::Truncated)?;
        if h[0] != 0xFF || h[1] & 0xF0 != 0xF0 {
            return Err(Error::Invalid("ADTS の syncword"));
        }
        if h[1] & 0x06 != 0 {
            return Err(Error::Invalid("ADTS の layer が 0 でない"));
        }
        let sampling_frequency_index = (h[2] >> 2) & 0x0F;
        if sampling_frequency_index as usize >= SAMPLING_FREQUENCIES.len() {
            return Err(Error::Invalid("samplingFrequencyIndex が予約値"));
        }
        let protection_absent = h[1] & 1 != 0;
        let frame_length =
            (((h[3] & 3) as u16) << 11) | ((h[4] as u16) << 3) | ((h[5] >> 5) as u16);
        let hdr = AdtsHeader {
            mpeg2: h[1] & 0x08 != 0,
            protection_absent,
            object_type: (h[2] >> 6) + 1,
            sampling_frequency_index,
            channel_configuration: ((h[2] & 1) << 2) | (h[3] >> 6),
            frame_length,
            buffer_fullness: (((h[5] & 0x1F) as u16) << 6) | ((h[6] >> 2) as u16),
            raw_data_blocks: (h[6] & 3) + 1,
        };
        if (frame_length as usize) < hdr.header_len() {
            return Err(Error::Invalid("ADTS の frame_length がヘッダーより短い"));
        }
        Ok(hdr)
    }

    /// ヘッダーの長さ（CRC を含む）。
    pub fn header_len(&self) -> usize {
        if self.protection_absent { 7 } else { 9 }
    }

    /// raw_data_block の長さ。
    pub fn payload_len(&self) -> usize {
        (self.frame_length as usize).saturating_sub(self.header_len())
    }

    pub fn sampling_frequency(&self) -> u32 {
        SAMPLING_FREQUENCIES[self.sampling_frequency_index as usize % SAMPLING_FREQUENCIES.len()]
    }

    /// AudioSpecificConfig に直す（channel_configuration が 0 なら PCE が要るので `Err`）。
    pub fn audio_specific_config(&self) -> Result<AudioSpecificConfig> {
        AudioSpecificConfig::new(
            self.object_type,
            self.sampling_frequency(),
            crate::asc::channels_of_configuration(self.channel_configuration)
                .ok_or(Error::Unsupported("channel_configuration 0（PCE）"))?,
        )
    }

    /// AudioSpecificConfig と raw_data_block の長さから、CRC なし・1 ブロックのヘッダーを作る。
    pub fn for_payload(asc: &AudioSpecificConfig, payload_len: usize) -> Result<AdtsHeader> {
        if !(1..=4).contains(&asc.object_type) {
            return Err(Error::Unsupported("ADTS で表せない audioObjectType"));
        }
        let sampling_frequency_index = asc
            .sampling_frequency_index
            .ok_or(Error::Unsupported("ADTS で表せない標本化周波数"))?;
        if asc.channel_configuration > 7 {
            return Err(Error::Unsupported("ADTS で表せない channelConfiguration"));
        }
        let frame_length = payload_len
            .checked_add(ADTS_HEADER_LEN)
            .filter(|&l| l <= MAX_FRAME_LEN)
            .ok_or(Error::Invalid("ADTS フレームが 8191 バイトを超える"))?;
        Ok(AdtsHeader {
            mpeg2: false,
            protection_absent: true,
            object_type: asc.object_type,
            sampling_frequency_index,
            channel_configuration: asc.channel_configuration,
            frame_length: frame_length as u16,
            buffer_fullness: 0x7FF,
            raw_data_blocks: 1,
        })
    }

    /// 7 バイトのヘッダーにする（CRC なしのみ）。
    pub fn to_bytes(&self) -> Result<[u8; 7]> {
        if !self.protection_absent {
            return Err(Error::Unsupported("CRC 付きの ADTS の組み立て"));
        }
        if !(1..=4).contains(&self.object_type)
            || self.sampling_frequency_index > 12
            || self.channel_configuration > 7
            || (self.frame_length as usize) > MAX_FRAME_LEN
            || (self.frame_length as usize) < ADTS_HEADER_LEN
            || self.buffer_fullness > 0x7FF
            || !(1..=4).contains(&self.raw_data_blocks)
        {
            return Err(Error::Invalid("ADTS ヘッダーの値が範囲外"));
        }
        let fl = self.frame_length;
        let bf = self.buffer_fullness;
        Ok([
            0xFF,
            0xF0 | ((self.mpeg2 as u8) << 3) | 1,
            ((self.object_type - 1) << 6)
                | (self.sampling_frequency_index << 2)
                | (self.channel_configuration >> 2),
            ((self.channel_configuration & 3) << 6) | ((fl >> 11) as u8 & 3),
            (fl >> 3) as u8,
            (((fl & 7) as u8) << 5) | ((bf >> 6) as u8 & 0x1F),
            (((bf & 0x3F) as u8) << 2) | (self.raw_data_blocks - 1),
        ])
    }
}

/// ADTS から取り出したフレーム。`payload` は raw_data_block（ヘッダー・CRC なし）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdtsFrame {
    pub header: AdtsHeader,
    pub payload: Bytes,
}

/// 先頭の 1 フレームを切り出す。足りなければ `Ok(None)`。
fn take_frame(data: &[u8]) -> Result<Option<(AdtsHeader, usize, usize)>> {
    if data.len() < ADTS_HEADER_LEN {
        return Ok(None);
    }
    let h = AdtsHeader::parse(data)?;
    if h.raw_data_blocks != 1 {
        return Err(Error::Unsupported("1 フレームに複数の raw_data_block"));
    }
    let len = h.frame_length as usize;
    if data.len() < len {
        return Ok(None);
    }
    Ok(Some((h, h.header_len(), len)))
}

/// ADTS のバイト列全体をフレームに分ける（コピーしない）。途中で切れていたら `Err(Truncated)`。
pub fn split_adts(data: &Bytes) -> Result<Vec<AdtsFrame>> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let (header, start, len) = take_frame(&data[pos..])?.ok_or(Error::Truncated)?;
        frames.push(AdtsFrame {
            header,
            payload: data.slice(pos + start..pos + len),
        });
        pos += len;
    }
    Ok(frames)
}

/// ADTS のバイト列を AudioSpecificConfig と raw_data_block の列にする。途中で設定が変わったら `Err`。
pub fn adts_to_raw(data: &Bytes) -> Result<(AudioSpecificConfig, Vec<Bytes>)> {
    let frames = split_adts(data)?;
    let first = frames.first().ok_or(Error::Truncated)?.header;
    let asc = first.audio_specific_config()?;
    let mut out = Vec::with_capacity(frames.len());
    for f in frames {
        if !same_config(&f.header, &first) {
            return Err(Error::Invalid("ADTS の設定が途中で変わる"));
        }
        out.push(f.payload);
    }
    Ok((asc, out))
}

/// 設定に関わる値（object type・周波数・チャンネル）が同じか。
pub fn same_config(a: &AdtsHeader, b: &AdtsHeader) -> bool {
    a.object_type == b.object_type
        && a.sampling_frequency_index == b.sampling_frequency_index
        && a.channel_configuration == b.channel_configuration
}

/// raw_data_block に ADTS ヘッダーを付ける。
pub fn wrap_adts(asc: &AudioSpecificConfig, payload: &[u8]) -> Result<Vec<u8>> {
    let h = AdtsHeader::for_payload(asc, payload.len())?.to_bytes()?;
    let mut out = Vec::with_capacity(h.len() + payload.len());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    Ok(out)
}

/// 少しずつ届く ADTS を受け取ってフレームを返す状態機械（sans-io）。
///
/// 壊れた入力（syncword 不一致など）では `Err` を返す。続けるなら [`AdtsReader::clear`] で捨てる。
#[derive(Debug, Default)]
pub struct AdtsReader {
    buf: BytesMut,
}

/// [`AdtsReader`] が溜める量の上限。
const MAX_BUFFERED: usize = 64 * 1024;

impl AdtsReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// バイト列を足す。溜めすぎたら `Err`（呼び出し側が先に [`AdtsReader::next_frame`] で取り出す）。
    pub fn push(&mut self, data: &[u8]) -> Result<()> {
        if self.buf.len() + data.len() > MAX_BUFFERED {
            return Err(Error::Invalid("ADTS の未処理データが多すぎる"));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// 完全なフレームがあれば 1 つ返す。
    pub fn next_frame(&mut self) -> Result<Option<AdtsFrame>> {
        let Some((header, start, len)) = take_frame(&self.buf)? else {
            return Ok(None);
        };
        let mut frame = self.buf.split_to(len).freeze();
        frame.advance(start);
        Ok(Some(AdtsFrame {
            header,
            payload: frame,
        }))
    }

    /// 溜まっているバイト数。
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// 溜まっているデータを捨てる。
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &[u8]) -> Vec<u8> {
        let asc = AudioSpecificConfig::new(2, 48000, 2).unwrap();
        wrap_adts(&asc, payload).unwrap()
    }

    #[test]
    fn header_roundtrip() {
        let asc = AudioSpecificConfig::new(2, 44100, 1).unwrap();
        let h = AdtsHeader::for_payload(&asc, 151).unwrap();
        let b = h.to_bytes().unwrap();
        // FFmpeg の出力（44.1 kHz モノラル、158 バイト）と同じ先頭
        assert_eq!(&b[..6], &[0xFF, 0xF1, 0x50, 0x40, 0x13, 0xDF]);
        let p = AdtsHeader::parse(&b).unwrap();
        assert_eq!(p, h);
        assert_eq!(p.frame_length, 158);
        assert_eq!(p.payload_len(), 151);
        assert_eq!(p.audio_specific_config().unwrap(), asc);
        assert_eq!(p.sampling_frequency(), 44100);
    }

    #[test]
    fn header_rejects() {
        let good = frame(&[1, 2, 3]);
        assert_eq!(AdtsHeader::parse(&good[..6]), Err(Error::Truncated));
        let mut b = good.clone();
        b[1] = 0xE1;
        assert!(AdtsHeader::parse(&b).is_err());
        let mut b = good.clone();
        b[1] |= 0x02; // layer
        assert!(AdtsHeader::parse(&b).is_err());
        let mut b = good.clone();
        b[2] = (b[2] & !0x3C) | (13 << 2);
        assert!(AdtsHeader::parse(&b).is_err());
        let mut b = good.clone();
        b[3] &= !3;
        b[4] = 0;
        b[5] &= 0x1F; // frame_length = 0
        assert!(AdtsHeader::parse(&b).is_err());
        let asc = AudioSpecificConfig::new(2, 48000, 2).unwrap();
        assert!(AdtsHeader::for_payload(&asc, 8185).is_err());
        assert!(AdtsHeader::for_payload(&asc, 8184).is_ok());
        let he = AudioSpecificConfig::new(5, 48000, 2).unwrap();
        assert!(AdtsHeader::for_payload(&he, 10).is_err());
        let odd = AudioSpecificConfig::new(2, 32001, 2).unwrap();
        assert!(AdtsHeader::for_payload(&odd, 10).is_err());
        let mut h = AdtsHeader::for_payload(&asc, 10).unwrap();
        h.protection_absent = false;
        assert!(h.to_bytes().is_err());
    }

    #[test]
    fn crc_header_length() {
        let mut b = frame(&[9; 5]);
        b[1] &= !1; // protection_absent = 0（CRC 2 バイトを含めて長さはそのまま）
        let h = AdtsHeader::parse(&b).unwrap();
        assert_eq!(h.header_len(), 9);
        let f = split_adts(&Bytes::from(b)).unwrap();
        assert_eq!(&f[0].payload[..], &[9, 9, 9]);
    }

    #[test]
    fn split_and_reader() {
        let mut stream = frame(&[1, 2, 3]);
        stream.extend(frame(&[4; 10]));
        stream.extend(frame(&[]));
        let bytes = Bytes::from(stream.clone());
        let frames = split_adts(&bytes).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(&frames[0].payload[..], &[1, 2, 3]);
        assert_eq!(frames[1].payload.len(), 10);
        let (asc, raws) = adts_to_raw(&bytes).unwrap();
        assert_eq!(asc.codecs_string(), "mp4a.40.2");
        assert_eq!(raws.len(), 3);
        assert!(split_adts(&bytes.slice(..bytes.len() - 1)).is_err());
        assert!(adts_to_raw(&Bytes::new()).is_err());

        // 1 バイトずつ入れる
        let mut r = AdtsReader::new();
        let mut got = Vec::new();
        for b in &stream {
            r.push(&[*b]).unwrap();
            while let Some(f) = r.next_frame().unwrap() {
                got.push(f.payload);
            }
        }
        assert_eq!(got, raws);
        assert_eq!(r.buffered(), 0);
        r.push(&[0x00; 8]).unwrap();
        assert!(r.next_frame().is_err());
        r.clear();
        assert!(r.push(&vec![0xFF; MAX_BUFFERED + 1]).is_err());
    }

    #[test]
    fn config_change_and_multiple_blocks() {
        let mut stream = frame(&[1]);
        let asc = AudioSpecificConfig::new(2, 44100, 2).unwrap();
        stream.extend(wrap_adts(&asc, &[2]).unwrap());
        assert!(adts_to_raw(&Bytes::from(stream)).is_err());
        let mut b = frame(&[1, 2]);
        b[6] |= 1; // raw_data_blocks = 2
        assert!(matches!(
            split_adts(&Bytes::from(b)),
            Err(Error::Unsupported(_))
        ));
    }
}
