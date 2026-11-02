//! ノード間のメッセージ。符号化は [`crate::codec`]。

use crate::types::{Entry, NodeId, SnapshotMeta};
use bytes::Bytes;

/// ノード間のメッセージ。`term` は送り手の term（PreVote とその許可だけは「なろうとしている term」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub from: NodeId,
    pub to: NodeId,
    pub term: u64,
    pub body: MsgBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsgBody {
    /// 事前投票（term を上げずに当選の見込みを確かめる）。
    PreVote {
        last_index: u64,
        last_term: u64,
    },
    PreVoteResp {
        reject: bool,
    },
    /// 投票依頼。`transfer` はリーダー移譲による選挙（lease を無視してよい）。
    Vote {
        last_index: u64,
        last_term: u64,
        transfer: bool,
    },
    VoteResp {
        reject: bool,
    },
    /// ログ複製。
    Append {
        prev_index: u64,
        prev_term: u64,
        entries: Vec<Entry>,
        commit: u64,
    },
    /// 複製の応答。成功なら `index` まで一致。拒否なら `index` は拒否した prev_index、
    /// `reject_hint`/`hint_term` は巻き戻し先の手がかり（自分のログで hint 以下の、term が hint_term の位置）。
    AppendResp {
        reject: bool,
        index: u64,
        reject_hint: u64,
        hint_term: u64,
    },
    /// 心拍。`read_seq` は ReadIndex の確認番号。
    Heartbeat {
        commit: u64,
        read_seq: u64,
    },
    HeartbeatResp {
        read_seq: u64,
    },
    /// snapshot の分割送信。`offset` からの `data`。`done` で最後。
    Snapshot {
        meta: SnapshotMeta,
        total_len: u64,
        offset: u64,
        data: Bytes,
        done: bool,
    },
    /// 分割送信の応答。`next_offset` まで受け取った。`reject` で中止（大きすぎるなど）。
    SnapshotResp {
        snap_index: u64,
        snap_term: u64,
        next_offset: u64,
        reject: bool,
    },
    /// リーダー移譲: 受け手はすぐに（PreVote なしで）選挙を始める。
    TimeoutNow,
}

impl MsgBody {
    pub(crate) fn kind(&self) -> u8 {
        match self {
            MsgBody::PreVote { .. } => 1,
            MsgBody::PreVoteResp { .. } => 2,
            MsgBody::Vote { .. } => 3,
            MsgBody::VoteResp { .. } => 4,
            MsgBody::Append { .. } => 5,
            MsgBody::AppendResp { .. } => 6,
            MsgBody::Heartbeat { .. } => 7,
            MsgBody::HeartbeatResp { .. } => 8,
            MsgBody::Snapshot { .. } => 9,
            MsgBody::SnapshotResp { .. } => 10,
            MsgBody::TimeoutNow => 11,
        }
    }
}
