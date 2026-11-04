// 検査器（右の欄）: 選んだ部品の設定・つなぎ先、選んだ線、何も選んでいなければパイプライン全体の設定。
//
// 欄は部品の種類（model.js の KINDS の fields）から作る。説明（help）は欄の下に出し、細かい設定は「詳しい設定」に畳む。
// つなぐのはキーボードでもできる: 入力ごとに「つなぐ元」、出力ごとに「つなぐ先」を選ぶ。

import { h, replace, select } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { toast } from '../lib/ui.js';
import { formField, badge, alert } from '../ui/index.js';
import {
  KINDS, OPTION_FIELDS, PORT_LABEL, inPorts, outPorts, outType, connect, connectSources, connectTargets, nodeById, edgeInto,
  removeNamedInput, addNamedInput, isNodeName, isPipelineName, applyLiveTracks, describeTracks, refOf,
} from './model.js';
import { streamSelect, createStreamDialog, NEW_STREAM } from './streams.js';

const portRef = (p, ref) => {
  const n = nodeById(p, ref.node);
  return n ? `${n.name}.${outPorts(n)[ref.port].name}` : '?';
};

/**
 * @param {HTMLElement} box
 * @param {object} ctx
 */
export function renderInspector(box, ctx) {
  const p = ctx.pl();
  const { sel } = ctx;
  const active = document.activeElement;
  const keep = active && box.contains(active) ? active.getAttribute('data-k') : null;
  let content;
  if (sel.nodes.size === 1) content = nodeInspector(nodeById(p, [...sel.nodes][0]), ctx);
  else if (sel.nodes.size > 1) content = multiInspector(ctx);
  else if (sel.edge && p.edges.some((e) => e.id === sel.edge)) content = edgeInspector(p.edges.find((e) => e.id === sel.edge), ctx);
  else content = pipelineInspector(ctx);
  replace(box, content);
  if (keep) {
    const again = box.querySelector(`[data-k="${CSS.escape(keep)}"]`);
    if (again) again.focus();
  }
}

// ---------------------------------------------------------------- 欄
function fieldInput(f, getV, setV, n, ctx, key) {
  const v = getV();
  let input;
  if (f.type === 'select') input = select(f.options.map((o) => (Array.isArray(o) ? o : [o, o])), v ?? f.def ?? '');
  else if (f.type === 'stream') {
    input = streamSelect(ctx.streamList(), v || '', { kind: n && n.type === 'source' ? 'input' : 'output', allowNew: true });
  } else if (f.type === 'scene') input = select([['', t('pipelines.none')], ...Object.keys((n && n.p.scenes) || {}).map((x) => [x, x])], v || '');
  else if (f.type === 'check') {
    input = h('input', { type: 'checkbox', checked: v ?? f.def ?? false, 'data-k': key });
    input.addEventListener('change', () => setV(input.checked));
    return h('div', { class: 'field' }, h('label', { class: 'check' }, input, f.label), f.help ? h('div', { class: 'hint' }, f.help) : null);
  } else if (f.type === 'color') input = h('input', { type: 'color', value: v || f.def || '#000000' });
  else input = h('input', { type: f.type === 'number' ? 'number' : 'text', value: v ?? '', min: f.min, max: f.max, step: f.step, placeholder: f.def !== undefined ? String(f.def) : '' });
  input.setAttribute('data-k', key);
  input.addEventListener('change', async () => {
    const x = input.value;
    if (f.type === 'stream' && x === NEW_STREAM) {
      const s = await createStreamDialog({ suggest: `${ctx.pl().name}-${n ? n.name : 'out'}`.slice(0, 64), royaltyFree: ctx.royaltyFree() });
      if (s) { await ctx.refreshStreams(); setV(s.id); } else input.value = v || '';
      return;
    }
    setV(f.type === 'number' ? (x === '' ? null : Number(x)) : x);
  });
  return formField({ label: f.label, input, required: !!f.required, hint: f.help || null });
}

function fieldsOf(n, ctx) {
  const k = KINDS[n.type];
  const visible = k.fields.filter((f) => !f.when || f.when(n.p));
  const make = (f) => fieldInput(f, () => n.p[f.path], (v) => {
    n.p[f.path] = v;
    let rerender = f.path.endsWith('.codec') || f.type === 'stream' || f.path === 'codec';
    if (n.type === 'source' && f.type === 'stream') {
      const s = ctx.streamList().find((x) => x.id === v);
      if (s && s.live && s.live.tracks && applyLiveTracks(n, s.live.tracks).length) {
        toast(t('pipelines.insp.tracksApplied', { tracks: describeTracks(s.live.tracks) }), 'ok');
        rerender = true;
      }
    }
    ctx.change('field', { rerender });
  }, n, ctx, `f:${f.path}`);
  const basic = visible.filter((f) => !f.advanced).map(make);
  const advanced = visible.filter((f) => f.advanced).map(make);
  return [basic, advanced];
}

// ---------------------------------------------------------------- つなぎ先
function connections(n, ctx) {
  const p = ctx.pl();
  const k = KINDS[n.type];
  const rows = [];
  // 入力: つなぐ元を選ぶ（名前付きの入力は名前も変えられる）
  inPorts(n).forEach((port, i) => {
    const e = edgeInto(p, n.id, i);
    const sources = connectSources(p, { node: n.id, port: i });
    const opts = [['', t('pipelines.insp.notConnected')], ...sources.map((s) => [`${s.node}|${s.port}`, `${nodeById(p, s.node).name}.${s.name} — ${PORT_LABEL[s.type] || '?'}`])];
    if (e && !sources.some((s) => s.node === e.from.node && s.port === e.from.port)) opts.push([`${e.from.node}|${e.from.port}`, portRef(p, e.from)]);
    const sel = select(opts, e ? `${e.from.node}|${e.from.port}` : '', { 'data-k': `in:${i}`, 'data-testid': `connect-from-${i}` });
    sel.addEventListener('change', () => {
      p.edges = p.edges.filter((x) => !(x.to.node === n.id && x.to.port === i));
      if (sel.value) {
        const [nid, pi] = sel.value.split('|');
        const r = connect(p, { node: nid, port: Number(pi) }, { node: n.id, port: i }, { replace: true });
        if (!r.ok) toast(r.reason, 'warn');
      }
      ctx.change(sel.value ? 'connect' : 'disconnect', { rerender: true });
    });
    let nameIn = null;
    if (k.named) {
      const inp = n.ins[i];
      nameIn = h('input', { type: 'text', value: inp.name, 'aria-label': t('pipelines.inputNameAria', { n: i + 1 }), class: 'pl-in-name', 'data-k': `inname:${i}`, maxlength: 64 });
      nameIn.addEventListener('change', () => {
        const old = inp.name;
        if (!isNodeName(nameIn.value) || n.ins.some((x, j) => j !== i && x.name === nameIn.value)) { toast(t('pipelines.inputNameInvalid'), 'warn'); nameIn.value = old; return; }
        inp.name = nameIn.value;
        if (n.type === 'compose') for (const sc of Object.values(n.p.scenes || {})) for (const l of sc.layers || []) if (l.source === old) l.source = inp.name;
        ctx.change('rename-input', { rerender: true });
      });
    }
    const label = h('label', { class: 'pl-conn-label', for: sel.id || (sel.id = `pl-in-${n.id}-${i}`) }, icon('arrow-right', { size: 'sm' }), k.named ? t('pipelines.insp.inputN', { n: i + 1 }) : port.name);
    const mixExtras = n.type === 'audio_mix' ? mixRow(n, i, ctx) : null;
    rows.push(h('div', { class: 'pl-conn in' },
      h('div', { class: 'pl-conn-head' }, label, k.named ? h('button', {
        type: 'button', class: 'btn ghost icon-only sm', 'aria-label': t('pipelines.removeInput', { input: n.ins[i].name }),
        onClick: () => { removeNamedInput(p, n, i); ctx.change('remove-input', { rerender: true }); },
      }, icon('close')) : null),
      nameIn ? h('div', { class: 'pl-conn-row' }, nameIn, sel) : sel,
      mixExtras));
  });
  if (k.named) {
    rows.push(h('button', { type: 'button', class: 'btn sm', 'data-k': 'add-input', onClick: () => { addNamedInput(n, n.type === 'compose' ? 'cam' : 'in'); ctx.change('add-input', { rerender: true }); } }, icon('plus'), t('pipelines.addInput')));
  }
  // 出力: 今のつなぎ先と、つなぐ先を足す
  outPorts(n).forEach((o, oi) => {
    const from = { node: n.id, port: oi };
    const ty = outType(p, from);
    const outs = p.edges.filter((e) => e.from.node === n.id && e.from.port === oi);
    const targets = connectTargets(p, from).filter((x) => !outs.some((e) => e.to.node === x.node && e.to.port === x.port));
    const add = select([['', t('pipelines.insp.connectTo')], ...targets.map((x) => [`${x.node}|${x.port}`, `${nodeById(p, x.node).name} · ${x.name}${x.taken ? t('pipelines.insp.replaces') : ''}`])], '', { 'data-k': `out:${oi}`, 'aria-label': t('pipelines.insp.connectToAria', { port: o.name }), 'data-testid': `connect-to-${oi}`, disabled: !targets.length });
    add.addEventListener('change', () => {
      if (!add.value) return;
      const [nid, pi] = add.value.split('|');
      const r = connect(p, from, { node: nid, port: Number(pi) }, { replace: true });
      if (!r.ok) toast(r.reason, 'warn');
      ctx.change('connect', { rerender: true });
    });
    rows.push(h('div', { class: 'pl-conn out' },
      h('div', { class: 'pl-conn-head' }, h('span', { class: 'pl-conn-label' }, o.name, ' ', h('span', { class: 'muted' }, `· ${PORT_LABEL[ty] || t('pipelines.insp.typeUnknown')}`), icon('arrow-right', { size: 'sm' }))),
      outs.length ? h('ul', { class: 'pl-chips' }, outs.map((e) => {
        const m = nodeById(p, e.to.node);
        return h('li', null,
          h('button', { type: 'button', class: 'pl-chip', onClick: () => ctx.focusNode(m.id) }, `${m.name} · ${inPorts(m)[e.to.port].name}`),
          h('button', { type: 'button', class: 'btn ghost icon-only sm', 'aria-label': t('pipelines.insp.disconnect', { to: m.name }), onClick: () => { p.edges = p.edges.filter((x) => x !== e); ctx.change('disconnect', { rerender: true }); } }, icon('unlink')));
      })) : h('p', { class: 'hint' }, n.type === 'compose' && oi === 1 ? t('pipelines.insp.previewOptional') : t('pipelines.insp.noOutputs')),
      add));
  });
  return rows;
}

function mixRow(n, i, ctx) {
  const inp = n.ins[i];
  const numIn = (key, label, step, min, max) => {
    const x = h('input', { type: 'number', step, min, max, value: inp[key] ?? 0, 'aria-label': t('pipelines.mixFieldAria', { input: inp.name, field: label }), 'data-k': `mix:${i}:${key}` });
    x.addEventListener('change', () => { inp[key] = Number(x.value) || 0; ctx.change('field'); });
    return h('label', { class: 'pl-mini-field' }, h('span', null, label), x);
  };
  const ck = (key, label) => {
    const x = h('input', { type: 'checkbox', checked: !!inp[key], 'data-k': `mix:${i}:${key}` });
    x.addEventListener('change', () => { inp[key] = x.checked; ctx.change('field'); });
    return h('label', { class: 'check small' }, x, label);
  };
  return h('div', { class: 'pl-mix-row' }, numIn('gain_db', t('pipelines.mix.gain'), 0.5, -120, 24), numIn('pan', t('pipelines.mix.pan'), 0.1, -1, 1), numIn('delay_ms', t('pipelines.mix.delay'), 1, 0, 5000), ck('mute', t('pipelines.mix.mute')), ck('solo', t('pipelines.mix.solo')));
}

// ---------------------------------------------------------------- 部品
function nodeInspector(n, ctx) {
  const p = ctx.pl();
  const k = KINDS[n.type];
  const name = h('input', { type: 'text', value: n.name, maxlength: 64, autocomplete: 'off', spellcheck: 'false', 'data-k': 'name', 'data-testid': 'node-name' });
  const nameField = formField({ label: t('pipelines.nodeName'), input: name, hint: t('pipelines.insp.nameHint') });
  name.addEventListener('input', () => nameField.setError(isNodeName(name.value) && !p.nodes.some((m) => m !== n && m.name === name.value) ? null : t('pipelines.nodeNameInvalid')));
  name.addEventListener('change', () => {
    if (!isNodeName(name.value) || p.nodes.some((m) => m !== n && m.name === name.value)) { name.value = n.name; nameField.setError(null); return; }
    n.name = name.value;
    ctx.change('rename');
  });
  const [basic, advanced] = fieldsOf(n, ctx);
  const issues = ctx.issuesOf(n.id);
  let live = null;
  if (n.type === 'source' && n.p.stream) {
    const s = ctx.streamList().find((x) => x.id === n.p.stream);
    if (s && s.observed_state === 'live' && s.live && s.live.tracks) {
      const probe = { type: 'source', p: { ...n.p } };
      const differs = applyLiveTracks(probe, s.live.tracks).length > 0;
      live = h('div', { class: 'pl-live-note' }, icon('live', { size: 'sm' }),
        h('span', null, t('pipelines.insp.liveTracks', { tracks: describeTracks(s.live.tracks) })),
        differs ? h('button', { type: 'button', class: 'btn sm', 'data-k': 'apply-tracks', onClick: () => { applyLiveTracks(n, s.live.tracks); ctx.change('field', { rerender: true }); } }, t('pipelines.insp.useTracks')) : badge(t('pipelines.insp.tracksMatch'), 'ok'));
    } else if (s) live = h('div', { class: 'pl-live-note muted' }, icon('info', { size: 'sm' }), t('pipelines.insp.notLive'));
  }
  const scenes = n.type === 'compose' ? Object.keys(n.p.scenes || {}) : null;
  return h('div', { class: 'pl-insp', 'data-testid': 'inspector-node' },
    h('div', { class: ['pl-insp-head', `k-${k.cat}`] },
      h('span', { class: 'pl-kind-chip' }, icon(k.icon)),
      h('div', { class: 'pl-insp-title' }, h('h2', null, n.name), h('span', { class: 'muted small' }, k.label))),
    h('p', { class: 'pl-insp-desc' }, k.desc),
    issues.length ? h('ul', { class: 'pl-insp-issues' }, issues.map((i) => h('li', { class: i.level }, icon(i.level === 'error' ? 'error' : 'warning', { size: 'sm' }), h('span', null, i.text)))) : null,
    h('div', { class: 'form' },
      nameField,
      live,
      ...basic,
      scenes ? h('div', { class: 'field' },
        h('span', { class: 'label' }, t('pipelines.insp.scenes')),
        h('div', { class: 'pl-scene-chips' }, scenes.map((sn) => h('span', { class: ['tag', sn === n.p.program && 'pgm', sn === n.p.preview && 'pvw'] }, sn))),
        h('button', { type: 'button', class: 'btn', 'data-k': 'scenes', 'data-testid': 'edit-scenes', onClick: () => ctx.openScenes(n) }, icon('layout-grid'), t('pipelines.editScenes'))) : null,
      h('fieldset', { class: 'pl-conns' }, h('legend', null, t('pipelines.insp.connections')), connections(n, ctx)),
      advanced.length ? h('details', { class: 'pl-adv' }, h('summary', null, t('pipelines.insp.advanced', { n: advanced.length })), h('div', { class: 'form' }, ...advanced)) : null,
      Object.keys(n.extra || {}).length ? h('p', { class: 'hint' }, t('pipelines.extraKeys', { keys: Object.keys(n.extra).join(', ') })) : null,
      h('div', { class: 'pl-insp-actions' },
        h('button', { type: 'button', class: 'btn sm', 'data-k': 'dup', onClick: () => ctx.duplicate([n.id]) }, icon('copy'), t('pipelines.insp.duplicate')),
        h('button', { type: 'button', class: 'btn danger sm', 'data-k': 'del', 'data-testid': 'node-delete', onClick: () => ctx.remove([n.id]) }, icon('trash'), t('pipelines.deleteNode')))));
}

function multiInspector(ctx) {
  const ids = [...ctx.sel.nodes];
  const p = ctx.pl();
  return h('div', { class: 'pl-insp' },
    h('div', { class: 'pl-insp-head' }, h('span', { class: 'pl-kind-chip' }, icon('layout-grid')), h('div', { class: 'pl-insp-title' }, h('h2', null, t('pipelines.insp.multi', { n: ids.length })))),
    h('ul', { class: 'pl-chips' }, ids.map((id) => nodeById(p, id)).filter(Boolean).map((n) => h('li', null, h('button', { type: 'button', class: 'pl-chip', onClick: () => ctx.focusNode(n.id) }, n.name)))),
    h('div', { class: 'pl-insp-actions' },
      h('button', { type: 'button', class: 'btn sm', onClick: () => ctx.duplicate(ids) }, icon('copy'), t('pipelines.insp.duplicate')),
      h('button', { type: 'button', class: 'btn danger sm', onClick: () => ctx.remove(ids) }, icon('trash'), t('pipelines.insp.deleteN', { n: ids.length }))),
    h('p', { class: 'hint' }, t('pipelines.insp.multiHint')));
}

function edgeInspector(e, ctx) {
  const p = ctx.pl();
  const a = nodeById(p, e.from.node);
  const b = nodeById(p, e.to.node);
  const ty = outType(p, e.from);
  return h('div', { class: 'pl-insp' },
    h('div', { class: 'pl-insp-head' }, h('span', { class: 'pl-kind-chip' }, icon('link')), h('div', { class: 'pl-insp-title' }, h('h2', null, t('pipelines.edge')), h('span', { class: 'muted small' }, PORT_LABEL[ty] || ''))),
    h('div', { class: 'pl-edge-route' },
      h('button', { type: 'button', class: 'pl-chip', onClick: () => ctx.focusNode(a.id) }, refOf(p, e.from)),
      icon('arrow-right', { size: 'sm' }),
      h('button', { type: 'button', class: 'pl-chip', onClick: () => ctx.focusNode(b.id) }, `${b.name} · ${inPorts(b)[e.to.port].name}`)),
    h('p', { class: 'hint' }, t('pipelines.insp.edgeHint')),
    h('div', { class: 'pl-insp-actions' }, h('button', { type: 'button', class: 'btn danger sm', 'data-k': 'del-edge', onClick: () => { p.edges = p.edges.filter((x) => x.id !== e.id); ctx.sel.edge = null; ctx.change('disconnect', { rerender: true }); } }, icon('unlink'), t('pipelines.deleteEdge'))));
}

// ---------------------------------------------------------------- 全体
function pipelineInspector(ctx) {
  const p = ctx.pl();
  const name = h('input', { type: 'text', value: p.name, maxlength: 64, autocomplete: 'off', spellcheck: 'false', 'data-k': 'pl-name', 'data-testid': 'pipeline-name' });
  const nameField = formField({ label: t('pipelines.name'), input: name, required: true, hint: t('pipelines.nameHint') });
  name.addEventListener('input', () => nameField.setError(isPipelineName(name.value) ? null : t('pipelines.v.pipelineName')));
  name.addEventListener('change', () => { p.name = name.value; ctx.change('rename-pipeline', { rerender: false, renamed: true }); });
  const opts = OPTION_FIELDS.map((f) => fieldInput(f, () => p.options[f.path], (v) => { p.options[f.path] = v; ctx.change('options'); }, null, ctx, `opt:${f.path}`));
  const counts = { nodes: p.nodes.length, edges: p.edges.length };
  return h('div', { class: 'pl-insp', 'data-testid': 'inspector-pipeline' },
    h('div', { class: 'pl-insp-head' }, h('span', { class: 'pl-kind-chip' }, icon('pipelines')), h('div', { class: 'pl-insp-title' }, h('h2', null, t('pipelines.insp.pipeline')), h('span', { class: 'muted small' }, t('pipelines.counts', counts)))),
    h('div', { class: 'form' },
      nameField,
      h('details', { class: 'pl-adv' }, h('summary', null, t('pipelines.options')), h('div', { class: 'form' }, ...opts)),
      p.nodes.length ? alert('info', h('div', null,
        h('p', null, t('pipelines.insp.tipSelect')),
        h('ul', { class: 'pl-tips' },
          h('li', null, t('pipelines.insp.tipConnect')),
          h('li', null, t('pipelines.insp.tipKeys')))), { small: true }) : null));
}
