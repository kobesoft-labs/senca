//! VISCA over IP（Sony の公開仕様、UDP 52381）のコマンドの組み立てと解釈（REQ-009 カメラ制御、REQ-008 R8-13）。
//!
//! 2 つの使い方:
//! - 操作する側（PTZ カメラへ送る）: [`Command::to_packet`] で送る、返事を [`Packet::parse`] → [`Reply::parse`]。
//! - 操作される側（スマホのアプリが VISCA の受け口になる）: [`Packet::parse`] → [`Command::parse`]、
//!   返事は [`Reply::to_packet`]（ACK のあと Completion）。パン・チルトはアプリが切り抜きで模擬する。
//!
//! 形: 8 バイトの見出し（payload type 2・payload length 2・sequence number 4、big endian）+ payload。
//! VISCA の payload は `8x … FF`（x はカメラの番号、IP では 1）で 1〜16 バイト。

use std::fmt;

/// VISCA over IP の UDP ポート。
pub const VISCA_PORT: u16 = 52381;
const MAX_PAYLOAD: usize = 16;

/// 見出しの payload type。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadType {
    Command,
    Inquiry,
    Reply,
    DeviceSetting,
    Control,
    ControlReply,
}

impl PayloadType {
    pub fn value(self) -> u16 {
        match self {
            PayloadType::Command => 0x0100,
            PayloadType::Inquiry => 0x0110,
            PayloadType::Reply => 0x0111,
            PayloadType::DeviceSetting => 0x0120,
            PayloadType::Control => 0x0200,
            PayloadType::ControlReply => 0x0201,
        }
    }

    pub fn from_value(v: u16) -> Option<Self> {
        Some(match v {
            0x0100 => PayloadType::Command,
            0x0110 => PayloadType::Inquiry,
            0x0111 => PayloadType::Reply,
            0x0120 => PayloadType::DeviceSetting,
            0x0200 => PayloadType::Control,
            0x0201 => PayloadType::ControlReply,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViscaError {
    Truncated,
    BadHeader,
    /// payload の長さが見出しと合わない、または 16 バイトを超える。
    BadLength,
    /// `8x … FF` の形でない。
    BadFraming,
    /// 知らないコマンド。
    Unknown,
    /// 値が範囲外。
    OutOfRange,
}

impl fmt::Display for ViscaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "visca: {self:?}")
    }
}

impl std::error::Error for ViscaError {}

/// VISCA over IP の 1 パケット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub payload_type: PayloadType,
    pub seq: u32,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + self.payload.len());
        v.extend_from_slice(&self.payload_type.value().to_be_bytes());
        v.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        v.extend_from_slice(&self.seq.to_be_bytes());
        v.extend_from_slice(&self.payload);
        v
    }

    pub fn parse(b: &[u8]) -> Result<Packet, ViscaError> {
        if b.len() < 8 {
            return Err(ViscaError::Truncated);
        }
        let pt = PayloadType::from_value(u16::from_be_bytes([b[0], b[1]]))
            .ok_or(ViscaError::BadHeader)?;
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        if len == 0 || len > MAX_PAYLOAD || b.len() != 8 + len {
            return Err(ViscaError::BadLength);
        }
        Ok(Packet {
            payload_type: pt,
            seq: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            payload: b[8..].to_vec(),
        })
    }

    /// 順番号を 0 に戻す（control の RESET）。
    pub fn reset_sequence(seq: u32) -> Packet {
        Packet {
            payload_type: PayloadType::Control,
            seq,
            payload: vec![0x01],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WhiteBalance {
    Auto = 0x00,
    Indoor = 0x01,
    Outdoor = 0x02,
    OnePush = 0x03,
    Manual = 0x05,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exposure {
    FullAuto = 0x00,
    Manual = 0x03,
    ShutterPriority = 0x0A,
    IrisPriority = 0x0B,
    Bright = 0x0D,
}

/// パン（左右）の向き。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Pan {
    Left = 1,
    Right = 2,
    Stop = 3,
}

/// チルト（上下）の向き。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tilt {
    Up = 1,
    Down = 2,
    Stop = 3,
}

/// 扱うコマンド（共通の操作 `zoom`・`focus`・`exposure`・`white_balance`・`preset.recall`・`pan_tilt` に対応）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Power(bool),
    ZoomStop,
    /// 速さ（0〜7）、None は標準の速さ。
    ZoomTele(Option<u8>),
    ZoomWide(Option<u8>),
    /// 位置（0x0000〜0x4000 など機種による）。
    ZoomDirect(u16),
    FocusStop,
    FocusFar(Option<u8>),
    FocusNear(Option<u8>),
    FocusDirect(u16),
    FocusAuto,
    FocusManual,
    FocusOnePush,
    WhiteBalance(WhiteBalance),
    Exposure(Exposure),
    /// プリセット番号（0〜127）。
    PresetReset(u8),
    PresetSet(u8),
    PresetRecall(u8),
    /// パンの速さ 1〜0x18、チルトの速さ 1〜0x17。
    PanTilt {
        pan_speed: u8,
        tilt_speed: u8,
        pan: Pan,
        tilt: Tilt,
    },
    PanTiltHome,
    /// ズーム位置の問い合わせ（返事は Completion の 4 ニブル）。
    ZoomPositionInquiry,
}

fn nibbles(v: u16) -> [u8; 4] {
    [
        (v >> 12) as u8 & 15,
        (v >> 8) as u8 & 15,
        (v >> 4) as u8 & 15,
        v as u8 & 15,
    ]
}

fn from_nibbles(b: &[u8]) -> Result<u16, ViscaError> {
    if b.len() != 4 || b.iter().any(|&x| x > 15) {
        return Err(ViscaError::OutOfRange);
    }
    Ok(b.iter().fold(0u16, |a, &x| a << 4 | x as u16))
}

fn speed(s: Option<u8>, base: u8, std: u8) -> Result<u8, ViscaError> {
    match s {
        None => Ok(std),
        Some(p) if p <= 7 => Ok(base | p),
        Some(_) => Err(ViscaError::OutOfRange),
    }
}

impl Command {
    pub fn payload_type(&self) -> PayloadType {
        match self {
            Command::ZoomPositionInquiry => PayloadType::Inquiry,
            _ => PayloadType::Command,
        }
    }

    /// VISCA の payload（`8x … FF`）。`addr` はカメラの番号（IP では 1）。
    pub fn to_visca(&self, addr: u8) -> Result<Vec<u8>, ViscaError> {
        if !(1..=7).contains(&addr) {
            return Err(ViscaError::OutOfRange);
        }
        let h = 0x80 | addr;
        let preset = |op: u8, n: u8| {
            if n <= 127 {
                Ok(vec![h, 0x01, 0x04, 0x3F, op, n, 0xFF])
            } else {
                Err(ViscaError::OutOfRange)
            }
        };
        Ok(match *self {
            Command::Power(on) => vec![h, 0x01, 0x04, 0x00, if on { 0x02 } else { 0x03 }, 0xFF],
            Command::ZoomStop => vec![h, 0x01, 0x04, 0x07, 0x00, 0xFF],
            Command::ZoomTele(s) => vec![h, 0x01, 0x04, 0x07, speed(s, 0x20, 0x02)?, 0xFF],
            Command::ZoomWide(s) => vec![h, 0x01, 0x04, 0x07, speed(s, 0x30, 0x03)?, 0xFF],
            Command::ZoomDirect(p) => {
                let n = nibbles(p);
                vec![h, 0x01, 0x04, 0x47, n[0], n[1], n[2], n[3], 0xFF]
            }
            Command::FocusStop => vec![h, 0x01, 0x04, 0x08, 0x00, 0xFF],
            Command::FocusFar(s) => vec![h, 0x01, 0x04, 0x08, speed(s, 0x20, 0x02)?, 0xFF],
            Command::FocusNear(s) => vec![h, 0x01, 0x04, 0x08, speed(s, 0x30, 0x03)?, 0xFF],
            Command::FocusDirect(p) => {
                let n = nibbles(p);
                vec![h, 0x01, 0x04, 0x48, n[0], n[1], n[2], n[3], 0xFF]
            }
            Command::FocusAuto => vec![h, 0x01, 0x04, 0x38, 0x02, 0xFF],
            Command::FocusManual => vec![h, 0x01, 0x04, 0x38, 0x03, 0xFF],
            Command::FocusOnePush => vec![h, 0x01, 0x04, 0x18, 0x01, 0xFF],
            Command::WhiteBalance(m) => vec![h, 0x01, 0x04, 0x35, m as u8, 0xFF],
            Command::Exposure(m) => vec![h, 0x01, 0x04, 0x39, m as u8, 0xFF],
            Command::PresetReset(n) => preset(0x00, n)?,
            Command::PresetSet(n) => preset(0x01, n)?,
            Command::PresetRecall(n) => preset(0x02, n)?,
            Command::PanTilt {
                pan_speed,
                tilt_speed,
                pan,
                tilt,
            } => {
                if !(1..=0x18).contains(&pan_speed) || !(1..=0x17).contains(&tilt_speed) {
                    return Err(ViscaError::OutOfRange);
                }
                vec![
                    h, 0x01, 0x06, 0x01, pan_speed, tilt_speed, pan as u8, tilt as u8, 0xFF,
                ]
            }
            Command::PanTiltHome => vec![h, 0x01, 0x06, 0x04, 0xFF],
            Command::ZoomPositionInquiry => vec![h, 0x09, 0x04, 0x47, 0xFF],
        })
    }

    /// VISCA over IP のパケットにする。
    pub fn to_packet(&self, seq: u32) -> Result<Vec<u8>, ViscaError> {
        Ok(Packet {
            payload_type: self.payload_type(),
            seq,
            payload: self.to_visca(1)?,
        }
        .encode())
    }

    /// VISCA の payload を読む（カメラの番号は問わない）。
    pub fn parse(m: &[u8]) -> Result<Command, ViscaError> {
        if m.len() < 3
            || m[0] & 0xF8 != 0x80
            || m[0] == 0x80
            || m[m.len() - 1] != 0xFF
            || m[..m.len() - 1].contains(&0xFF)
        {
            return Err(ViscaError::BadFraming);
        }
        let b = &m[1..m.len() - 1];
        let var = |v: u8| Some(v & 0x0F).filter(|&p| p <= 7);
        Ok(match b {
            [0x01, 0x04, 0x00, 0x02] => Command::Power(true),
            [0x01, 0x04, 0x00, 0x03] => Command::Power(false),
            [0x01, 0x04, 0x07, 0x00] => Command::ZoomStop,
            [0x01, 0x04, 0x07, 0x02] => Command::ZoomTele(None),
            [0x01, 0x04, 0x07, 0x03] => Command::ZoomWide(None),
            [0x01, 0x04, 0x07, v] if v & 0xF0 == 0x20 && var(*v).is_some() => {
                Command::ZoomTele(var(*v))
            }
            [0x01, 0x04, 0x07, v] if v & 0xF0 == 0x30 && var(*v).is_some() => {
                Command::ZoomWide(var(*v))
            }
            [0x01, 0x04, 0x47, p @ ..] => Command::ZoomDirect(from_nibbles(p)?),
            [0x01, 0x04, 0x08, 0x00] => Command::FocusStop,
            [0x01, 0x04, 0x08, 0x02] => Command::FocusFar(None),
            [0x01, 0x04, 0x08, 0x03] => Command::FocusNear(None),
            [0x01, 0x04, 0x08, v] if v & 0xF0 == 0x20 && var(*v).is_some() => {
                Command::FocusFar(var(*v))
            }
            [0x01, 0x04, 0x08, v] if v & 0xF0 == 0x30 && var(*v).is_some() => {
                Command::FocusNear(var(*v))
            }
            [0x01, 0x04, 0x48, p @ ..] => Command::FocusDirect(from_nibbles(p)?),
            [0x01, 0x04, 0x38, 0x02] => Command::FocusAuto,
            [0x01, 0x04, 0x38, 0x03] => Command::FocusManual,
            [0x01, 0x04, 0x18, 0x01] => Command::FocusOnePush,
            [0x01, 0x04, 0x35, v] => Command::WhiteBalance(match v {
                0x00 => WhiteBalance::Auto,
                0x01 => WhiteBalance::Indoor,
                0x02 => WhiteBalance::Outdoor,
                0x03 => WhiteBalance::OnePush,
                0x05 => WhiteBalance::Manual,
                _ => return Err(ViscaError::Unknown),
            }),
            [0x01, 0x04, 0x39, v] => Command::Exposure(match v {
                0x00 => Exposure::FullAuto,
                0x03 => Exposure::Manual,
                0x0A => Exposure::ShutterPriority,
                0x0B => Exposure::IrisPriority,
                0x0D => Exposure::Bright,
                _ => return Err(ViscaError::Unknown),
            }),
            [0x01, 0x04, 0x3F, op, n] if *n <= 127 => match op {
                0x00 => Command::PresetReset(*n),
                0x01 => Command::PresetSet(*n),
                0x02 => Command::PresetRecall(*n),
                _ => return Err(ViscaError::Unknown),
            },
            [0x01, 0x06, 0x01, ps, ts, p, t] => {
                if !(1..=0x18).contains(ps) || !(1..=0x17).contains(ts) {
                    return Err(ViscaError::OutOfRange);
                }
                let pan = match p {
                    1 => Pan::Left,
                    2 => Pan::Right,
                    3 => Pan::Stop,
                    _ => return Err(ViscaError::OutOfRange),
                };
                let tilt = match t {
                    1 => Tilt::Up,
                    2 => Tilt::Down,
                    3 => Tilt::Stop,
                    _ => return Err(ViscaError::OutOfRange),
                };
                Command::PanTilt {
                    pan_speed: *ps,
                    tilt_speed: *ts,
                    pan,
                    tilt,
                }
            }
            [0x01, 0x06, 0x04] => Command::PanTiltHome,
            [0x09, 0x04, 0x47] => Command::ZoomPositionInquiry,
            _ => return Err(ViscaError::Unknown),
        })
    }
}

/// VISCA の返事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ack {
        socket: u8,
    },
    /// 問い合わせの返事は `data` に値が入る。
    Completion {
        socket: u8,
        data: Vec<u8>,
    },
    /// 02 = 構文の誤り、03 = コマンドバッファが一杯、04 = 取り消し、05 = ソケットなし、41 = 実行できない。
    Error {
        socket: u8,
        code: u8,
    },
}

impl Reply {
    pub const SYNTAX_ERROR: u8 = 0x02;
    pub const BUFFER_FULL: u8 = 0x03;
    pub const NOT_EXECUTABLE: u8 = 0x41;

    pub fn to_visca(&self) -> Vec<u8> {
        match self {
            Reply::Ack { socket } => vec![0x90, 0x40 | (socket & 15), 0xFF],
            Reply::Completion { socket, data } => {
                let mut v = vec![0x90, 0x50 | (socket & 15)];
                v.extend(data.iter().take(MAX_PAYLOAD - 3).copied());
                v.push(0xFF);
                v
            }
            Reply::Error { socket, code } => vec![0x90, 0x60 | (socket & 15), *code, 0xFF],
        }
    }

    pub fn to_packet(&self, seq: u32) -> Vec<u8> {
        Packet {
            payload_type: PayloadType::Reply,
            seq,
            payload: self.to_visca(),
        }
        .encode()
    }

    /// ズーム位置の問い合わせへの返事（`90 50 0p 0q 0r 0s FF`）。
    pub fn zoom_position(pos: u16) -> Reply {
        Reply::Completion {
            socket: 0,
            data: nibbles(pos).to_vec(),
        }
    }

    pub fn parse(m: &[u8]) -> Result<Reply, ViscaError> {
        if m.len() < 3 || m[0] & 0xF0 != 0x90 || m[m.len() - 1] != 0xFF {
            return Err(ViscaError::BadFraming);
        }
        let socket = m[1] & 15;
        Ok(match m[1] & 0xF0 {
            0x40 if m.len() == 3 => Reply::Ack { socket },
            0x50 => Reply::Completion {
                socket,
                data: m[2..m.len() - 1].to_vec(),
            },
            0x60 if m.len() == 4 => Reply::Error { socket, code: m[2] },
            _ => return Err(ViscaError::Unknown),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_command_bytes() {
        assert_eq!(
            Command::ZoomTele(Some(5)).to_visca(1).unwrap(),
            [0x81, 0x01, 0x04, 0x07, 0x25, 0xFF]
        );
        assert_eq!(
            Command::ZoomDirect(0x4000).to_visca(1).unwrap(),
            [0x81, 0x01, 0x04, 0x47, 4, 0, 0, 0, 0xFF]
        );
        assert_eq!(
            Command::PresetRecall(3).to_visca(1).unwrap(),
            [0x81, 0x01, 0x04, 0x3F, 0x02, 0x03, 0xFF]
        );
        assert_eq!(
            Command::PanTilt {
                pan_speed: 0x18,
                tilt_speed: 0x14,
                pan: Pan::Right,
                tilt: Tilt::Stop
            }
            .to_visca(1)
            .unwrap(),
            [0x81, 0x01, 0x06, 0x01, 0x18, 0x14, 0x02, 0x03, 0xFF]
        );
        // パケット: 01 00 | 00 06 | 00 00 00 2A | payload
        assert_eq!(
            Command::ZoomStop.to_packet(42).unwrap(),
            [
                0x01, 0x00, 0x00, 0x06, 0, 0, 0, 0x2A, 0x81, 0x01, 0x04, 0x07, 0x00, 0xFF
            ]
        );
        assert_eq!(
            Packet::reset_sequence(0).encode(),
            [0x02, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0x01]
        );
    }

    #[test]
    fn round_trip_all_commands() {
        let all = [
            Command::Power(true),
            Command::Power(false),
            Command::ZoomStop,
            Command::ZoomTele(None),
            Command::ZoomTele(Some(0)),
            Command::ZoomWide(Some(7)),
            Command::ZoomDirect(0x1234),
            Command::FocusStop,
            Command::FocusFar(None),
            Command::FocusNear(Some(3)),
            Command::FocusDirect(0xABCD),
            Command::FocusAuto,
            Command::FocusManual,
            Command::FocusOnePush,
            Command::WhiteBalance(WhiteBalance::Outdoor),
            Command::Exposure(Exposure::IrisPriority),
            Command::PresetReset(0),
            Command::PresetSet(127),
            Command::PresetRecall(9),
            Command::PanTilt {
                pan_speed: 1,
                tilt_speed: 1,
                pan: Pan::Left,
                tilt: Tilt::Up,
            },
            Command::PanTiltHome,
            Command::ZoomPositionInquiry,
        ];
        for c in all {
            let pkt = c.to_packet(7).unwrap();
            let p = Packet::parse(&pkt).unwrap();
            assert_eq!((p.payload_type, p.seq), (c.payload_type(), 7));
            assert_eq!(Command::parse(&p.payload).unwrap(), c);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            Command::ZoomTele(Some(8)).to_visca(1),
            Err(ViscaError::OutOfRange)
        );
        assert_eq!(
            Command::PresetSet(128).to_visca(1),
            Err(ViscaError::OutOfRange)
        );
        assert_eq!(Command::ZoomStop.to_visca(0), Err(ViscaError::OutOfRange));
        assert_eq!(
            Command::parse(&[0x81, 0x01, 0x04, 0x07, 0x00]),
            Err(ViscaError::BadFraming)
        );
        assert_eq!(
            Command::parse(&[0x81, 0xFF, 0x04, 0xFF]),
            Err(ViscaError::BadFraming)
        );
        assert_eq!(
            Command::parse(&[0x81, 0x01, 0x04, 0x47, 0x10, 0, 0, 0, 0xFF]),
            Err(ViscaError::OutOfRange)
        );
        assert_eq!(
            Command::parse(&[0x81, 0x01, 0x7F, 0xFF]),
            Err(ViscaError::Unknown)
        );
        assert_eq!(
            Packet::parse(&[0x01, 0x00, 0x00, 0x05, 0, 0, 0, 0, 0x81]),
            Err(ViscaError::BadLength)
        );
        assert_eq!(
            Packet::parse(&[0x09, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0x81]),
            Err(ViscaError::BadHeader)
        );
        assert_eq!(
            Packet::parse(&[0x01, 0x00, 0x00, 0x11, 0, 0, 0, 0]),
            Err(ViscaError::BadLength)
        );
    }

    #[test]
    fn replies() {
        assert_eq!(Reply::Ack { socket: 1 }.to_visca(), [0x90, 0x41, 0xFF]);
        assert_eq!(
            Reply::zoom_position(0x4000).to_visca(),
            [0x90, 0x50, 4, 0, 0, 0, 0xFF]
        );
        for r in [
            Reply::Ack { socket: 2 },
            Reply::Completion {
                socket: 1,
                data: vec![],
            },
            Reply::Error {
                socket: 1,
                code: Reply::NOT_EXECUTABLE,
            },
        ] {
            let p = Packet::parse(&r.to_packet(3)).unwrap();
            assert_eq!(p.payload_type, PayloadType::Reply);
            assert_eq!(Reply::parse(&p.payload).unwrap(), r);
        }
    }
}
