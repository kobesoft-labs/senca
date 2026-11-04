// ノードとデバイス: ノードごとのカード（役割・スケジューラー・状態・予約と実測の量・配信・ワークロード）と drain・解除、
// 装置（GPU・コーデックのプロバイダー・キャプチャ機器）と使えない理由・直し方、変換できるコーデックの表。node:admin が要る。
//
// 量はグリッド（GET /grid）の capacity・reserved・usage（usage と last_heartbeat はスケジューラーのノードだけが返す）。
// コーデックのプロバイダーとキャプチャ機器は、応答したノード（この管理画面を開いたノード）の分だけ（GET /devices）。

import { h, replace } from '../lib/dom.js';
import { confirmDialog, reportError, toast, errorBox, copyField } from '../lib/ui.js';
import { system } from '../api/resources.js';
import { grid as gridApi } from '../api/ops.js';
import { fmtTime, fmtRelative, fmtNumber, fmtPercent } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { listNav } from '../lib/listnav.js';
import { t, has } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { guideUrl } from '../lib/guide.js';
import { button, pageHeader, panel, statusPill, badge, alert, helpHint, meter, skeleton, emptyState, tooltip } from '../ui/index.js';

/** GPU のプロバイダー（無いのが普通なので、使えなくても赤にしない）。 */
export const HW_PROVIDERS = ['nvidia', 'intel-vpl', 'amd-amf', 'vaapi'];
const SW_PROVIDERS = ['dav1d', 'svt-av1', 'libvpx', 'opus'];

async function settle(p) {
  try { return { ok: true, value: await p }; } catch (e) { return { ok: false, error: e }; }
}

const ratioKind = (r) => (r >= 0.95 ? 'danger' : r >= 0.8 ? 'warn' : undefined);

/** 予約と容量のメーター（実測があれば下に添える）。 */
function loadMeter(label, reserved, capacity, used, fmt) {
  const r = capacity > 0 ? reserved / capacity : 0;
  const m = meter({ label, value: reserved, max: capacity || 1, kind: ratioKind(r), valueText: t('nodes.meter.reserved', { used: fmt(reserved), cap: fmt(capacity) }) });
  if (used !== null && used !== undefined) {
    m.append(h('span', { class: 'meter-foot' }, t('nodes.meter.actual', { v: fmt(used), pct: fmtPercent(capacity ? used / capacity : 0) })));
  }
  return m;
}
const fmtCpu = (m) => t('nodes.unit.cores', { n: fmtNumber(m / 1000, { maximumFractionDigits: 1 }) });
const fmtMem = (mb) => (mb >= 1024 ? `${fmtNumber(mb / 1024, { maximumFractionDigits: 1 })} GB` : `${fmtNumber(mb)} MB`);
const fmtNet = (mbps) => (mbps >= 1000 ? `${fmtNumber(mbps / 1000, { maximumFractionDigits: 1 })} Gbps` : `${fmtNumber(mbps)} Mbps`);

/** プロバイダーの名前と、使えないときの直し方（知らないものは null）。 */
function providerInfo(p) {
  const k = p.provider;
  return {
    title: has(`nodes.provider.${k}`) ? t(`nodes.provider.${k}`) : k,
    hw: HW_PROVIDERS.includes(k),
    fix: has(`nodes.fix.${k}`) ? t(`nodes.fix.${k}`) : null,
    guide: HW_PROVIDERS.includes(k) ? '16-containers.md' : '14-codecs-and-patents.md',
  };
}

export async function render(root, _p, { caps }) {
  root = root.appendChild(h('div', { class: 'ops-view' }));
  const summaryBox = h('div');
  const nodesBox = h('div', null, skeleton({ kind: 'cards', rows: 3 }));
  const gpuBox = h('div', null, skeleton({ kind: 'table', rows: 2 }));
  const provBox = h('div', null, skeleton({ kind: 'table', rows: 4 }));
  const capBox = h('div', null, skeleton({ kind: 'table', rows: 2 }));
  const codecBox = h('div');
  let disposed = false;

  const nodeLabel = (n) => n.name || n.id.slice(0, 8);

  // ---------------------------------------------------------------- drain・解除
  const drain = async (n, single) => {
    const ok = await confirmDialog(t('nodes.drainTitle', { name: nodeLabel(n) }), t('nodes.drainBody'), {
      ok: t('nodes.drain'), danger: false, icon: 'pause',
      consequences: [t('nodes.drainC1'), t('nodes.drainC2'), t('nodes.drainC3'), t('nodes.drainC4'), single ? t('nodes.drainSingle') : null].filter(Boolean),
    });
    if (!ok) return;
    try {
      await system.drain(n.id);
      toast(t('nodes.drained', { name: nodeLabel(n) }), 'ok');
      refresh();
    } catch (e) {
      reportError(e, t('nodes.drain'));
    }
  };
  const undrain = async (n) => {
    const ok = await confirmDialog(t('nodes.undrainTitle'), t('nodes.undrainBody', { name: nodeLabel(n) }), { ok: t('nodes.undrainOk'), danger: false, icon: 'play', consequences: [t('nodes.undrainC1')] });
    if (!ok) return;
    try {
      await system.undrain(n.id);
      toast(t('nodes.undrained', { name: nodeLabel(n) }), 'ok');
      refresh();
    } catch (e) {
      reportError(e, t('nodes.undrainTitle'));
    }
  };

  // ---------------------------------------------------------------- 装置の無効化・有効化
  const setDevice = async (node, dev, enabled) => {
    if (!enabled) {
      const ok = await confirmDialog(t('nodes.disableTitle', { id: dev }), t('nodes.disableBody', { node: nodeLabel(node) }), {
        ok: t('nodes.disable'), danger: false, icon: 'power', consequences: [t('nodes.disableC1'), t('nodes.disableC2'), t('nodes.disableC3')],
      });
      if (!ok) return;
    }
    try {
      await gridApi.setDevice(node.id, dev, enabled);
      toast(t(enabled ? 'nodes.enabled' : 'nodes.disabled', { id: dev }), 'ok');
      refresh();
    } catch (e) {
      reportError(e, t(enabled ? 'nodes.enable' : 'nodes.disable'));
    }
  };

  // ---------------------------------------------------------------- ノードのカード
  const nodeCard = (n, g, scheduler, single) => {
    const draining = (g ? g.draining : n.draining);
    const up = g ? g.up : true;
    const labels = Object.entries((g && g.labels) || n.labels || {}).filter(([k]) => !k.startsWith('senca.io/'));
    const roleTag = (r) => {
      const el = h('span', { class: 'tag' }, t(`nodes.role.${r}`));
      tooltip(el, t(`nodes.roleHelp.${r}`));
      el.tabIndex = 0;
      return el;
    };
    const stateBadge = !up ? badge(t('nodes.state.down'), 'danger', { dot: true }) : draining ? statusPill('draining') : statusPill('accepting');
    const isLeader = scheduler && scheduler === n.id;
    return h('article', { class: ['node-card', 'card', !up && 'is-down', draining && 'is-draining'].filter(Boolean).join(' '), 'data-node': n.id, 'data-testid': 'node-card', 'data-nav-item': '', tabindex: '-1', 'aria-labelledby': `node-${n.id}` },
      h('header', { class: 'node-head' },
        h('span', { class: 'node-icon' }, icon('nodes')),
        h('div', { class: 'node-title' },
          h('h3', { id: `node-${n.id}` }, nodeLabel(n)),
          h('div', { class: 'row node-tags' },
            n.self ? h('span', { class: 'tag accent-tag' }, t('nodes.thisNode')) : null,
            isLeader ? badge(t('nodes.leader'), 'accent', { icon: 'crown', title: t('nodes.leaderHelp') }) : null,
            ...(g ? g.roles.map(roleTag) : []))),
        stateBadge),
      g ? h('div', { class: 'node-meters' },
        loadMeter(t('nodes.meter.cpu'), g.reserved.cpu_millis, g.capacity.cpu_millis, g.usage ? g.usage.cpu_millis : null, fmtCpu),
        loadMeter(t('nodes.meter.memory'), g.reserved.memory_mb, g.capacity.memory_mb, g.usage ? g.usage.memory_mb : null, fmtMem),
        loadMeter(t('nodes.meter.netOut'), g.reserved.net_out_mbps, g.capacity.net_out_mbps, g.usage ? g.usage.net_out_mbps : null, fmtNet),
        loadMeter(t('nodes.meter.netIn'), g.reserved.net_in_mbps, g.capacity.net_in_mbps, g.usage ? g.usage.net_in_mbps : null, fmtNet))
        : h('p', { class: 'muted small' }, t('nodes.noGrid')),
      h('dl', { class: 'node-facts' },
        h('div', null, h('dt', null, t('nodes.fact.live')), h('dd', { class: 'num' }, n.self ? fmtNumber(n.live_streams) : h('span', { class: 'muted', title: t('nodes.liveOtherHelp') }, '—'))),
        h('div', null, h('dt', null, t('nodes.fact.workloads')), h('dd', { class: 'num' }, g ? h('a', { href: `#/grid?node=${encodeURIComponent(n.id)}` }, fmtNumber(g.workloads)) : '—')),
        h('div', null, h('dt', null, t('nodes.fact.devices')), h('dd', { class: 'num' }, g ? `${g.gpus.length} GPU · ${g.capture.length}` : '—')),
        h('div', null, h('dt', null, t('nodes.fact.heartbeat')), h('dd', null, g && g.last_heartbeat ? h('span', { title: fmtTime(g.last_heartbeat) }, fmtRelative(g.last_heartbeat)) : h('span', { class: 'muted' }, '—'))),
        h('div', null, h('dt', null, t('nodes.fact.registered')), h('dd', null, n.registered_at ? h('span', { title: fmtTime(n.registered_at) }, fmtRelative(n.registered_at)) : '—'))),
      labels.length ? h('div', { class: 'row node-labels' }, labels.map(([k, v]) => h('code', { class: 'label-chip' }, `${k}=${v}`))) : null,
      h('div', { class: 'node-id' }, copyField(n.id, t('nodes.idLabel', { name: nodeLabel(n) }))),
      h('footer', { class: 'node-actions' },
        draining
          ? button(t('nodes.undrain'), { size: 'sm', icon: 'play', testid: 'node-undrain', onClick: () => undrain(n) })
          : button(t('nodes.drain'), { size: 'sm', icon: 'pause', testid: 'node-drain', attrs: { disabled: !up || null }, onClick: () => drain(n, single) }),
        h('a', { class: 'btn sm ghost', href: `#/grid?node=${encodeURIComponent(n.id)}` }, icon('grid'), t('nodes.showOnGrid'))));
  };

  const drawNodes = (nodes, gr) => {
    if (!nodes.ok) {
      replace(summaryBox);
      replace(nodesBox, nodes.error.status === 403 ? alert('warn', t('nodes.forbidden'), { title: t('nodes.forbiddenTitle') }) : errorBox(nodes.error));
      return;
    }
    const items = nodes.value.data.items;
    const g = gr.ok ? gr.value.data : null;
    const byId = new Map(g ? g.nodes.map((x) => [x.id, x]) : []);
    // グリッドにしか無いノード（停止して /nodes から消えたもの）も出す
    const extra = g ? g.nodes.filter((x) => !items.some((n) => n.id === x.id)).map((x) => ({ id: x.id, name: x.name, draining: x.draining, labels: x.labels, live_streams: 0, self: false, registered_at: null })) : [];
    const all = [...items, ...extra].sort((a, b) => (b.self - a.self) || String(a.name).localeCompare(String(b.name)));
    const single = all.length === 1;
    const up = g ? g.nodes.filter((x) => x.up).length : all.length;
    const leader = g && g.scheduler ? (byId.get(g.scheduler) || {}).name : null;
    replace(summaryBox, h('div', { class: 'ops-summary', 'data-testid': 'nodes-summary' },
      h('span', { class: 'ops-summary-main' }, icon(single ? 'nodes' : 'grid'), single ? t('nodes.summary.single') : t('nodes.summary.cluster', { n: all.length })),
      h('span', null, t('nodes.summary.up', { up, total: all.length })),
      leader ? h('span', null, icon('crown'), t('nodes.summary.leader', { name: leader })) : (!single && g ? h('span', { class: 'muted' }, t('nodes.summary.notLeader')) : null),
      helpHint({ title: t('nodes.summary.helpTitle'), body: t('nodes.summary.help'), guide: '09-cluster-grid.md' })));
    replace(nodesBox, h('div', { class: 'node-grid' }, all.map((n) => nodeCard(n, byId.get(n.id) || null, g ? g.scheduler : null, single))));
  };

  // ---------------------------------------------------------------- GPU
  const drawGpus = (gr) => {
    if (!gr.ok) { replace(gpuBox, gr.error.status === 403 ? alert('info', t('nodes.forbidden'), { small: true }) : errorBox(gr.error)); return; }
    const rows = gr.value.data.nodes.flatMap((n) => n.gpus.map((gp) => ({ n, gp })));
    if (!rows.length) {
      replace(gpuBox, emptyState({
        compact: true, icon: 'gpu', title: t('nodes.gpu.none'), body: t('nodes.gpu.noneBody'), testid: 'gpu-empty',
        actions: [h('a', { class: 'btn sm', href: guideUrl('16-containers.md'), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('nodes.gpu.howTo'), icon('external'))],
      }));
      return;
    }
    replace(gpuBox, h('ul', { class: 'device-list' }, rows.map(({ n, gp }) => {
      const state = !gp.enabled ? statusPill('disabled') : !gp.healthy ? badge(t('nodes.gpu.unhealthy'), 'danger', { dot: true }) : statusPill('available');
      return h('li', { class: 'device-row', 'data-testid': 'gpu-row' },
        h('span', { class: 'device-icon' }, icon('gpu')),
        h('div', { class: 'device-main' },
          h('div', { class: 'row' }, h('strong', null, gp.id), h('span', { class: 'tag' }, gp.vendor), state),
          h('div', { class: 'small muted' }, t('nodes.gpu.on', { node: nodeLabel(n) }), !gp.healthy && gp.enabled ? ` · ${t('nodes.gpu.unhealthyBody')}` : '')),
        h('div', { class: 'device-meters' },
          meter({ label: t('nodes.gpu.encode'), value: gp.encode_reserved, max: gp.encode_sessions || 1, kind: ratioKind(gp.encode_reserved / (gp.encode_sessions || 1)), valueText: `${gp.encode_reserved} / ${gp.encode_sessions}` }),
          meter({ label: t('nodes.gpu.decode'), value: gp.decode_reserved, max: gp.decode_sessions || 1, kind: ratioKind(gp.decode_reserved / (gp.decode_sessions || 1)), valueText: `${gp.decode_reserved} / ${gp.decode_sessions}` })),
        gp.enabled
          ? button(t('nodes.disable'), { size: 'sm', icon: 'power', onClick: () => setDevice(n, gp.id, false) })
          : button(t('nodes.enable'), { size: 'sm', icon: 'power', onClick: () => setDevice(n, gp.id, true) }));
    })));
  };

  // ---------------------------------------------------------------- コーデックのプロバイダー（このノード）
  const providerRow = (p) => {
    const info = providerInfo(p);
    const state = p.available
      ? badge(t('nodes.prov.ready'), 'ok', { dot: true })
      : info.hw ? badge(t('nodes.prov.notDetected'), 'outline', { dot: true }) : badge(t('nodes.prov.missing'), 'warn', { dot: true });
    return h('li', { class: ['device-row', !p.available && 'is-off'].filter(Boolean).join(' '), 'data-provider': p.provider },
      h('span', { class: 'device-icon' }, icon(info.hw ? 'gpu' : 'cpu')),
      h('div', { class: 'device-main' },
        h('div', { class: 'row' }, h('strong', null, info.title), h('code', { class: 'muted' }, p.provider), state),
        p.available
          ? h('div', { class: 'small muted break' }, p.detail)
          : h('div', { class: 'device-why' },
            info.fix ? h('p', { class: 'small' }, info.fix) : null,
            h('details', { class: 'small muted' }, h('summary', null, t('nodes.prov.technical')), h('code', { class: 'break' }, p.detail)))),
      !p.available && info.fix ? h('a', { class: 'btn sm ghost', href: guideUrl(info.guide), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('nodes.prov.guide')) : h('span'));
  };

  const drawProviders = (dev, caps0) => {
    if (!dev.ok) { replace(provBox, dev.error.status === 403 ? alert('info', t('nodes.forbidden'), { small: true }) : errorBox(dev.error)); replace(capBox); return; }
    const me = dev.value.data.nodes[0];
    if (!me) { replace(provBox, h('p', { class: 'muted' }, t('common.none'))); return; }
    const provs = me.devices.providers;
    if (!provs.length) {
      replace(provBox, emptyState({
        compact: true, icon: 'cpu', title: t('nodes.noWorker'), body: t('nodes.noWorkerBody'),
        actions: [h('a', { class: 'btn sm', href: guideUrl('14-codecs-and-patents.md'), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('nodes.prov.guide'), icon('external'))],
      }));
    } else {
      const hw = provs.filter((p) => HW_PROVIDERS.includes(p.provider) || !SW_PROVIDERS.includes(p.provider));
      const sw = provs.filter((p) => SW_PROVIDERS.includes(p.provider));
      const hwReady = hw.some((p) => p.available);
      replace(provBox,
        h('h3', { class: 'sub-head' }, t('nodes.prov.hardware'), h('span', { class: 'muted small' }, hwReady ? t('nodes.prov.hardwareReady') : t('nodes.prov.hardwareNone'))),
        hwReady
          ? h('ul', { class: 'device-list' }, hw.map(providerRow))
          : h('details', { class: 'hw-none', 'data-testid': 'hw-none' },
            h('summary', null, icon('gpu'), t('nodes.prov.hwChecked', { n: hw.length, list: hw.map((p) => providerInfo(p).title).join(' · ') })),
            h('ul', { class: 'device-list' }, hw.map(providerRow))),
        h('h3', { class: 'sub-head' }, t('nodes.prov.software'), h('span', { class: 'muted small' }, t('nodes.prov.softwareNote'))),
        h('ul', { class: 'device-list' }, sw.map(providerRow)),
        me.devices.sandbox ? h('details', { class: 'small muted sandbox' }, h('summary', null, t('nodes.sandbox')), h('code', { class: 'break' }, me.devices.sandbox)) : null);
    }
    // キャプチャ機器
    const cap = me.capture;
    const gNode = caps0 && caps0.nodes ? caps0.nodes.find((x) => x.id === me.node) : null;
    if (cap === null || cap === undefined) {
      replace(capBox, emptyState({ compact: true, icon: 'camera', title: t('nodes.capture.none'), body: t('nodes.capture.noneBody') }));
    } else if (cap.available === false) {
      replace(capBox, alert('warn', cap.reason, { title: t('nodes.capture.failed'), small: true }));
    } else {
      const devs = cap.devices || [];
      const backends = cap.backends || [];
      replace(capBox,
        devs.length
          ? h('ul', { class: 'device-list' }, devs.map((d) => {
            const gd = gNode ? gNode.capture.find((x) => x.id === d.id) : null;
            const disabled = gd && gd.disabled;
            return h('li', { class: 'device-row', 'data-testid': 'capture-row' },
              h('span', { class: 'device-icon' }, icon('camera')),
              h('div', { class: 'device-main' },
                h('div', { class: 'row' }, h('strong', null, d.name || d.label || d.id), h('code', { class: 'muted' }, d.id), disabled ? statusPill('disabled') : gd && !gd.healthy ? badge(t('nodes.gpu.unhealthy'), 'danger', { dot: true }) : statusPill('available')),
                h('div', { class: 'small muted' }, t('nodes.capture.useAs', { url: `device:${d.id}` }))),
              h('span'),
              gd ? button(disabled ? t('nodes.enable') : t('nodes.disable'), { size: 'sm', icon: 'power', onClick: () => setDevice(gNode, d.id, !!disabled) }) : h('span'));
          }))
          : h('p', { class: 'muted small' }, t('nodes.capture.noDevices')),
        backends.length ? h('div', { class: 'backend-list' }, h('h3', { class: 'sub-head' }, t('nodes.capture.backends')),
          h('ul', { class: 'device-list compact' }, backends.map((b) => h('li', { class: ['device-row', !b.available && 'is-off'].filter(Boolean).join(' ') },
            h('span', { class: 'device-icon' }, icon(b.available ? 'success' : 'circle')),
            h('div', { class: 'device-main' },
              h('div', { class: 'row' }, h('strong', null, has(`nodes.backend.${b.backend}`) ? t(`nodes.backend.${b.backend}`) : b.backend), b.available ? badge(t('nodes.prov.ready'), 'ok', { dot: true }) : badge(t('nodes.prov.notDetected'), 'outline', { dot: true })),
              h('div', { class: 'small muted break' }, b.detail)), h('span'), h('span'))))) : null);
    }
  };

  // ---------------------------------------------------------------- 変換できるコーデック（このノード）
  const drawCodecs = () => {
    const tc = caps ? caps.codecs.transcode : [];
    if (!tc.length) {
      replace(codecBox, emptyState({ compact: true, icon: 'zap', title: t('nodes.noCaps'), body: caps ? t('nodes.passthroughOnly', { list: caps.codecs.passthrough.join(', ') }) : '' }));
      return;
    }
    const codecs = [...new Set(tc.map((x) => x.codec))].sort();
    const cell = (codec, dir) => {
      const xs = tc.filter((x) => x.codec === codec && x.direction === dir);
      if (!xs.length) return h('td', { 'data-label': t(dir === 'encode' ? 'nodes.encode' : 'nodes.decode') }, h('div', { class: 'cell' }, h('span', { class: 'muted' }, '—')));
      return h('td', { 'data-label': t(dir === 'encode' ? 'nodes.encode' : 'nodes.decode') }, h('div', { class: 'cell row' }, xs.map((x) => {
        const chip = h('span', { class: ['cap-chip', x.hardware && 'hw'].filter(Boolean).join(' '), tabindex: '0' }, x.provider, x.hardware ? badge('GPU', 'ok') : null);
        tooltip(chip, [x.device, x.max_width ? `${x.max_width}×${x.max_height}` : null, x.max_sessions ? t('nodes.sessions', { n: x.max_sessions }) : null].filter(Boolean).join(' · '));
        return chip;
      })));
    };
    replace(codecBox, h('div', { class: 'table-wrap' }, h('table', { class: 'codec-matrix', 'data-testid': 'codec-matrix' },
      h('caption', { class: 'visually-hidden' }, t('nodes.caps')),
      h('thead', null, h('tr', null, h('th', { scope: 'col' }, t('nodes.col.codec')), h('th', { scope: 'col' }, t('nodes.decode')), h('th', { scope: 'col' }, t('nodes.encode')))),
      h('tbody', null, codecs.map((c) => h('tr', null, h('td', { class: 'cell-primary', 'data-label': '' }, h('div', { class: 'cell' }, h('strong', null, c.toUpperCase()))), cell(c, 'decode'), cell(c, 'encode')))))),
    h('p', { class: 'small muted mt-2' }, t('nodes.passthroughNote', { list: caps.codecs.passthrough.join(', ') })));
  };

  let seq = 0;
  const refresh = async () => {
    const my = ++seq;
    const [nodes, gr, dev] = await Promise.all([settle(system.nodes()), settle(gridApi.get()), settle(system.devices())]);
    if (my !== seq || disposed) return;
    drawNodes(nodes, gr);
    drawGpus(gr);
    drawProviders(dev, gr.ok ? gr.value.data : null);
  };

  const cluster = !!(caps && caps.cluster);
  root.append(
    pageHeader({
      title: t('nav.nodes'),
      description: t('nodes.lead'),
      help: helpHint({ title: t('nodes.drain'), body: t('nodes.drainHelp'), guide: '13-operations.md' }),
      actions: [h('a', { class: 'btn', href: '#/grid' }, icon('grid'), t('nodes.toGrid')), button(t('common.refresh'), { iconOnly: true, icon: 'refresh', onClick: () => refresh() })],
    }),
    summaryBox,
    nodesBox,
    panel({ title: t('nodes.gpus'), icon: 'gpu', description: t('nodes.gpusDesc'), testid: 'nodes-gpus' }, gpuBox),
    panel({ title: t('nodes.providers'), icon: 'cpu', description: cluster ? t('nodes.providersDescCluster') : t('nodes.providersDesc'), testid: 'nodes-providers' }, provBox),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('nodes.capture'), icon: 'camera', description: t('nodes.captureDesc') }, capBox),
      panel({ title: t('nodes.caps'), icon: 'zap', description: t('nodes.capsDesc'), testid: 'nodes-codecs' }, codecBox)));
  drawCodecs();
  await refresh();
  const offNav = listNav(nodesBox);
  const offLive = refreshOn(['node.', 'device.', 'grid.'], refresh, { wait: 600 });
  return () => { disposed = true; offNav(); offLive(); };
}
