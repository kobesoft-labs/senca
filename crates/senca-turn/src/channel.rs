//! ChannelData メッセージ（RFC 8656 12.4）。

use bytes::Bytes;

/// channel 番号の範囲（RFC 8656 12: 0x4000 から 0x4FFF）。
pub const CHANNEL_MIN: u16 = 0x4000;
pub const CHANNEL_MAX: u16 = 0x4FFF;

/// channel 番号として正しいか。
pub fn is_valid_channel(n: u16) -> bool {
    (CHANNEL_MIN..=CHANNEL_MAX).contains(&n)
}

/// ChannelData らしいか（先頭 2 bit が 01、RFC 7983 の 64..=79）。
pub fn is_channel_data(buf: &[u8]) -> bool {
    buf.len() >= 4 && (0x40..=0x4F).contains(&buf[0])
}

/// ChannelData メッセージ。
#[derive(Clone, PartialEq, Eq)]
pub struct ChannelData {
    pub number: u16,
    pub data: Bytes,
}

impl std::fmt::Debug for ChannelData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ChannelData(0x{:04x}, {} bytes)",
            self.number,
            self.data.len()
        )
    }
}

/// ChannelData の誤り。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelDataError {
    TooShort,
    BadNumber,
    BadLength,
}

impl ChannelData {
    /// 解析する。UDP では長さの後ろに padding があってよい（長さより短いのは誤り）。
    pub fn parse(buf: Bytes) -> Result<ChannelData, ChannelDataError> {
        if buf.len() < 4 {
            return Err(ChannelDataError::TooShort);
        }
        let number = u16::from_be_bytes([buf[0], buf[1]]);
        if !is_valid_channel(number) {
            return Err(ChannelDataError::BadNumber);
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if 4 + len > buf.len() || buf.len() > 4 + ((len + 3) & !3) {
            return Err(ChannelDataError::BadLength);
        }
        Ok(ChannelData {
            number,
            data: buf.slice(4..4 + len),
        })
    }

    /// 組み立てる。`pad` が真なら 4 バイト境界まで詰める（TCP では必須、UDP では任意）。
    pub fn encode(number: u16, data: &[u8], pad: bool) -> Bytes {
        let mut v = Vec::with_capacity(4 + data.len() + 3);
        v.extend_from_slice(&number.to_be_bytes());
        v.extend_from_slice(&(data.len().min(0xFFFF) as u16).to_be_bytes());
        v.extend_from_slice(&data[..data.len().min(0xFFFF)]);
        if pad {
            while v.len() % 4 != 0 {
                v.push(0);
            }
        }
        Bytes::from(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_padding() {
        let b = ChannelData::encode(0x4001, b"abcde", true);
        assert_eq!(b.len(), 12);
        let c = ChannelData::parse(b).unwrap();
        assert_eq!(c.number, 0x4001);
        assert_eq!(&c.data[..], b"abcde");
        let b = ChannelData::encode(0x4FFF, b"abcde", false);
        assert_eq!(b.len(), 9);
        assert_eq!(&ChannelData::parse(b).unwrap().data[..], b"abcde");
    }

    #[test]
    fn rejects() {
        assert_eq!(
            ChannelData::parse(Bytes::from_static(&[0x40, 0, 0])),
            Err(ChannelDataError::TooShort)
        );
        assert_eq!(
            ChannelData::parse(Bytes::from_static(&[0x50, 0, 0, 0])),
            Err(ChannelDataError::BadNumber)
        );
        assert_eq!(
            ChannelData::parse(Bytes::from_static(&[0x40, 0, 0, 5, 1])),
            Err(ChannelDataError::BadLength)
        );
        // padding が 3 バイトを越える
        assert_eq!(
            ChannelData::parse(Bytes::from_static(&[0x40, 0, 0, 1, 1, 0, 0, 0, 0])),
            Err(ChannelDataError::BadLength)
        );
    }
}
