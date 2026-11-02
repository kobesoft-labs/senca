//! transport parameters（RFC 9000 18 章、RFC 9221 3 章）。

use std::time::Duration;

use crate::cid::{ConnectionId, ResetToken};
use crate::coding::Reader;
use crate::error::{TransportError, terr};
use crate::varint;

mod id {
    pub const ORIGINAL_DESTINATION_CONNECTION_ID: u64 = 0x00;
    pub const MAX_IDLE_TIMEOUT: u64 = 0x01;
    pub const STATELESS_RESET_TOKEN: u64 = 0x02;
    pub const MAX_UDP_PAYLOAD_SIZE: u64 = 0x03;
    pub const INITIAL_MAX_DATA: u64 = 0x04;
    pub const INITIAL_MAX_STREAM_DATA_BIDI_LOCAL: u64 = 0x05;
    pub const INITIAL_MAX_STREAM_DATA_BIDI_REMOTE: u64 = 0x06;
    pub const INITIAL_MAX_STREAM_DATA_UNI: u64 = 0x07;
    pub const INITIAL_MAX_STREAMS_BIDI: u64 = 0x08;
    pub const INITIAL_MAX_STREAMS_UNI: u64 = 0x09;
    pub const ACK_DELAY_EXPONENT: u64 = 0x0a;
    pub const MAX_ACK_DELAY: u64 = 0x0b;
    pub const DISABLE_ACTIVE_MIGRATION: u64 = 0x0c;
    pub const PREFERRED_ADDRESS: u64 = 0x0d;
    pub const ACTIVE_CONNECTION_ID_LIMIT: u64 = 0x0e;
    pub const INITIAL_SOURCE_CONNECTION_ID: u64 = 0x0f;
    pub const RETRY_SOURCE_CONNECTION_ID: u64 = 0x10;
    pub const MAX_DATAGRAM_FRAME_SIZE: u64 = 0x20;
}

/// transport parameters。省略時は RFC の既定値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportParameters {
    pub original_destination_connection_id: Option<ConnectionId>,
    /// 0 なら idle timeout なし。
    pub max_idle_timeout: Duration,
    pub stateless_reset_token: Option<ResetToken>,
    pub max_udp_payload_size: u64,
    pub initial_max_data: u64,
    pub initial_max_stream_data_bidi_local: u64,
    pub initial_max_stream_data_bidi_remote: u64,
    pub initial_max_stream_data_uni: u64,
    pub initial_max_streams_bidi: u64,
    pub initial_max_streams_uni: u64,
    pub ack_delay_exponent: u64,
    pub max_ack_delay: Duration,
    pub disable_active_migration: bool,
    /// preferred_address は使わない（受けても無視する）。送ったかどうかだけ持つ。
    pub has_preferred_address: bool,
    pub active_connection_id_limit: u64,
    pub initial_source_connection_id: Option<ConnectionId>,
    pub retry_source_connection_id: Option<ConnectionId>,
    /// 0 なら DATAGRAM 非対応。
    pub max_datagram_frame_size: u64,
}

impl Default for TransportParameters {
    fn default() -> Self {
        Self {
            original_destination_connection_id: None,
            max_idle_timeout: Duration::ZERO,
            stateless_reset_token: None,
            max_udp_payload_size: 65527,
            initial_max_data: 0,
            initial_max_stream_data_bidi_local: 0,
            initial_max_stream_data_bidi_remote: 0,
            initial_max_stream_data_uni: 0,
            initial_max_streams_bidi: 0,
            initial_max_streams_uni: 0,
            ack_delay_exponent: 3,
            max_ack_delay: Duration::from_millis(25),
            disable_active_migration: false,
            has_preferred_address: false,
            active_connection_id_limit: 2,
            initial_source_connection_id: None,
            retry_source_connection_id: None,
            max_datagram_frame_size: 0,
        }
    }
}

fn put_int(out: &mut Vec<u8>, id: u64, v: u64) {
    varint::write(out, id);
    varint::write(out, varint::encoded_len(v) as u64);
    varint::write(out, v);
}

fn put_bytes(out: &mut Vec<u8>, id: u64, v: &[u8]) {
    varint::write(out, id);
    varint::write(out, v.len() as u64);
    out.extend_from_slice(v);
}

impl TransportParameters {
    /// 符号化する。既定値と同じものは省く。
    pub fn encode(&self) -> Vec<u8> {
        let d = Self::default();
        let mut o = Vec::new();
        if let Some(c) = &self.original_destination_connection_id {
            put_bytes(&mut o, id::ORIGINAL_DESTINATION_CONNECTION_ID, c.as_slice());
        }
        if !self.max_idle_timeout.is_zero() {
            put_int(&mut o, id::MAX_IDLE_TIMEOUT, self.max_idle_timeout.as_millis() as u64);
        }
        if let Some(t) = &self.stateless_reset_token {
            put_bytes(&mut o, id::STATELESS_RESET_TOKEN, t);
        }
        let ints = [
            (id::MAX_UDP_PAYLOAD_SIZE, self.max_udp_payload_size, d.max_udp_payload_size),
            (id::INITIAL_MAX_DATA, self.initial_max_data, 0),
            (id::INITIAL_MAX_STREAM_DATA_BIDI_LOCAL, self.initial_max_stream_data_bidi_local, 0),
            (id::INITIAL_MAX_STREAM_DATA_BIDI_REMOTE, self.initial_max_stream_data_bidi_remote, 0),
            (id::INITIAL_MAX_STREAM_DATA_UNI, self.initial_max_stream_data_uni, 0),
            (id::INITIAL_MAX_STREAMS_BIDI, self.initial_max_streams_bidi, 0),
            (id::INITIAL_MAX_STREAMS_UNI, self.initial_max_streams_uni, 0),
            (id::ACK_DELAY_EXPONENT, self.ack_delay_exponent, 3),
            (id::MAX_ACK_DELAY, self.max_ack_delay.as_millis() as u64, 25),
            (id::ACTIVE_CONNECTION_ID_LIMIT, self.active_connection_id_limit, 2),
            (id::MAX_DATAGRAM_FRAME_SIZE, self.max_datagram_frame_size, 0),
        ];
        for (i, v, def) in ints {
            if v != def {
                put_int(&mut o, i, v);
            }
        }
        if self.disable_active_migration {
            varint::write(&mut o, id::DISABLE_ACTIVE_MIGRATION);
            varint::write(&mut o, 0);
        }
        if let Some(c) = &self.initial_source_connection_id {
            put_bytes(&mut o, id::INITIAL_SOURCE_CONNECTION_ID, c.as_slice());
        }
        if let Some(c) = &self.retry_source_connection_id {
            put_bytes(&mut o, id::RETRY_SOURCE_CONNECTION_ID, c.as_slice());
        }
        // grease（RFC 9000 18.1: 31 * N + 27）
        put_bytes(&mut o, 31 * 7 + 27, &[]);
        o
    }

    /// 相手の値を読んで検証する。`from_server` は相手が server か。
    pub(crate) fn decode(buf: &[u8], from_server: bool) -> Result<Self, TransportError> {
        let bad = |why: &str| terr!(TRANSPORT_PARAMETER_ERROR, "transport parameter: {why}");
        let mut p = Self::default();
        let mut r = Reader::new(buf);
        let mut seen: Vec<u64> = Vec::new();
        while !r.is_empty() {
            let i = r.varint().map_err(|_| bad("切れている"))?;
            let v = r.varint_bytes().map_err(|_| bad("切れている"))?;
            if i <= id::MAX_DATAGRAM_FRAME_SIZE {
                if seen.contains(&i) {
                    return Err(bad("重複"));
                }
                seen.push(i);
            }
            let int = || -> Result<u64, TransportError> {
                let (x, n) = varint::read(v).ok_or_else(|| bad("整数が不正"))?;
                if n != v.len() {
                    return Err(bad("整数の長さが不正"));
                }
                Ok(x)
            };
            let cid = || ConnectionId::new(v).ok_or_else(|| bad("connection ID が長すぎる"));
            let server_only = matches!(
                i,
                id::ORIGINAL_DESTINATION_CONNECTION_ID
                    | id::STATELESS_RESET_TOKEN
                    | id::PREFERRED_ADDRESS
                    | id::RETRY_SOURCE_CONNECTION_ID
            );
            if server_only && !from_server {
                return Err(bad("client が server 専用の値を送った"));
            }
            match i {
                id::ORIGINAL_DESTINATION_CONNECTION_ID => p.original_destination_connection_id = Some(cid()?),
                id::MAX_IDLE_TIMEOUT => p.max_idle_timeout = Duration::from_millis(int()?),
                id::STATELESS_RESET_TOKEN => {
                    p.stateless_reset_token = Some(v.try_into().map_err(|_| bad("reset token の長さ"))?);
                }
                id::MAX_UDP_PAYLOAD_SIZE => {
                    p.max_udp_payload_size = int()?;
                    if p.max_udp_payload_size < 1200 {
                        return Err(bad("max_udp_payload_size < 1200"));
                    }
                }
                id::INITIAL_MAX_DATA => p.initial_max_data = int()?,
                id::INITIAL_MAX_STREAM_DATA_BIDI_LOCAL => p.initial_max_stream_data_bidi_local = int()?,
                id::INITIAL_MAX_STREAM_DATA_BIDI_REMOTE => p.initial_max_stream_data_bidi_remote = int()?,
                id::INITIAL_MAX_STREAM_DATA_UNI => p.initial_max_stream_data_uni = int()?,
                id::INITIAL_MAX_STREAMS_BIDI | id::INITIAL_MAX_STREAMS_UNI => {
                    let x = int()?;
                    if x > 1 << 60 {
                        return Err(bad("max_streams > 2^60"));
                    }
                    if i == id::INITIAL_MAX_STREAMS_BIDI {
                        p.initial_max_streams_bidi = x;
                    } else {
                        p.initial_max_streams_uni = x;
                    }
                }
                id::ACK_DELAY_EXPONENT => {
                    p.ack_delay_exponent = int()?;
                    if p.ack_delay_exponent > 20 {
                        return Err(bad("ack_delay_exponent > 20"));
                    }
                }
                id::MAX_ACK_DELAY => {
                    let x = int()?;
                    if x >= 1 << 14 {
                        return Err(bad("max_ack_delay >= 2^14"));
                    }
                    p.max_ack_delay = Duration::from_millis(x);
                }
                id::DISABLE_ACTIVE_MIGRATION => {
                    if !v.is_empty() {
                        return Err(bad("disable_active_migration に値がある"));
                    }
                    p.disable_active_migration = true;
                }
                id::PREFERRED_ADDRESS => {
                    // 4+2+16+2+1+cid+16 以上。中身は使わない。
                    if v.len() < 41 {
                        return Err(bad("preferred_address が短い"));
                    }
                    let cl = v[24] as usize;
                    if cl == 0 || cl > 20 || v.len() != 25 + cl + 16 {
                        return Err(bad("preferred_address の connection ID"));
                    }
                    p.has_preferred_address = true;
                }
                id::ACTIVE_CONNECTION_ID_LIMIT => {
                    p.active_connection_id_limit = int()?;
                    if p.active_connection_id_limit < 2 {
                        return Err(bad("active_connection_id_limit < 2"));
                    }
                }
                id::INITIAL_SOURCE_CONNECTION_ID => p.initial_source_connection_id = Some(cid()?),
                id::RETRY_SOURCE_CONNECTION_ID => p.retry_source_connection_id = Some(cid()?),
                id::MAX_DATAGRAM_FRAME_SIZE => p.max_datagram_frame_size = int()?,
                _ => {} // 未知（grease を含む）は無視
            }
        }
        if p.initial_source_connection_id.is_none() {
            return Err(bad("initial_source_connection_id がない"));
        }
        if from_server && p.original_destination_connection_id.is_none() {
            return Err(bad("original_destination_connection_id がない"));
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validation() {
        let p = TransportParameters {
            original_destination_connection_id: ConnectionId::new(&[1, 2, 3, 4]),
            initial_source_connection_id: ConnectionId::new(&[5, 6]),
            stateless_reset_token: Some([9; 16]),
            max_idle_timeout: Duration::from_secs(30),
            initial_max_data: 1 << 20,
            initial_max_streams_bidi: 100,
            max_datagram_frame_size: 1200,
            disable_active_migration: true,
            ..Default::default()
        };
        let b = p.encode();
        assert_eq!(TransportParameters::decode(&b, true).unwrap(), p);
        // client は server 専用の値を送れない
        assert!(TransportParameters::decode(&b, false).is_err());
        // 重複
        let mut d = b.clone();
        d.extend_from_slice(&[0x04, 0x01, 0x05]);
        assert!(TransportParameters::decode(&d, true).is_err());
        // 範囲外
        let mut q = TransportParameters {
            initial_source_connection_id: ConnectionId::new(&[1]),
            ..Default::default()
        };
        q.ack_delay_exponent = 21;
        assert!(TransportParameters::decode(&q.encode(), false).is_err());
    }
}
