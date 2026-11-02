//! H.264・H.265 の共通部品: NAL から AU を組み立てる（パラメーターセットの抽出・FU の再構成・欠落時の破棄）と、
//! NAL 列を single・aggregation・fragmentation の payload にする処理。

use super::{ConfigState, Frame, FrameQueue, MAX_AU_SIZE, Payload, SeqWatch};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_core::media::CodecConfig;
use std::collections::BTreeMap;

/// 1 つの NAL の上限。
pub(crate) const MAX_NAL_SIZE: usize = 8 << 20;
/// 1 つの AU に入れる NAL の数の上限。
pub(crate) const MAX_NALS_PER_AU: usize = 4096;
/// 覚えておくパラメーターセットの数の上限（種類ごと）。
const MAX_PARAM_SETS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamKind {
    Vps,
    Sps,
    Pps,
}

/// NAL の分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NalClass {
    /// パラメーターセット（種類と ID）。AU から抜いて設定に入れる。
    Param(ParamKind, u8),
    /// 捨てる（AUD・filler など）。
    Skip,
    /// スライス。`key` は IDR・IRAP。
    Vcl { key: bool },
    /// その他（SEI など）。AU に入れる。
    Other,
}

/// H.264 と H.265 の違い。
pub(crate) trait Flavor: Send + 'static {
    /// NAL ヘッダーの長さ。
    const HDR: usize;
    fn classify(nal: &[u8]) -> NalClass;
    fn build_config(
        vps: &[Bytes],
        sps: &[Bytes],
        pps: &[Bytes],
        generation: u32,
    ) -> Result<CodecConfig>;
    /// 設定（record）からパラメーターセットの NAL 列と NAL 長さのバイト数を取り出す。
    fn from_config(cfg: &CodecConfig) -> Result<(Vec<Bytes>, u8)>;
    /// aggregation packet の payload ヘッダー。
    fn agg_header(nals: &[Bytes]) -> Vec<u8>;
    /// fragmentation unit のヘッダー（payload の前に付く部分）。
    fn fu_header(nal: &[u8], start: bool, end: bool) -> Vec<u8>;
}

/// AU の組み立て。
pub(crate) struct NalAssembler<F: Flavor> {
    seq: SeqWatch,
    cur_ts: Option<u32>,
    nals: Vec<Bytes>,
    size: usize,
    key: bool,
    broken: bool,
    fu: Option<Vec<u8>>,
    vps: BTreeMap<u8, Bytes>,
    sps: BTreeMap<u8, Bytes>,
    pps: BTreeMap<u8, Bytes>,
    params_dirty: bool,
    pub(crate) cfg: ConfigState,
    waiting_key: bool,
    discontinuity: bool,
    frames: FrameQueue,
    _f: std::marker::PhantomData<F>,
}

impl<F: Flavor> NalAssembler<F> {
    pub(crate) fn new() -> Self {
        NalAssembler {
            seq: SeqWatch::default(),
            cur_ts: None,
            nals: Vec::new(),
            size: 0,
            key: false,
            broken: false,
            fu: None,
            vps: BTreeMap::new(),
            sps: BTreeMap::new(),
            pps: BTreeMap::new(),
            params_dirty: false,
            cfg: ConfigState::default(),
            waiting_key: true,
            discontinuity: false,
            frames: FrameQueue::default(),
            _f: std::marker::PhantomData,
        }
    }

    /// SDP（sprop-*）から得たパラメーターセットを入れる。
    pub(crate) fn add_out_of_band(&mut self, nals: &[Bytes]) {
        for n in nals {
            self.store_param(n);
        }
        self.refresh_config();
    }

    fn store_param(&mut self, nal: &Bytes) -> bool {
        if let NalClass::Param(kind, id) = F::classify(nal) {
            let map = match kind {
                ParamKind::Vps => &mut self.vps,
                ParamKind::Sps => &mut self.sps,
                ParamKind::Pps => &mut self.pps,
            };
            if map.get(&id) != Some(nal) && (map.contains_key(&id) || map.len() < MAX_PARAM_SETS) {
                map.insert(id, nal.clone());
                self.params_dirty = true;
            }
            return true;
        }
        false
    }

    fn refresh_config(&mut self) {
        if !self.params_dirty {
            return;
        }
        let v: Vec<Bytes> = self.vps.values().cloned().collect();
        let s: Vec<Bytes> = self.sps.values().cloned().collect();
        let p: Vec<Bytes> = self.pps.values().cloned().collect();
        if s.is_empty() || p.is_empty() {
            return;
        }
        if let Ok(c) = F::build_config(&v, &s, &p, 0) {
            self.params_dirty = false;
            self.cfg.update(c);
        }
    }

    /// パケットの処理を始める（抜けと timestamp の変化を見る）。
    pub(crate) fn begin(&mut self, pkt: &RtpPacket) {
        let gap = self.seq.gap(pkt.header.sequence);
        let ts = pkt.header.timestamp;
        let new_au = self.cur_ts != Some(ts);
        if new_au && self.cur_ts.is_some() {
            // marker が来ないまま次の AU が始まった（抜けがあれば前の AU の終わりが欠けている）
            if gap {
                self.broken = true;
            }
            self.finish();
        }
        if gap {
            self.fu = None;
            self.waiting_key = true;
            self.discontinuity = true;
            if !new_au {
                self.broken = true;
            }
        }
        self.cur_ts = Some(ts);
    }

    /// 損失: 組み立て中の AU を壊れたことにし、キーフレームを待つ。
    pub(crate) fn loss(&mut self) {
        self.broken = true;
        self.fu = None;
        self.waiting_key = true;
        self.discontinuity = true;
    }

    /// payload が壊れていた。
    pub(crate) fn set_broken(&mut self) {
        self.loss();
    }

    pub(crate) fn add_nal(&mut self, nal: Bytes) -> Result<()> {
        if nal.len() < F::HDR {
            return Err(Error::Invalid("NAL が短い"));
        }
        if nal[0] & 0x80 != 0 {
            return Err(Error::Invalid("forbidden_zero_bit"));
        }
        if self.store_param(&nal) {
            return Ok(());
        }
        match F::classify(&nal) {
            NalClass::Skip => return Ok(()),
            NalClass::Vcl { key } => self.key |= key,
            _ => {}
        }
        if self.nals.len() >= MAX_NALS_PER_AU {
            return Err(Error::Limit("AU の NAL の数"));
        }
        self.size += 4 + nal.len();
        if self.size > MAX_AU_SIZE {
            return Err(Error::Limit("AU の大きさ"));
        }
        self.nals.push(nal);
        Ok(())
    }

    pub(crate) fn fu_start(&mut self, header: &[u8], data: &[u8]) -> Result<()> {
        if self.fu.is_some() {
            // 前の FU が終わらないまま次が始まった
            self.broken = true;
        }
        let mut v = Vec::with_capacity(header.len() + data.len() * 4);
        v.extend_from_slice(header);
        v.extend_from_slice(data);
        self.fu = Some(v);
        Ok(())
    }

    /// FU の続き。始まりを受け取っていなければ `false`（この FU は捨てる）。
    pub(crate) fn fu_continue(&mut self, data: &[u8], end: bool) -> Result<()> {
        let Some(buf) = self.fu.as_mut() else {
            self.broken = true;
            return Ok(());
        };
        if buf.len() + data.len() > MAX_NAL_SIZE {
            self.fu = None;
            return Err(Error::Limit("FU の合計"));
        }
        buf.extend_from_slice(data);
        if end {
            let nal = Bytes::from(self.fu.take().unwrap_or_default());
            self.add_nal(nal)?;
        }
        Ok(())
    }

    /// パケットの処理を終える。
    pub(crate) fn end(&mut self, marker: bool) {
        if marker {
            if self.fu.is_some() {
                // marker のパケットで FU が終わっていない
                self.fu = None;
                self.broken = true;
            }
            self.finish();
        }
    }

    fn finish(&mut self) {
        let ts = self.cur_ts.take();
        let nals = std::mem::take(&mut self.nals);
        let key = std::mem::take(&mut self.key);
        let broken = std::mem::take(&mut self.broken);
        self.size = 0;
        if self.fu.take().is_some() {
            self.discontinuity = true;
            self.waiting_key = true;
            return;
        }
        self.refresh_config();
        let Some(ts) = ts else { return };
        if broken {
            self.discontinuity = true;
            self.waiting_key = true;
            return;
        }
        if nals.is_empty() {
            return;
        }
        if self.waiting_key && !key {
            self.discontinuity = true;
            return;
        }
        let Some(cur) = &self.cfg.current else {
            // 設定がまだ無い: 復号できないので捨てる
            self.discontinuity = true;
            return;
        };
        let generation = cur.generation;
        self.waiting_key = false;
        let total: usize = nals.iter().map(|n| 4 + n.len()).sum();
        let mut payload = Vec::with_capacity(total);
        for n in &nals {
            payload.extend_from_slice(&(n.len() as u32).to_be_bytes());
            payload.extend_from_slice(n);
        }
        self.frames.push(Frame {
            timestamp: ts,
            payload: Bytes::from(payload),
            keyframe: key,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: generation,
            duration: 0,
        });
    }

    pub(crate) fn pop(&mut self) -> Option<Frame> {
        self.frames.pop()
    }
}

/// NAL 列を payload にする。`aggregate` が偽なら single NAL だけ（大きすぎる NAL は `Err`）。
pub(crate) fn packetize_nals<F: Flavor>(
    nals: &[Bytes],
    mtu: usize,
    aggregate: bool,
    out: &mut Vec<Payload>,
) -> Result<()> {
    let start = out.len();
    let mut group: Vec<Bytes> = Vec::new();
    let agg_hdr = F::HDR;
    let mut group_size = agg_hdr;
    let flush = |group: &mut Vec<Bytes>, out: &mut Vec<Payload>| {
        match group.len() {
            0 => {}
            1 => out.push(Payload::new(group[0].clone(), false)),
            _ => {
                let mut v = F::agg_header(group);
                for n in group.iter() {
                    v.extend_from_slice(&(n.len() as u16).to_be_bytes());
                    v.extend_from_slice(n);
                }
                out.push(Payload::new(Bytes::from(v), false));
            }
        }
        group.clear();
    };
    for nal in nals {
        if nal.len() < F::HDR {
            return Err(Error::Invalid("NAL が短い"));
        }
        if nal.len() <= mtu {
            if !aggregate {
                out.push(Payload::new(nal.clone(), false));
                continue;
            }
            if group_size + 2 + nal.len() > mtu {
                flush(&mut group, out);
                group_size = agg_hdr;
            }
            group_size += 2 + nal.len();
            group.push(nal.clone());
            continue;
        }
        if !aggregate {
            return Err(Error::Limit("packetization-mode 0 で MTU を超える NAL"));
        }
        flush(&mut group, out);
        group_size = agg_hdr;
        let body = &nal[F::HDR..];
        let chunk = mtu - F::fu_header(nal, true, false).len();
        let n = body.len().div_ceil(chunk);
        for (i, part) in body.chunks(chunk).enumerate() {
            let mut v = F::fu_header(nal, i == 0, i + 1 == n);
            v.extend_from_slice(part);
            out.push(Payload::new(Bytes::from(v), false));
        }
    }
    flush(&mut group, out);
    if let Some(last) = out[start..].last_mut() {
        last.marker = true;
    }
    Ok(())
}

/// 長さ前置の AU を NAL に分け、キーフレームならパラメーターセットを先頭に足す（無いとき）。
pub(crate) fn au_nals<F: Flavor>(
    payload: &Bytes,
    keyframe: bool,
    config: Option<&CodecConfig>,
    split: impl Fn(&Bytes, u8) -> std::result::Result<Vec<Bytes>, ()>,
) -> Result<Vec<Bytes>> {
    let (params, length_size) = match config {
        Some(c) => F::from_config(c)?,
        None => (Vec::new(), 4),
    };
    let nals = split(payload, length_size).map_err(|_| Error::Invalid("長さ前置の NAL 列"))?;
    let has_params = nals
        .iter()
        .any(|n| matches!(F::classify(n), NalClass::Param(ParamKind::Sps, _)));
    let key = keyframe
        || nals
            .iter()
            .any(|n| matches!(F::classify(n), NalClass::Vcl { key: true }));
    if key && !has_params && !params.is_empty() {
        let mut v = params;
        v.extend(
            nals.into_iter()
                .filter(|n| F::classify(n) != NalClass::Skip),
        );
        return Ok(v);
    }
    Ok(nals
        .into_iter()
        .filter(|n| F::classify(n) != NalClass::Skip)
        .collect())
}
