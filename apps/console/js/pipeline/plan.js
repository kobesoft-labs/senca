// 計画（ノードの検証が返す Plan）と、検証の問題の見せ方。
//
// - 計画: どこで動くか、コーデックを CPU と GPU のどちらで動かすか、遅延、出力のトラック。部品は実行の順に並べる。
// - 問題: ノードの issues[].code ごとに、平らな言葉の説明と「どうすればよいか」を添える（CAPACITY_UNAVAILABLE など）。

import { h } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { badge, alert, emptyState, statTile } from '../ui/index.js';
import { fmtNumber } from '../lib/format.js';
import { KINDS } from './model.js';

const KNOWN_CODES = ['MALFORMED', 'INVALID_PARAM', 'DUPLICATE_ID', 'UNKNOWN_REFERENCE', 'TYPE_MISMATCH', 'CYCLE', 'UNCONSUMED_OUTPUT', 'POLICY_REJECTED', 'UNSUPPORTED_CODEC', 'CAPACITY_UNAVAILABLE', 'LIMIT_EXCEEDED', 'INVALID_CONFIG'];

/** 問題の code の説明と直し方（知らない code は null）。 */
export function issueHelp(code) {
  if (!KNOWN_CODES.includes(code)) return null;
  return { title: t(`pipelines.code.${code}`), fix: t(`pipelines.fix.${code}`) };
}

const MODE_KIND = { passthrough: 'ok', transcode: 'accent', process: 'info', terminal: 'outline' };
const modeLabel = (m) => (['passthrough', 'transcode', 'process', 'terminal'].includes(m) ? t(`pipelines.mode.${m}`) : m);

/**
 * 計画を見せる。
 * @param {object|null} plan
 * @param {{streamName: (id: string) => string, graphNodes?: object[], placement?: object|null, validatedAt?: number|null}} o
 */
export function planView(plan, { streamName, graphNodes = [], placement = null } = {}) {
  if (!plan) {
    return emptyState({ icon: 'list', title: t('pipelines.plan.noneTitle'), body: t('pipelines.plan.none'), compact: true });
  }
  const tot = plan.totals || {};
  const typeOf = (id) => (graphNodes.find((n) => n.id === id) || {}).type || (plan.nodes.find((n) => n.id === id) || {}).type;
  const where = placement && (placement.state === 'running' || placement.state === 'degraded')
    ? (placement.node_name || placement.node.slice(0, 8))
    : t('pipelines.plan.thisNode');
  const gpu = tot.gpu_codec_sessions || 0;
  const hwNodes = (plan.nodes || []).filter((n) => n.hardware === true);
  return h('div', { class: 'pl-plan', 'data-testid': 'plan' },
    h('div', { class: 'pl-plan-stats' },
      statTile({ label: t('pipelines.plan.where'), value: where, icon: 'nodes', sub: placement && placement.state === 'degraded' ? t('pipelines.plan.degraded', { label: placement.level_label }) : '' }),
      statTile({ label: t('pipelines.plan.codecs'), value: t('pipelines.plan.codecsValue', { cpu: tot.cpu_codec_sessions ?? 0, gpu }), icon: 'cpu', sub: gpu ? t('pipelines.plan.gpuSub') : t('pipelines.plan.cpuOnly') }),
      statTile({ label: t('pipelines.plan.latency'), value: `${fmtNumber(Math.round(tot.max_latency_ms || 0))} ms`, icon: 'clock', sub: t('pipelines.plan.latencySub') }),
      statTile({ label: t('pipelines.plan.load'), value: `${fmtNumber(Math.round((tot.pixels_per_sec || 0) / 1e6))} Mpx/s`, icon: 'gauge', sub: t('pipelines.plan.threads', { n: tot.threads ?? 0 }) })),
    gpu ? alert('info', t('pipelines.plan.gpuBody', { n: gpu, list: hwNodes.map((n) => `${n.id}${n.provider ? ` (${n.provider})` : ''}`).join(', ') || '—' }), { title: t('pipelines.plan.gpuTitle'), small: true }) : null,
    h('h3', { class: 'pl-plan-h' }, t('pipelines.plan.steps')),
    h('ol', { class: 'pl-plan-steps' }, (plan.order || plan.nodes.map((n) => n.id)).map((id) => {
      const n = plan.nodes.find((x) => x.id === id);
      if (!n) return null;
      const k = KINDS[n.type || typeOf(id)];
      return h('li', { class: ['pl-step', k && `k-${k.cat}`], 'data-plan-node': id },
        h('span', { class: 'pl-kind-chip sm' }, icon(k ? k.icon : 'circle')),
        h('div', { class: 'pl-step-main' },
          h('div', { class: 'pl-step-title' }, h('code', null, n.id), badge(modeLabel(n.mode), MODE_KIND[n.mode] || ''),
            n.codec ? h('span', { class: 'pl-step-codec' }, h('code', null, n.codec), n.hardware === true ? badge('GPU', 'warn', { icon: 'gpu' }) : n.hardware === false ? badge('CPU', 'outline', { icon: 'cpu' }) : null, n.provider ? h('span', { class: 'muted small' }, n.provider) : null) : null),
          (n.outputs && n.outputs.length) || (n.notes && n.notes.length) ? h('div', { class: 'pl-step-sub muted small' }, [...(n.outputs || []), ...(n.notes || [])].join(' · ')) : null),
        h('span', { class: 'pl-step-lat num muted small', title: t('pipelines.plan.addedLatency') }, `+${Math.round(n.added_latency_ms || 0)} ms`));
    })),
    h('h3', { class: 'pl-plan-h' }, t('pipelines.plan.outputsTitle')),
    (plan.sinks || []).length ? h('ul', { class: 'pl-plan-sinks' }, plan.sinks.map((sk) => h('li', null,
      h('div', { class: 'pl-sink-head' }, icon('live', { size: 'sm' }), h('code', null, sk.id), icon('arrow-right', { size: 'sm' }),
        h('a', { href: `#/streams/${encodeURIComponent(sk.stream)}` }, streamName(sk.stream))),
      h('ul', { class: 'pl-sink-tracks' }, sk.tracks.map((tk) => h('li', null,
        t('pipelines.plan.trackLine', { track: tk.track, kind: t(`pipelines.media.${tk.kind === 'audio' ? 'audio' : 'video'}`), codec: tk.codec.toUpperCase(), from: tk.from, mode: modeLabel(tk.mode), ms: Math.round(tk.latency_ms || 0) }))))))) : h('p', { class: 'muted' }, t('pipelines.plan.noSinks')));
}

/**
 * 問題の一覧（押すとその部品へ）。
 * @param {{level: string, text: string, node?: string|null, code?: string|null}[]} items node は部品の id（編集用のモデル）
 * @param {{onFocus: (id: string) => void, nameOf: (id: string) => string}} o
 */
export function issueList(items, { onFocus, nameOf }) {
  return h('ul', { class: 'pl-issues', 'data-testid': 'issues' }, items.map((i) => {
    const helpText = i.code ? issueHelp(i.code) : null;
    const body = [
      h('span', { class: 'pl-issue-text' }, i.text),
      helpText ? h('span', { class: 'pl-issue-fix' }, h('strong', null, helpText.title), ' ', helpText.fix) : null,
    ];
    const inner = [icon(i.level === 'error' ? 'error' : 'warning', { size: 'sm' }), h('span', { class: 'pl-issue-body' }, body),
      i.node ? h('span', { class: 'pl-issue-node' }, nameOf(i.node), icon('chevron-right', { size: 'sm' })) : null];
    return h('li', { class: ['pl-issue', i.level, i.remote && 'remote'], 'data-code': i.code || null },
      i.node ? h('button', { type: 'button', class: 'pl-issue-btn', onClick: () => onFocus(i.node) }, inner) : h('div', { class: 'pl-issue-btn static' }, inner));
  }));
}
