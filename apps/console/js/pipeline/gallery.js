// 新しいパイプライン: ひな形を選ぶ → 入力と出力の stream を選ぶ → 下書きを作る（ダイアログ）。
//
// - ひな形のカードには部品のつながりの小さな絵、入力・出力の数、使うコーデックを出す。
// - stream は配信中のものを入力の候補の先に出し、配信中ならコーデックと大きさをそのグラフに入れる（H.264 なら GPU が要ることを知らせる）。
// - 出力は「新しい stream を作る」を選べる（作成のときに fMP4 の stream を作る）。

import { h, s, replace } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { modal, reportError } from '../lib/ui.js';
import { formField, alert, badge } from '../ui/index.js';
import { streams as streamsApi } from '../api/resources.js';
import { TEMPLATES, templateById, templateStats, fillTemplate } from './templates.js';
import { KINDS, NODE_W, nodeHeight, portOffset, fromGraph, isPipelineName, describeTracks, GPU_CODECS } from './model.js';
import { streamSelect, NEW_STREAM } from './streams.js';

/** グラフの小さな絵（SVG）。 */
export function miniGraph(graph) {
  let pl;
  try { pl = fromGraph(graph, { name: 'x' }); } catch { return s('svg'); }
  if (!pl.nodes.length) {
    return s('svg', { class: 'tpl-mini', viewBox: '0 0 300 120', 'aria-hidden': 'true' },
      s('rect', { class: 'tpl-mini-empty', x: 110, y: 30, width: 80, height: 60, rx: 10 }),
      s('path', { class: 'tpl-mini-plus', d: 'M150 48 V72 M138 60 H162' }));
  }
  const x2 = Math.max(...pl.nodes.map((n) => n.x + NODE_W));
  const y2 = Math.max(...pl.nodes.map((n) => n.y + nodeHeight(n)));
  const pos = (n, dir, i) => { const o = portOffset(n, dir, i); return [n.x + o.x, n.y + o.y]; };
  const byId = new Map(pl.nodes.map((n) => [n.id, n]));
  return s('svg', { class: 'tpl-mini', viewBox: `0 0 ${x2 + 40} ${y2 + 40}`, preserveAspectRatio: 'xMidYMid meet', 'aria-hidden': 'true' },
    pl.edges.map((e) => {
      const [ax, ay] = pos(byId.get(e.from.node), 'out', e.from.port);
      const [bx, by] = pos(byId.get(e.to.node), 'in', e.to.port);
      const dx = Math.max(40, Math.abs(bx - ax) / 2);
      return s('path', { class: 'tpl-mini-edge', d: `M${ax},${ay} C${ax + dx},${ay} ${bx - dx},${by} ${bx},${by}` });
    }),
    pl.nodes.map((n) => s('rect', { class: `tpl-mini-node k-${KINDS[n.type].cat}`, x: n.x, y: n.y, width: NODE_W, height: nodeHeight(n), rx: 16 })));
}

/**
 * ひな形を選んで、パイプラインの下書きの元（{name, graph}）を返す。やめたら null。
 * @param {{streamList: object[], initial?: string, existingNames?: string[]}} o
 */
export async function templateGallery({ streamList, initial = null, existingNames = [] }) {
  let picked = initial ? templateById(initial) : null;
  const body = h('div', { class: 'tpl-body' });
  let closeFn = null;
  let result = null;

  const uniqueName = (base) => {
    let name = base;
    for (let i = 2; existingNames.includes(name); i += 1) name = `${base}-${i}`;
    return name;
  };

  const drawGrid = () => {
    picked = null;
    replace(body,
      h('p', { class: 'muted' }, t('pipelines.tpl.lead')),
      h('div', { class: 'tpl-grid', role: 'list' }, TEMPLATES.map((tpl) => {
        const st = templateStats(tpl);
        const card = h('button', { type: 'button', class: ['tpl-card', tpl.id === 'blank' && 'blank'], role: 'listitem', 'data-template': tpl.id, onClick: () => { picked = tpl; drawForm(); } },
          h('div', { class: 'tpl-art' }, miniGraph(tpl.graph({}))),
          h('div', { class: 'tpl-text' },
            h('div', { class: 'tpl-title' }, icon(tpl.icon), h('strong', null, t(`pipelines.tpl.${tpl.id}.title`))),
            h('p', null, t(`pipelines.tpl.${tpl.id}.desc`)),
            tpl.id === 'blank' ? null : h('div', { class: 'tpl-tags' },
              h('span', { class: 'tag' }, t('pipelines.tpl.inputs', { n: st.inputs })),
              h('span', { class: 'tag' }, t('pipelines.tpl.outputs', { n: st.outputs })),
              st.compose ? h('span', { class: 'tag accent' }, t('pipelines.tpl.hasSwitcher')) : null,
              ...st.codecs.map((c) => h('span', { class: 'tag' }, c.toUpperCase())))));
        return card;
      })));
    const first = body.querySelector('.tpl-card');
    if (first) first.focus();
  };

  const drawForm = () => {
    const tpl = picked;
    const name = h('input', { type: 'text', value: uniqueName(tpl.id === 'blank' ? 'pipeline' : tpl.id), maxlength: 64, autocomplete: 'off', spellcheck: 'false', 'data-testid': 'tpl-name' });
    const nameField = formField({ label: t('pipelines.name'), input: name, required: true, hint: t('pipelines.nameHint') });
    const opts = {};
    const optBoxes = (tpl.options || []).map((o) => {
      opts[o.id] = o.def;
      const cb = h('input', { type: 'checkbox', checked: o.def, 'data-testid': `tpl-opt-${o.id}` });
      cb.addEventListener('change', () => { opts[o.id] = cb.checked; drawRoles(); });
      return h('div', { class: 'field' }, h('label', { class: 'check' }, cb, t(`pipelines.tpl.opt.${o.id}`)), h('div', { class: 'hint' }, t(`pipelines.tpl.opt.${o.id}.hint`)));
    });
    const rolesBox = h('div', { class: 'form' });
    const selects = {};
    const used = new Set();
    const drawRoles = () => {
      const roles = tpl.roles.filter((r) => !r.option || opts[r.option]);
      used.clear();
      replace(rolesBox, roles.map((r) => {
        const live = streamList.filter((x) => x.observed_state === 'live' && !used.has(x.id));
        const idle = streamList.filter((x) => x.observed_state !== 'live' && !used.has(x.id));
        let def = selects[r.node] ? selects[r.node].value : '';
        if (!def) def = r.kind === 'input' ? ((live[0] || {}).id || '') : ((idle[0] || {}).id || NEW_STREAM);
        if (def && def !== NEW_STREAM) used.add(def);
        const sel = streamSelect(streamList, def === NEW_STREAM ? '' : def, { kind: r.kind, allowNew: r.kind === 'output', attrs: { 'data-testid': `tpl-role-${r.node}` } });
        if (def === NEW_STREAM) sel.value = NEW_STREAM;
        selects[r.node] = sel;
        const note = h('div', { class: 'hint' });
        const sync = () => {
          const st = streamList.find((x) => x.id === sel.value);
          const tracks = st && st.live && st.live.tracks;
          const gpu = tracks && tracks.some((x) => GPU_CODECS.has(x.codec));
          replace(note,
            r.kind === 'output' && sel.value === NEW_STREAM ? t('pipelines.tpl.newStreamNote', { name: `${name.value}-${r.role}` }) : null,
            tracks ? h('span', { class: 'row' }, badge(t('status.live'), 'live'), ' ', describeTracks(tracks)) : null,
            gpu ? h('span', { class: 'warn-text' }, ' ', t('pipelines.tpl.gpuNote')) : null);
        };
        sel.addEventListener('change', sync);
        name.addEventListener('input', sync);
        sync();
        return formField({ label: h('span', { class: 'row' }, icon(r.kind === 'input' ? 'streams' : 'live', { size: 'sm' }), t(`pipelines.tpl.role.${r.role}`)), input: sel, hint: note });
      }));
    };
    drawRoles();
    const err = h('div');
    const create = async () => {
      if (!isPipelineName(name.value)) { nameField.setError(t('pipelines.v.pipelineName')); name.focus(); return; }
      if (existingNames.includes(name.value)) { nameField.setError(t('pipelines.tpl.nameTaken')); name.focus(); return; }
      const chosen = {};
      try {
        for (const [node, sel] of Object.entries(selects)) {
          const role = tpl.roles.find((r) => r.node === node);
          if (role.option && !opts[role.option]) continue;
          if (sel.value === NEW_STREAM) {
            const { data } = await streamsApi.create({ name: `${name.value}-${role.role}`.slice(0, 64), codec_profile: 'royalty_free', hls: { container: 'fmp4' } });
            chosen[node] = data.id;
          } else if (sel.value) chosen[node] = sel.value;
        }
      } catch (e) {
        replace(err, alert('error', e.title || e.message, { title: t('pipelines.tpl.createFailed'), small: true }));
        reportError(e, t('pipelines.stream.create'));
        return;
      }
      const graph = fillTemplate(tpl, chosen, opts);
      // 配信中の stream のコーデックを入れる
      for (const n of graph.nodes) {
        if (n.type !== 'source' || !n.stream) continue;
        const st = streamList.find((x) => x.id === n.stream);
        const tracks = st && st.live && st.live.tracks;
        if (!tracks) continue;
        const v = tracks.find((x) => KINDS.source && ['av1', 'vp9', 'vp8', 'h264', 'h265'].includes(x.codec));
        const a = tracks.find((x) => ['opus', 'aac', 'pcma', 'pcmu'].includes(x.codec));
        if (n.video && v) Object.assign(n.video, { codec: v.codec });
        if (n.audio && a) Object.assign(n.audio, { codec: a.codec });
      }
      result = { name: name.value, graph, template: tpl.id };
      closeFn(true);
    };
    name.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); create(); } });
    name.addEventListener('input', () => nameField.setError(null));
    replace(body,
      h('div', { class: 'tpl-form' },
        h('div', { class: 'tpl-form-art' }, miniGraph(tpl.graph(opts)), h('div', null, h('div', { class: 'tpl-title' }, icon(tpl.icon), h('strong', null, t(`pipelines.tpl.${tpl.id}.title`))), h('p', { class: 'muted small' }, t(`pipelines.tpl.${tpl.id}.desc`)))),
        h('div', { class: 'form' },
          nameField,
          ...optBoxes,
          tpl.roles.length ? h('fieldset', null, h('legend', null, t('pipelines.tpl.streams')), h('p', { class: 'hint' }, t('pipelines.tpl.streamsHint')), rolesBox) : null,
          err,
          h('div', { class: 'tpl-actions' },
            h('button', { type: 'button', class: 'btn ghost', onClick: drawGrid }, icon('arrow-left'), t('pipelines.tpl.back')),
            h('button', { type: 'button', class: 'btn primary', 'data-testid': 'tpl-create', onClick: create }, icon('sparkles'), t('pipelines.tpl.create'))))));
    name.focus();
    name.select();
  };

  const ok = await modal(t('pipelines.tpl.title'), (close) => {
    closeFn = close;
    if (picked) drawForm(); else drawGrid();
    return body;
  }, { wide: true, icon: 'sparkles', testid: 'template-gallery', initialFocus: picked ? '[data-testid="tpl-name"]' : '.tpl-card' });
  return ok ? result : null;
}
