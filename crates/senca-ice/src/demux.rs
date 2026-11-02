//! 1 つの UDP ソケットに多重化された packet の振り分け（RFC 7983、RFC 9443 で QUIC を追加）。

/// 先頭バイトから見た packet の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// 0-3: STUN。
    Stun,
    /// 16-19: ZRTP。
    Zrtp,
    /// 20-63: DTLS。
    Dtls,
    /// 64-79: TURN の ChannelData。
    TurnChannel,
    /// 128-191: RTP・RTCP。
    Rtp,
    /// 192-255: QUIC（RFC 9443）。
    Quic,
    /// それ以外（捨てる）。
    Unknown,
}

/// 先頭バイトで分類する。空なら `Unknown`。
pub fn classify(packet: &[u8]) -> PacketKind {
    match packet.first() {
        Some(0..=3) => PacketKind::Stun,
        Some(16..=19) => PacketKind::Zrtp,
        Some(20..=63) => PacketKind::Dtls,
        Some(64..=79) => PacketKind::TurnChannel,
        Some(128..=191) => PacketKind::Rtp,
        Some(192..=255) => PacketKind::Quic,
        _ => PacketKind::Unknown,
    }
}

/// RTP と RTCP の区別（RFC 5761 4 節: payload type 64-95 は RTCP）。`classify` が `Rtp` のときに使う。
pub fn is_rtcp(packet: &[u8]) -> bool {
    packet.len() >= 2 && (192..=223).contains(&packet[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(classify(&[0, 1]), PacketKind::Stun);
        assert_eq!(classify(&[3]), PacketKind::Stun);
        assert_eq!(classify(&[4]), PacketKind::Unknown);
        assert_eq!(classify(&[16]), PacketKind::Zrtp);
        assert_eq!(classify(&[22, 254, 253]), PacketKind::Dtls);
        assert_eq!(classify(&[63]), PacketKind::Dtls);
        assert_eq!(classify(&[0x40]), PacketKind::TurnChannel);
        assert_eq!(classify(&[79]), PacketKind::TurnChannel);
        assert_eq!(classify(&[80]), PacketKind::Unknown);
        assert_eq!(classify(&[0x80, 96]), PacketKind::Rtp);
        assert_eq!(classify(&[191]), PacketKind::Rtp);
        assert_eq!(classify(&[192]), PacketKind::Quic);
        assert_eq!(classify(&[]), PacketKind::Unknown);
        assert!(is_rtcp(&[0x80, 200]));
        assert!(!is_rtcp(&[0x80, 96]));
    }
}
