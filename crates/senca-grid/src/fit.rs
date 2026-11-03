//! 容量の勘定と静的な条件（配置係と `ApplyDecisions` の検査で共有する）。

use crate::model::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GpuUse {
    pub enc: u32,
    pub dec: u32,
    pub mem: u64,
}

/// 1 つのノードの予約の和。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub res: Resources,
    pub gpu: Vec<GpuUse>,
    pub capture: Vec<u32>,
}

/// 容量を超えた分を容量の千分率で（超えていれば最低 1）。
fn over(used: u64, cap: u64) -> u64 {
    if used <= cap { 0 } else { (used - cap).saturating_mul(1000) / cap.max(1) + 1 }
}

impl Usage {
    pub fn new(n: &NodeSpec) -> Self {
        Usage { res: Resources::default(), gpu: vec![GpuUse::default(); n.gpus.len()], capture: vec![0; n.capture.len()] }
    }

    pub fn add(&mut self, n: &NodeSpec, r: &Requirements, gpu: Option<usize>) {
        self.res = self.res.plus(&r.resources);
        if let (Some(g), Some(gr)) = (gpu, &r.gpu)
            && let Some(u) = self.gpu.get_mut(g)
        {
            u.enc = u.enc.saturating_add(gr.encode);
            u.dec = u.dec.saturating_add(gr.decode);
            u.mem = u.mem.saturating_add(gr.memory_mb);
        }
        if let Some(c) = &r.capture
            && let Some(i) = n.capture_index(c)
        {
            self.capture[i] += 1;
        }
    }

    pub fn sub(&mut self, n: &NodeSpec, r: &Requirements, gpu: Option<usize>) {
        self.res = self.res.minus(&r.resources);
        if let (Some(g), Some(gr)) = (gpu, &r.gpu)
            && let Some(u) = self.gpu.get_mut(g)
        {
            u.enc = u.enc.saturating_sub(gr.encode);
            u.dec = u.dec.saturating_sub(gr.decode);
            u.mem = u.mem.saturating_sub(gr.memory_mb);
        }
        if let Some(c) = &r.capture
            && let Some(i) = n.capture_index(c)
        {
            self.capture[i] = self.capture[i].saturating_sub(1);
        }
    }

    /// `r` を（装置 `gpu` で）足しても収まるか。収まらなければ最初に足りない次元。
    pub fn fits(&self, n: &NodeSpec, r: &Requirements, gpu: Option<usize>) -> Result<(), Reject> {
        let (u, q, cap) = (&self.res, &r.resources, &n.capacity);
        if u.cpu_millis.saturating_add(q.cpu_millis) > cap.cpu_millis {
            return Err(Reject::Cpu);
        }
        if u.memory_mb.saturating_add(q.memory_mb) > cap.memory_mb {
            return Err(Reject::Memory);
        }
        if u.net_in_mbps.saturating_add(q.net_in_mbps) > cap.net_in_mbps {
            return Err(Reject::NetIn);
        }
        if u.net_out_mbps.saturating_add(q.net_out_mbps) > cap.net_out_mbps {
            return Err(Reject::NetOut);
        }
        if let Some(gr) = &r.gpu {
            let Some(g) = gpu else { return Err(Reject::NoDevice) };
            let (d, u) = (&n.gpus[g], self.gpu[g]);
            if u.enc.saturating_add(gr.encode) > d.encode_sessions {
                return Err(Reject::GpuEncode);
            }
            if u.dec.saturating_add(gr.decode) > d.decode_sessions {
                return Err(Reject::GpuDecode);
            }
            if u.mem.saturating_add(gr.memory_mb) > d.memory_mb {
                return Err(Reject::GpuMemory);
            }
        }
        if let Some(c) = &r.capture {
            match n.capture_index(c) {
                Some(i) if n.capture[i].usable() && self.capture[i] == 0 => {}
                _ => return Err(Reject::Capture),
            }
        }
        Ok(())
    }

    /// 足りない量（0 なら収まる）。`need` が無ければ今の予約が容量を超えている量（すべての装置）。
    pub fn deficit(&self, n: &NodeSpec, need: Option<(&Requirements, Option<usize>)>) -> u64 {
        let zero = Resources::default();
        let q = need.map_or(&zero, |(r, _)| &r.resources);
        let (u, cap) = (&self.res, &n.capacity);
        let mut d = over(u.cpu_millis.saturating_add(q.cpu_millis), cap.cpu_millis)
            + over(u.memory_mb.saturating_add(q.memory_mb), cap.memory_mb)
            + over(u.net_in_mbps.saturating_add(q.net_in_mbps), cap.net_in_mbps)
            + over(u.net_out_mbps.saturating_add(q.net_out_mbps), cap.net_out_mbps);
        let gpu_over = |g: usize, e: u32, dc: u32, m: u64| {
            let (dev, u) = (&n.gpus[g], self.gpu[g]);
            over(u64::from(u.enc) + u64::from(e), u64::from(dev.encode_sessions))
                + over(u64::from(u.dec) + u64::from(dc), u64::from(dev.decode_sessions))
                + over(u.mem.saturating_add(m), dev.memory_mb)
        };
        match need {
            None => {
                for g in 0..n.gpus.len() {
                    d += gpu_over(g, 0, 0, 0);
                }
                for &c in &self.capture {
                    d += u64::from(c.saturating_sub(1)) * 1000;
                }
            }
            Some((r, gpu)) => {
                match (&r.gpu, gpu) {
                    (Some(gr), Some(g)) => d += gpu_over(g, gr.encode, gr.decode, gr.memory_mb),
                    (Some(_), None) => d += 1_000_000,
                    _ => {}
                }
                if let Some(c) = &r.capture {
                    match n.capture_index(c) {
                        Some(i) if n.capture[i].usable() => d += u64::from(self.capture[i]) * 1000,
                        // 機器が無い・壊れている: 何を空けても収まらない
                        _ => d += 1_000_000,
                    }
                }
            }
        }
        d
    }

    /// 資源（CPU・メモリ・帯域）の使用が容量の (1000 - permille)/1000 以下か。
    pub fn within_headroom(&self, n: &NodeSpec, permille: u32) -> bool {
        if permille == 0 {
            return true;
        }
        let keep = 1000u128.saturating_sub(u128::from(permille));
        let ok = |u: u64, c: u64| u128::from(u) * 1000 <= u128::from(c) * keep;
        let (u, c) = (&self.res, &n.capacity);
        ok(u.cpu_millis, c.cpu_millis)
            && ok(u.memory_mb, c.memory_mb)
            && ok(u.net_in_mbps, c.net_in_mbps)
            && ok(u.net_out_mbps, c.net_out_mbps)
    }

    /// 置いた後の空きの割合（千分率、資源と GPU の小さい方）。
    pub fn free_permille(&self, n: &NodeSpec, r: &Requirements, gpu: Option<usize>) -> i64 {
        let f = |u: u64, q: u64, c: u64| -> i64 {
            if c == 0 {
                return 1000;
            }
            let used = u.saturating_add(q).min(c);
            ((c - used) as u128 * 1000 / c as u128) as i64
        };
        let (u, q, c) = (&self.res, &r.resources, &n.capacity);
        let mut m = f(u.cpu_millis, q.cpu_millis, c.cpu_millis)
            .min(f(u.memory_mb, q.memory_mb, c.memory_mb))
            .min(f(u.net_in_mbps, q.net_in_mbps, c.net_in_mbps))
            .min(f(u.net_out_mbps, q.net_out_mbps, c.net_out_mbps));
        if let (Some(gr), Some(g)) = (&r.gpu, gpu) {
            let (d, u) = (&n.gpus[g], self.gpu[g]);
            m = m
                .min(f(u64::from(u.enc), u64::from(gr.encode), u64::from(d.encode_sessions)))
                .min(f(u64::from(u.dec), u64::from(gr.decode), u64::from(d.decode_sessions)));
        }
        m
    }
}

/// 手動の割り当ての対象の様子。`healthy` は生きている（ノードが up・media、指定の GPU が健康）、
/// `usable` はさらに排出中でない（新しく置ける）。pin が無ければ両方 true。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PinState {
    pub healthy: bool,
    pub usable: bool,
}

pub(crate) fn pin_state(nodes: &BTreeMap<NodeId, Node>, w: &Workload) -> PinState {
    let c = &w.constraints;
    let ok = |n: &Node| {
        n.status.up
            && n.spec.roles.contains(&Role::Media)
            && c.device.as_ref().is_none_or(|d| n.spec.gpus.iter().any(|g| &g.id == d && g.usable()))
    };
    match &c.node {
        Some(id) => match nodes.get(id) {
            Some(n) if ok(n) => PinState { healthy: true, usable: !n.spec.draining },
            _ => PinState::default(),
        },
        None if c.device.is_some() => {
            let mut s = PinState::default();
            for n in nodes.values().filter(|n| ok(n)) {
                s.healthy = true;
                if !n.spec.draining {
                    s.usable = true;
                    break;
                }
            }
            s
        }
        None => PinState { healthy: true, usable: true },
    }
}

/// 効いている GPU の指定。strict はいつも、prefer は対象が生きていて新しく置くときだけ
/// （prefer で他へ移った仕事は、そのまま認める。戻すのは任意の移動）。
pub(crate) fn dev_pin(w: &Workload, pin: PinState, existing: bool) -> Option<&str> {
    let d = w.constraints.device.as_deref()?;
    (w.constraints.pin_policy == PinPolicy::Strict || (pin.healthy && !existing)).then_some(d)
}

/// 容量と反親和性を除く条件。`existing` は今の割り当ての検査（排出中のノード・prefer の pin の外れを許す）。
pub(crate) fn static_check(node: &Node, w: &Workload, pin: PinState, existing: bool) -> Result<(), Reject> {
    if !node.status.up {
        return Err(Reject::NodeDown);
    }
    if !node.spec.roles.contains(&Role::Media) {
        return Err(Reject::NotMedia);
    }
    if !existing && node.spec.draining {
        return Err(Reject::Draining);
    }
    let c = &w.constraints;
    if w.is_pinned() {
        let strict = c.pin_policy == PinPolicy::Strict;
        if strict && !pin.healthy {
            return Err(Reject::PinTargetDown);
        }
        // 対象が生きている間は、手動の割り当ては縛る（prefer が緩むのは対象が止まったときだけ）
        let enforce = strict || (pin.healthy && !existing);
        if enforce && c.node.as_ref().is_some_and(|p| *p != node.spec.id) {
            return Err(Reject::PinnedElsewhere);
        }
        if let Some(d) = dev_pin(w, pin, existing)
            && node.spec.gpu_index(d).is_none()
        {
            return Err(Reject::NoDevice);
        }
    }
    if !c.labels.iter().all(|s| s.matches(&node.spec.labels)) {
        return Err(Reject::Labels);
    }
    Ok(())
}

/// GPU `gi` が要求に使えるか（健康・製造元・指定）。
pub(crate) fn gpu_ok(node: &NodeSpec, gi: usize, gr: &GpuReq, pin: Option<&str>) -> bool {
    let g = &node.gpus[gi];
    g.usable() && gr.vendor.as_ref().is_none_or(|v| v.eq_ignore_ascii_case(&g.vendor)) && pin.is_none_or(|p| p == g.id)
}
