//! H.264 復号の前処理（I/O を持たない状態機械）。
//!
//! AU を 1 つ入れると、ハードウェアにさせる仕事（[`Action`]）の列を返す:
//! - [`Action::Reinit`] — ストリームの大きさが変わった（サーフェスを作り直す）
//! - [`Action::Submit`] — 1 枚のピクチャーを復号する（ピクチャーと各スライスの値、参照リスト）
//! - [`Action::Output`] — 表示順で 1 枚を出す（サーフェスを読み出す）
//!
//! ここでするのは、POC の計算（8.2.1、型 0・1・2）、参照ピクチャーの印付け（8.2.5、スライディングウィンドウと
//! MMCO 1〜6、frame_num の欠落の補い）、参照リストの初期化と並べ替え（8.2.4）、DPB からの出力（C.4.5.3 の bumping）。
//! フレーム（プログレッシブ）だけを扱い、フィールド・MBAFF は `Unsupported` で拒否する。
//!
//! サーフェスは番号（0..`StreamInfo::surfaces`）で表す。`Output` の後のサーフェスは、次に `decode_au` を呼ぶまで
//! 上書きされない（呼び出し側はその間に読み出す）。

use super::params::{Pps, Sps, parse_pps, parse_sps};
use super::slice::{Mmco, SLICE_B, SLICE_I, SLICE_P, SliceHeader, parse_slice_header};
use crate::error::{CodecError, Result};
use bytes::Bytes;

/// ストリームの大きさ（サーフェスの作り直しに要る値）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub coded_width: u32,
    pub coded_height: u32,
    /// 切り抜き（左・右・上・下、輝度の画素）。
    pub crop: [u32; 4],
    pub bit_depth: u8,
    pub chroma_format_idc: u32,
    pub dpb_size: usize,
    /// 必要なサーフェスの数（DPB + 復号中の 1 枚 + 余裕 1 枚）。
    pub surfaces: usize,
}

impl StreamInfo {
    pub fn display_width(&self) -> u32 {
        self.coded_width - self.crop[0] - self.crop[1]
    }
    pub fn display_height(&self) -> u32 {
        self.coded_height - self.crop[2] - self.crop[3]
    }
}

/// 参照ピクチャー（VA-API の `VAPictureH264` に当たる値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    /// サーフェス番号。frame_num の欠落を補った「存在しない」フレームでは `None`。
    pub surface: Option<usize>,
    /// 短期は frame_num、長期は LongTermFrameIdx。
    pub frame_idx: u32,
    pub long_term: bool,
    pub top_poc: i32,
    pub bottom_poc: i32,
}

/// 1 スライスの仕事。
#[derive(Debug, Clone)]
pub struct SliceJob {
    /// スライスの NAL（NAL ヘッダーを含み、開始符号・長さは含まない。エミュレーション防止バイトはそのまま）。
    pub nal: Bytes,
    pub header: SliceHeader,
    /// 並べ替え後の参照リスト（長さ = num_ref_idx_active。該当が無い位置は `None`）。
    pub list0: Vec<Option<RefPic>>,
    pub list1: Vec<Option<RefPic>>,
}

/// 1 ピクチャーの仕事。
#[derive(Debug, Clone)]
pub struct PictureJob {
    pub surface: usize,
    pub sps: Sps,
    pub pps: Pps,
    pub frame_num: u32,
    pub top_poc: i32,
    pub bottom_poc: i32,
    pub is_reference: bool,
    pub is_idr: bool,
    /// DPB の中の参照ピクチャー（`VAPictureParameterBufferH264::ReferenceFrames`、最大 16）。
    pub refs: Vec<RefPic>,
    pub slices: Vec<SliceJob>,
}

/// 表示順で出す 1 枚。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputPic {
    pub surface: usize,
    pub pts: i64,
    pub keyframe: bool,
    pub poc: i32,
}

/// 参照リスト（該当が無い位置は `None`）。
pub type RefList = Vec<Option<RefPic>>;

#[derive(Debug, Clone)]
pub enum Action {
    Reinit(StreamInfo),
    Submit(Box<PictureJob>),
    Output(OutputPic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefState {
    None,
    Short,
    Long(u32),
}

#[derive(Debug, Clone)]
struct DpbFrame {
    surface: Option<usize>,
    frame_num: u32,
    top_poc: i32,
    bottom_poc: i32,
    reference: RefState,
    output_needed: bool,
    pts: i64,
    keyframe: bool,
}

impl DpbFrame {
    fn poc(&self) -> i32 {
        self.top_poc.min(self.bottom_poc)
    }
}

/// H.264 の復号の前処理。
#[derive(Debug)]
pub struct H264Frontend {
    sps: Vec<Option<Sps>>,
    pps_raw: Vec<Option<Bytes>>,
    pps: Vec<Option<Pps>>,
    max_width: u32,
    max_height: u32,
    info: Option<StreamInfo>,
    dpb: Vec<DpbFrame>,
    free: Vec<usize>,
    dpb_size: usize,
    reorder: usize,
    started: bool,
    // 8.2.1 の前のピクチャーの状態
    prev_ref_frame_num: u32,
    prev_frame_num: u32,
    prev_frame_num_offset: i64,
    prev_had_mmco5: bool,
    prev_poc_msb: i64,
    prev_poc_lsb: i64,
    max_long_term_frame_idx: Option<u32>,
}

/// 1 AU の NAL の数の上限。
const MAX_SLICES: usize = 1024;

impl H264Frontend {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        H264Frontend {
            sps: vec![None; 32],
            pps_raw: vec![None; 256],
            pps: vec![None; 256],
            max_width,
            max_height,
            info: None,
            dpb: Vec::new(),
            free: Vec::new(),
            dpb_size: 1,
            reorder: 0,
            started: false,
            prev_ref_frame_num: 0,
            prev_frame_num: 0,
            prev_frame_num_offset: 0,
            prev_had_mmco5: false,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            max_long_term_frame_idx: None,
        }
    }

    pub fn stream_info(&self) -> Option<&StreamInfo> {
        self.info.as_ref()
    }

    /// パラメーターセット（SPS・PPS の NAL）を入れる。
    pub fn add_parameter_set(&mut self, nal: &Bytes) -> Result<()> {
        match nal.first().map(|b| b & 0x1f) {
            Some(7) => {
                let s = parse_sps(nal)?;
                let id = s.sps_id as usize;
                let changed = self.sps[id].as_ref() != Some(&s);
                self.sps[id] = Some(s);
                if changed {
                    // この SPS を参照する PPS は倍率表の補い方が変わりうるので読み直す。
                    for i in 0..256 {
                        if let Some(raw) = self.pps_raw[i].clone() {
                            self.pps[i] =
                                parse_pps(&raw, |sid| self.sps[sid as usize].clone()).ok();
                        }
                    }
                }
                Ok(())
            }
            Some(8) => {
                let p = parse_pps(nal, |sid| self.sps[sid as usize].clone())?;
                let id = p.pps_id as usize;
                self.pps_raw[id] = Some(nal.clone());
                self.pps[id] = Some(p);
                Ok(())
            }
            _ => Err(CodecError::invalid_input("not an H.264 parameter set")),
        }
    }

    /// 1 AU（NAL の列）を入れる。
    pub fn decode_au(
        &mut self,
        nals: &[Bytes],
        pts: i64,
        keyframe_hint: bool,
    ) -> Result<Vec<Action>> {
        let mut slices: Vec<Bytes> = Vec::new();
        for n in nals {
            match n.first().map(|b| b & 0x1f) {
                Some(7) | Some(8) => self.add_parameter_set(n)?,
                Some(1) | Some(5) => {
                    slices.push(n.clone());
                    if slices.len() > MAX_SLICES {
                        return Err(CodecError::invalid_input(
                            "too many H.264 slices in one access unit",
                        ));
                    }
                }
                Some(2..=4) => {
                    return Err(CodecError::unsupported(
                        "H.264 data partitioning is not supported",
                    ));
                }
                Some(20) | Some(21) => {
                    return Err(CodecError::unsupported(
                        "H.264 SVC/MVC extensions are not supported",
                    ));
                }
                _ => {} // SEI・AUD・終端・フィラーなど
            }
        }
        if slices.is_empty() {
            return Ok(Vec::new());
        }
        let mut headers = Vec::with_capacity(slices.len());
        for s in &slices {
            let h = parse_slice_header(
                s,
                |id| self.pps[id as usize].as_ref(),
                |id| self.sps[id as usize].as_ref(),
            )?;
            headers.push(h);
        }
        // 冗長スライスは捨てる（主ピクチャーがあれば足りる）。
        let mut jobs: Vec<(Bytes, SliceHeader)> = slices
            .into_iter()
            .zip(headers)
            .filter(|(_, h)| h.redundant_pic_cnt == 0)
            .collect();
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let first = jobs[0].1.clone();
        for (_, h) in &jobs[1..] {
            if h.frame_num != first.frame_num
                || h.pps_id != first.pps_id
                || h.nal_unit_type != first.nal_unit_type
                || h.pic_order_cnt_lsb != first.pic_order_cnt_lsb
                || (h.nal_ref_idc == 0) != (first.nal_ref_idc == 0)
            {
                return Err(CodecError::invalid_input(
                    "H.264 access unit contains slices of different pictures",
                ));
            }
        }
        let pps = self.pps[first.pps_id as usize]
            .clone()
            .expect("checked by slice parser");
        let sps = self.sps[pps.sps_id as usize]
            .clone()
            .expect("checked by slice parser");

        let mut actions = Vec::new();
        let info = self.stream_info_for(&sps)?;
        if self.info.as_ref() != Some(&info) {
            if !first.is_idr() && self.info.is_some() {
                return Err(CodecError::invalid_input(
                    "H.264 stream parameters changed without an IDR picture",
                ));
            }
            // 前のストリームの残りを出し切ってから作り直す。
            self.bump_all(&mut actions);
            self.dpb.clear();
            self.free = (0..info.surfaces).rev().collect();
            self.dpb_size = info.dpb_size;
            self.reorder = sps.reorder_depth();
            self.info = Some(info.clone());
            self.max_long_term_frame_idx = None;
            actions.push(Action::Reinit(info));
        }
        self.reorder = sps.reorder_depth();
        if !self.started {
            // 最初は IDR（または全スライスが I のピクチャー）から始める。それまでは捨てる。
            let all_intra = jobs.iter().all(|(_, h)| h.slice_type == SLICE_I);
            if !first.is_idr() && !all_intra {
                return Ok(actions);
            }
            self.started = true;
            if !first.is_idr() {
                self.prev_ref_frame_num = first.frame_num;
                self.prev_frame_num = first.frame_num;
            }
        }

        let max_frame_num = sps.max_frame_num();
        if first.is_idr() {
            self.prev_ref_frame_num = 0;
        } else if first.frame_num != self.prev_ref_frame_num
            && first.frame_num != (self.prev_ref_frame_num + 1) % max_frame_num
            && sps.gaps_in_frame_num_allowed
        {
            // 許された欠落だけを補う。許されていない欠落（伝送の損失）は補わずに続ける（誤り隠し）。
            self.fill_frame_num_gap(&sps, first.frame_num, &mut actions);
        }

        let (top_poc, bottom_poc, frame_num_offset) = self.compute_poc(&sps, &first);

        // 復号先のサーフェスを確保する（足りなければ表示待ちを出して空ける）。
        let surface = loop {
            if let Some(s) = self.free.pop() {
                break s;
            }
            if !self.bump_one(&mut actions) {
                return Err(CodecError::internal("no free H.264 decode surface"));
            }
        };

        let cur_poc = top_poc.min(bottom_poc);
        let mut slice_jobs = Vec::with_capacity(jobs.len());
        for (nal, h) in jobs.drain(..) {
            let (l0, l1) = self.build_ref_lists(&sps, &h, cur_poc)?;
            slice_jobs.push(SliceJob {
                nal,
                header: h,
                list0: l0,
                list1: l1,
            });
        }
        let refs: Vec<RefPic> = self
            .dpb
            .iter()
            .filter(|f| f.reference != RefState::None)
            .take(16)
            .map(|f| self.ref_pic(f))
            .collect();
        let is_reference = first.nal_ref_idc != 0;
        actions.push(Action::Submit(Box::new(PictureJob {
            surface,
            sps: sps.clone(),
            pps,
            frame_num: first.frame_num,
            top_poc,
            bottom_poc,
            is_reference,
            is_idr: first.is_idr(),
            refs,
            slices: slice_jobs,
        })));

        // 参照の印付け（8.2.5）
        let mut cur = DpbFrame {
            surface: Some(surface),
            frame_num: first.frame_num,
            top_poc,
            bottom_poc,
            reference: RefState::None,
            output_needed: true,
            pts,
            keyframe: keyframe_hint || first.is_idr(),
        };
        let mut mmco5 = false;
        if is_reference {
            mmco5 = self.mark_references(&sps, &first, &mut cur);
        }

        // 次のピクチャーのための状態（8.2.1）
        if mmco5 {
            let temp = cur.top_poc.min(cur.bottom_poc);
            cur.top_poc -= temp;
            cur.bottom_poc -= temp;
            cur.frame_num = 0;
        }
        if is_reference {
            if mmco5 {
                self.prev_poc_msb = 0;
                self.prev_poc_lsb = cur.top_poc as i64;
                self.prev_ref_frame_num = 0;
            } else {
                let lsb = first.pic_order_cnt_lsb as i64;
                self.prev_poc_msb = top_poc as i64 - lsb;
                self.prev_poc_lsb = lsb;
                self.prev_ref_frame_num = first.frame_num;
            }
        }
        self.prev_frame_num = if mmco5 { 0 } else { first.frame_num };
        self.prev_frame_num_offset = if mmco5 { 0 } else { frame_num_offset };
        self.prev_had_mmco5 = mmco5;

        // DPB に入れて、表示順に出す（C.4.5.3）
        if first.is_idr() || mmco5 {
            // 前のピクチャーはすべて参照から外れているので、表示待ちを出し切る。
            self.bump_all(&mut actions);
        }
        self.remove_unused();
        while self.dpb.len() >= self.dpb_size {
            // C.4.5.2: 参照でない現在のピクチャーが、表示待ちのどれよりも先に表示されるなら、DPB に入れずにすぐ出す。
            if !is_reference {
                let min_waiting = self
                    .dpb
                    .iter()
                    .filter(|f| f.output_needed)
                    .map(|f| f.poc())
                    .min();
                if min_waiting.is_none_or(|m| cur.poc() < m) {
                    actions.push(Action::Output(OutputPic {
                        surface,
                        pts: cur.pts,
                        keyframe: cur.keyframe,
                        poc: cur.poc(),
                    }));
                    self.free.push(surface);
                    return Ok(actions);
                }
            }
            if !self.bump_one(&mut actions) {
                // 参照だけで埋まっている（ストリームの誤り）。最も古い短期参照を外して続ける。
                if let Some(i) = self.oldest_short_term(cur.frame_num, max_frame_num) {
                    self.dpb[i].reference = RefState::None;
                    self.remove_unused();
                } else {
                    return Err(CodecError::invalid_input("H.264 DPB overflow"));
                }
            }
        }
        self.dpb.push(cur);
        while self.dpb.iter().filter(|f| f.output_needed).count() > self.reorder {
            self.bump_one(&mut actions);
        }
        Ok(actions)
    }

    /// 入力の終わり。表示待ちをすべて出す。
    pub fn flush(&mut self) -> Vec<Action> {
        let mut a = Vec::new();
        self.bump_all(&mut a);
        a
    }

    fn stream_info_for(&self, sps: &Sps) -> Result<StreamInfo> {
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 {
            return Err(CodecError::unsupported(
                "only 8-bit 4:2:0 H.264 is supported by the VA-API decoder",
            ));
        }
        let (w, h) = sps.display_size();
        if w > self.max_width || h > self.max_height {
            return Err(CodecError::invalid_input(format!(
                "H.264 stream {w}x{h} exceeds the allowed resolution"
            )));
        }
        let dpb_size = sps.dpb_size();
        Ok(StreamInfo {
            profile_idc: sps.profile_idc,
            constraint_flags: sps.constraint_flags,
            coded_width: sps.coded_width(),
            coded_height: sps.coded_height(),
            crop: sps.crop,
            bit_depth: 8,
            chroma_format_idc: 1,
            dpb_size,
            surfaces: dpb_size + 2,
        })
    }

    /// (TopFieldOrderCnt, BottomFieldOrderCnt, FrameNumOffset)。
    fn compute_poc(&self, sps: &Sps, h: &SliceHeader) -> (i32, i32, i64) {
        let max_frame_num = sps.max_frame_num() as i64;
        let frame_num = h.frame_num as i64;
        let frame_num_offset = if h.is_idr() {
            0
        } else {
            let prev = if self.prev_had_mmco5 {
                0
            } else {
                self.prev_frame_num_offset
            };
            if (self.prev_frame_num as i64) > frame_num {
                prev + max_frame_num
            } else {
                prev
            }
        };
        let clamp = |v: i64| v.clamp(i32::MIN as i64 / 2, i32::MAX as i64 / 2) as i32;
        match sps.poc_type {
            0 => {
                let (prev_msb, prev_lsb) = if h.is_idr() {
                    (0, 0)
                } else {
                    (self.prev_poc_msb, self.prev_poc_lsb)
                };
                let max_lsb = 1i64 << sps.log2_max_poc_lsb;
                let lsb = h.pic_order_cnt_lsb as i64;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                let bottom = top + h.delta_pic_order_cnt_bottom as i64;
                (clamp(top), clamp(bottom), frame_num_offset)
            }
            1 => {
                let n = sps.offset_for_ref_frame.len() as i64;
                let mut abs = if n != 0 {
                    frame_num_offset + frame_num
                } else {
                    0
                };
                if h.nal_ref_idc == 0 && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i64;
                if abs > 0 {
                    let cycle = (abs - 1) / n;
                    let in_cycle = ((abs - 1) % n) as usize;
                    let delta_per_cycle: i64 =
                        sps.offset_for_ref_frame.iter().map(|v| *v as i64).sum();
                    expected = cycle * delta_per_cycle;
                    for v in &sps.offset_for_ref_frame[..=in_cycle] {
                        expected += *v as i64;
                    }
                }
                if h.nal_ref_idc == 0 {
                    expected += sps.offset_for_non_ref_pic as i64;
                }
                let top = expected + h.delta_pic_order_cnt[0] as i64;
                let bottom = top
                    + sps.offset_for_top_to_bottom_field as i64
                    + h.delta_pic_order_cnt[1] as i64;
                (clamp(top), clamp(bottom), frame_num_offset)
            }
            _ => {
                let t = if h.is_idr() {
                    0
                } else if h.nal_ref_idc == 0 {
                    2 * (frame_num_offset + frame_num) - 1
                } else {
                    2 * (frame_num_offset + frame_num)
                };
                (clamp(t), clamp(t), frame_num_offset)
            }
        }
    }

    /// FrameNumWrap（= フレームの PicNum）。
    fn pic_num(frame_num: u32, cur_frame_num: u32, max_frame_num: u32) -> i64 {
        if frame_num > cur_frame_num {
            frame_num as i64 - max_frame_num as i64
        } else {
            frame_num as i64
        }
    }

    fn ref_pic(&self, f: &DpbFrame) -> RefPic {
        let (long_term, frame_idx) = match f.reference {
            RefState::Long(i) => (true, i),
            _ => (false, f.frame_num),
        };
        RefPic {
            surface: f.surface,
            frame_idx,
            long_term,
            top_poc: f.top_poc,
            bottom_poc: f.bottom_poc,
        }
    }

    /// 参照リストの初期化と並べ替え（8.2.4）。値は DPB の番号ではなく [`RefPic`]。
    fn build_ref_lists(
        &self,
        sps: &Sps,
        h: &SliceHeader,
        cur_poc: i32,
    ) -> Result<(RefList, RefList)> {
        if h.slice_type == SLICE_I {
            return Ok((Vec::new(), Vec::new()));
        }
        let max_frame_num = sps.max_frame_num();
        let shorts: Vec<usize> = (0..self.dpb.len())
            .filter(|&i| self.dpb[i].reference == RefState::Short)
            .collect();
        let mut longs: Vec<usize> = (0..self.dpb.len())
            .filter(|&i| matches!(self.dpb[i].reference, RefState::Long(_)))
            .collect();
        let lt_idx = |i: usize| match self.dpb[i].reference {
            RefState::Long(n) => n,
            _ => u32::MAX,
        };
        longs.sort_by_key(|&i| lt_idx(i));
        let mut lists: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
        if h.slice_type == SLICE_P {
            let mut s = shorts.clone();
            s.sort_by_key(|&i| {
                std::cmp::Reverse(Self::pic_num(
                    self.dpb[i].frame_num,
                    h.frame_num,
                    max_frame_num,
                ))
            });
            lists[0] = s;
            lists[0].extend(&longs);
        } else {
            debug_assert_eq!(h.slice_type, SLICE_B);
            let mut before: Vec<usize> = shorts
                .iter()
                .copied()
                .filter(|&i| self.dpb[i].poc() < cur_poc)
                .collect();
            let mut after: Vec<usize> = shorts
                .iter()
                .copied()
                .filter(|&i| self.dpb[i].poc() > cur_poc)
                .collect();
            before.sort_by_key(|&i| std::cmp::Reverse(self.dpb[i].poc()));
            after.sort_by_key(|&i| self.dpb[i].poc());
            lists[0] = before
                .iter()
                .chain(after.iter())
                .chain(longs.iter())
                .copied()
                .collect();
            lists[1] = after
                .iter()
                .chain(before.iter())
                .chain(longs.iter())
                .copied()
                .collect();
            if lists[1].len() > 1 && lists[0] == lists[1] {
                lists[1].swap(0, 1);
            }
        }
        let counts = [
            h.num_ref_idx_l0_active_minus1 as usize + 1,
            h.num_ref_idx_l1_active_minus1 as usize + 1,
        ];
        let nlists = if h.slice_type == SLICE_B { 2 } else { 1 };
        let mut out: [RefList; 2] = [Vec::new(), Vec::new()];
        for l in 0..nlists {
            let n = counts[l];
            let mut list: Vec<Option<usize>> = lists[l].iter().take(n).map(|&i| Some(i)).collect();
            list.resize(n, None);
            // 並べ替え（8.2.4.3）
            let max_pic_num = max_frame_num as i64;
            let curr_pic_num = h.frame_num as i64;
            let mut pred = curr_pic_num;
            for (ref_idx, m) in h.ref_list_mods[l].iter().enumerate() {
                if ref_idx >= n {
                    return Err(CodecError::invalid_input(
                        "too many H.264 reference list modifications",
                    ));
                }
                let target = match m.idc {
                    0 | 1 => {
                        let abs_diff = m.value as i64 + 1;
                        if abs_diff > max_pic_num {
                            return Err(CodecError::invalid_input("bad H.264 abs_diff_pic_num"));
                        }
                        let mut no_wrap = if m.idc == 0 {
                            pred - abs_diff
                        } else {
                            pred + abs_diff
                        };
                        if no_wrap < 0 {
                            no_wrap += max_pic_num;
                        } else if no_wrap >= max_pic_num {
                            no_wrap -= max_pic_num;
                        }
                        pred = no_wrap;
                        let pic_num = if no_wrap > curr_pic_num {
                            no_wrap - max_pic_num
                        } else {
                            no_wrap
                        };
                        shorts.iter().copied().find(|&i| {
                            Self::pic_num(self.dpb[i].frame_num, h.frame_num, max_frame_num)
                                == pic_num
                        })
                    }
                    _ => longs.iter().copied().find(|&i| lt_idx(i) == m.value),
                };
                // 見つからなければ「参照ピクチャー無し」を入れる（誤りのあるストリーム）。
                list.insert(ref_idx, target);
                let mut k = ref_idx + 1;
                for c in ref_idx + 1..list.len() {
                    if list[c] != target || target.is_none() {
                        list[k] = list[c];
                        k += 1;
                    }
                }
                list.truncate(n);
                list.resize(n, None);
            }
            out[l] = list
                .into_iter()
                .map(|o| o.map(|i| self.ref_pic(&self.dpb[i])))
                .collect();
        }
        let [l0, l1] = out;
        Ok((l0, l1))
    }

    /// 参照の印付け（8.2.5）。MMCO 5 があれば `true`。
    fn mark_references(&mut self, sps: &Sps, h: &SliceHeader, cur: &mut DpbFrame) -> bool {
        let max_frame_num = sps.max_frame_num();
        if h.is_idr() {
            for f in &mut self.dpb {
                f.reference = RefState::None;
            }
            if h.long_term_reference_flag {
                cur.reference = RefState::Long(0);
                self.max_long_term_frame_idx = Some(0);
            } else {
                cur.reference = RefState::Short;
                self.max_long_term_frame_idx = None;
            }
            return false;
        }
        let mut mmco5 = false;
        let mut cur_long = None;
        match &h.mmco {
            Some(ops) => {
                let curr_pic_num = h.frame_num as i64;
                for op in ops {
                    match *op {
                        Mmco::UnrefShort {
                            difference_of_pic_nums_minus1,
                        } => {
                            let x = curr_pic_num - (difference_of_pic_nums_minus1 as i64 + 1);
                            for f in &mut self.dpb {
                                if f.reference == RefState::Short
                                    && Self::pic_num(f.frame_num, h.frame_num, max_frame_num) == x
                                {
                                    f.reference = RefState::None;
                                }
                            }
                        }
                        Mmco::UnrefLong { long_term_pic_num } => {
                            for f in &mut self.dpb {
                                if f.reference == RefState::Long(long_term_pic_num) {
                                    f.reference = RefState::None;
                                }
                            }
                        }
                        Mmco::ShortToLong {
                            difference_of_pic_nums_minus1,
                            long_term_frame_idx,
                        } => {
                            let x = curr_pic_num - (difference_of_pic_nums_minus1 as i64 + 1);
                            for f in &mut self.dpb {
                                if f.reference == RefState::Long(long_term_frame_idx) {
                                    f.reference = RefState::None;
                                }
                            }
                            for f in &mut self.dpb {
                                if f.reference == RefState::Short
                                    && Self::pic_num(f.frame_num, h.frame_num, max_frame_num) == x
                                {
                                    f.reference = RefState::Long(long_term_frame_idx);
                                }
                            }
                        }
                        Mmco::MaxLongTermIdx {
                            max_long_term_frame_idx_plus1,
                        } => {
                            self.max_long_term_frame_idx =
                                max_long_term_frame_idx_plus1.checked_sub(1);
                            for f in &mut self.dpb {
                                if let RefState::Long(i) = f.reference
                                    && self.max_long_term_frame_idx.is_none_or(|m| i > m)
                                {
                                    f.reference = RefState::None;
                                }
                            }
                        }
                        Mmco::UnrefAll => {
                            for f in &mut self.dpb {
                                f.reference = RefState::None;
                            }
                            self.max_long_term_frame_idx = None;
                            mmco5 = true;
                        }
                        Mmco::CurrentToLong {
                            long_term_frame_idx,
                        } => {
                            for f in &mut self.dpb {
                                if f.reference == RefState::Long(long_term_frame_idx) {
                                    f.reference = RefState::None;
                                }
                            }
                            cur_long = Some(long_term_frame_idx);
                        }
                    }
                }
            }
            None => self.sliding_window(sps, h.frame_num),
        }
        cur.reference = match cur_long {
            Some(i) => RefState::Long(i),
            None => RefState::Short,
        };
        // 参照が max_num_ref_frames を超えたら（誤りのあるストリーム）古い短期参照を外す。
        let max_refs = sps.max_num_ref_frames.max(1) as usize;
        while self
            .dpb
            .iter()
            .filter(|f| f.reference != RefState::None)
            .count()
            + 1
            > max_refs
        {
            match self.oldest_short_term(h.frame_num, max_frame_num) {
                Some(i) => self.dpb[i].reference = RefState::None,
                None => break,
            }
        }
        mmco5
    }

    fn oldest_short_term(&self, cur_frame_num: u32, max_frame_num: u32) -> Option<usize> {
        (0..self.dpb.len())
            .filter(|&i| self.dpb[i].reference == RefState::Short)
            .min_by_key(|&i| Self::pic_num(self.dpb[i].frame_num, cur_frame_num, max_frame_num))
    }

    /// スライディングウィンドウ（8.2.5.3）。
    fn sliding_window(&mut self, sps: &Sps, cur_frame_num: u32) {
        let max_refs = sps.max_num_ref_frames.max(1) as usize;
        while self
            .dpb
            .iter()
            .filter(|f| f.reference != RefState::None)
            .count()
            >= max_refs
        {
            match self.oldest_short_term(cur_frame_num, sps.max_frame_num()) {
                Some(i) => self.dpb[i].reference = RefState::None,
                None => break,
            }
        }
    }

    /// frame_num の欠落を「存在しない」フレームで補う（8.2.5.2）。
    fn fill_frame_num_gap(&mut self, sps: &Sps, frame_num: u32, actions: &mut Vec<Action>) {
        let max = sps.max_frame_num();
        let mut unused = (self.prev_ref_frame_num + 1) % max;
        let mut guard = 0;
        while unused != frame_num && guard < max {
            self.sliding_window(sps, unused);
            self.remove_unused();
            while self.dpb.len() >= self.dpb_size {
                if !self.bump_one(actions) {
                    if let Some(i) = self.oldest_short_term(unused, max) {
                        self.dpb[i].reference = RefState::None;
                        self.remove_unused();
                    } else {
                        break;
                    }
                }
            }
            self.dpb.push(DpbFrame {
                surface: None,
                frame_num: unused,
                top_poc: 0,
                bottom_poc: 0,
                reference: RefState::Short,
                output_needed: false,
                pts: 0,
                keyframe: false,
            });
            self.prev_ref_frame_num = unused;
            self.prev_frame_num = unused;
            unused = (unused + 1) % max;
            guard += 1;
        }
    }

    /// 参照でも表示待ちでもないフレームを外し、サーフェスを返す。
    fn remove_unused(&mut self) {
        let mut i = 0;
        while i < self.dpb.len() {
            let f = &self.dpb[i];
            if f.reference == RefState::None && !f.output_needed {
                if let Some(s) = f.surface {
                    self.free.push(s);
                }
                self.dpb.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// 表示待ちのうち POC が最小のものを 1 枚出す。出すものが無ければ `false`。
    fn bump_one(&mut self, actions: &mut Vec<Action>) -> bool {
        let Some(i) = (0..self.dpb.len())
            .filter(|&i| self.dpb[i].output_needed)
            .min_by_key(|&i| self.dpb[i].poc())
        else {
            return false;
        };
        let f = &mut self.dpb[i];
        f.output_needed = false;
        if let Some(s) = f.surface {
            actions.push(Action::Output(OutputPic {
                surface: s,
                pts: f.pts,
                keyframe: f.keyframe,
                poc: f.poc(),
            }));
        }
        self.remove_unused();
        true
    }

    fn bump_all(&mut self, actions: &mut Vec<Action>) {
        while self.bump_one(actions) {}
        self.remove_unused();
    }
}

#[cfg(test)]
mod tests {
    use super::super::params::parse_sps;
    use super::super::slice::RefListMod;
    use super::*;

    fn sps() -> Sps {
        // Main、11x9 MB、log2_max_frame_num=4、POC 型 2、参照 4 枚
        let mut w = senca_core::bits::BitWriter::new();
        w.write_bits(77, 8);
        w.write_bits(0, 8);
        w.write_bits(30, 8);
        w.write_ue(0);
        w.write_ue(0);
        w.write_ue(2);
        w.write_ue(4);
        w.write_bit(false);
        w.write_ue(10);
        w.write_ue(8);
        w.write_bit(true);
        w.write_bit(true);
        w.write_bit(false);
        w.write_bit(false);
        w.write_bit(true);
        let mut v = vec![0x67];
        v.extend(w.finish());
        parse_sps(&v).unwrap()
    }

    fn header(frame_num: u32, slice_type: u8) -> SliceHeader {
        SliceHeader {
            nal_ref_idc: 1,
            nal_unit_type: 1,
            first_mb_in_slice: 0,
            slice_type,
            pps_id: 0,
            frame_num,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            redundant_pic_cnt: 0,
            direct_spatial_mv_pred: false,
            num_ref_idx_l0_active_minus1: 3,
            num_ref_idx_l1_active_minus1: 0,
            ref_list_mods: [Vec::new(), Vec::new()],
            pred_weight: None,
            no_output_of_prior_pics: false,
            long_term_reference_flag: false,
            mmco: None,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            header_bits: 0,
        }
    }

    fn frame(surface: usize, frame_num: u32, reference: RefState) -> DpbFrame {
        DpbFrame {
            surface: Some(surface),
            frame_num,
            top_poc: 2 * frame_num as i32,
            bottom_poc: 2 * frame_num as i32,
            reference,
            output_needed: false,
            pts: 0,
            keyframe: false,
        }
    }

    #[test]
    fn mmco_and_long_term_lists() {
        let sps = sps();
        let mut fe = H264Frontend::new(1920, 1080);
        fe.dpb = vec![
            frame(0, 1, RefState::Short),
            frame(1, 2, RefState::Short),
            frame(2, 3, RefState::Short),
        ];
        // 現在 frame_num=4: MMCO3（picNum 3 → 長期 0）、MMCO1（picNum 1 を外す）
        let mut h = header(4, SLICE_P);
        h.mmco = Some(vec![
            Mmco::ShortToLong {
                difference_of_pic_nums_minus1: 0,
                long_term_frame_idx: 0,
            },
            Mmco::UnrefShort {
                difference_of_pic_nums_minus1: 2,
            },
        ]);
        fe.max_long_term_frame_idx = Some(1);
        let mut cur = frame(3, 4, RefState::None);
        assert!(!fe.mark_references(&sps, &h, &mut cur));
        assert_eq!(fe.dpb[0].reference, RefState::None);
        assert_eq!(fe.dpb[1].reference, RefState::Short);
        assert_eq!(fe.dpb[2].reference, RefState::Long(0));
        assert_eq!(cur.reference, RefState::Short);
        fe.dpb.push(cur);
        // 次の P（frame_num=5）: 短期を PicNum の降順（4, 2）、次に長期（0）
        let h = header(5, SLICE_P);
        let (l0, _) = fe.build_ref_lists(&sps, &h, 10).unwrap();
        let ids: Vec<_> = l0
            .iter()
            .map(|r| r.map(|r| (r.frame_idx, r.long_term)))
            .collect();
        assert_eq!(
            ids,
            vec![Some((4, false)), Some((2, false)), Some((0, true)), None]
        );
        // 並べ替え: 長期 0 を先頭へ、次に picNum 2（pred 5 → 5-3=2）
        let mut h = header(5, SLICE_P);
        h.ref_list_mods[0] = vec![
            RefListMod { idc: 2, value: 0 },
            RefListMod { idc: 0, value: 2 },
        ];
        let (l0, _) = fe.build_ref_lists(&sps, &h, 10).unwrap();
        let ids: Vec<_> = l0
            .iter()
            .map(|r| r.map(|r| (r.frame_idx, r.long_term)))
            .collect();
        assert_eq!(
            ids,
            vec![Some((0, true)), Some((2, false)), Some((4, false)), None]
        );
        // MMCO5 ですべて外れる
        let mut h = header(5, SLICE_P);
        h.mmco = Some(vec![Mmco::UnrefAll]);
        let mut cur = frame(4, 5, RefState::None);
        assert!(fe.mark_references(&sps, &h, &mut cur));
        assert!(fe.dpb.iter().all(|f| f.reference == RefState::None));
        assert_eq!(fe.max_long_term_frame_idx, None);
    }

    #[test]
    fn sliding_window_removes_smallest_frame_num_wrap() {
        let sps = sps(); // max_num_ref_frames = 4
        let mut fe = H264Frontend::new(1920, 1080);
        // frame_num が折り返している: 14, 15, 0, 1（現在 2）→ 14 が最も古い
        fe.dpb = vec![
            frame(0, 0, RefState::Short),
            frame(1, 14, RefState::Short),
            frame(2, 1, RefState::Short),
            frame(3, 15, RefState::Short),
        ];
        let h = header(2, SLICE_P);
        let mut cur = frame(4, 2, RefState::None);
        fe.mark_references(&sps, &h, &mut cur);
        assert_eq!(fe.dpb[1].reference, RefState::None);
        assert_eq!(
            fe.dpb
                .iter()
                .filter(|f| f.reference == RefState::Short)
                .count(),
            3
        );
    }

    #[test]
    fn poc_type_0_wraps() {
        let mut w = senca_core::bits::BitWriter::new();
        w.write_bits(77, 8);
        w.write_bits(0, 8);
        w.write_bits(30, 8);
        w.write_ue(0);
        w.write_ue(0);
        w.write_ue(0); // POC 型 0
        w.write_ue(0); // log2_max_poc_lsb = 4（16）
        w.write_ue(1);
        w.write_bit(false);
        w.write_ue(0);
        w.write_ue(0);
        w.write_bit(true);
        w.write_bit(true);
        w.write_bit(false);
        w.write_bit(false);
        w.write_bit(true);
        let mut v = vec![0x67];
        v.extend(w.finish());
        let sps = parse_sps(&v).unwrap();
        let mut fe = H264Frontend::new(1920, 1080);
        fe.prev_poc_msb = 0;
        fe.prev_poc_lsb = 14;
        let mut h = header(1, SLICE_P);
        h.pic_order_cnt_lsb = 2;
        assert_eq!(fe.compute_poc(&sps, &h).0, 18);
        fe.prev_poc_msb = 16;
        fe.prev_poc_lsb = 2;
        h.pic_order_cnt_lsb = 14;
        assert_eq!(fe.compute_poc(&sps, &h).0, 14);
    }
}
