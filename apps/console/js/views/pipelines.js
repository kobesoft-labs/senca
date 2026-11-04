// パイプライン（REQ-005）: 一覧（#/pipelines）と編集（#/pipelines/{id}、下書きは #/pipelines/{key}）。
//
// 一覧: 保存済みのもの（状態・入力 → 出力・最後のエラー）と、このブラウザの下書き。新しいパイプラインはひな形から作る
// （#/pipelines?new=1、?template=switcher で選んだ状態で開く）。編集は js/pipeline/editor.js。

import { h, replace } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { toast, reportError, confirmDialog, load } from '../lib/ui.js';
import { button, iconButton, pageHeader, panel, alert, emptyState, dataTable, badge, helpHint, attachMenu } from '../ui/index.js';
import { onPageKey } from '../shortcuts.js';
import { fmtRelative, fmtTime } from '../lib/format.js';
import { invalidatePalette } from '../palette.js';
import { pipelines as api, NotAvailableError } from '../api/pipelines.js';
import { streams as streamsApi, listAll } from '../api/resources.js';
import { composeNodes, graphStreams, emptyPipeline, fromGraph, positionsOf, toGraph } from '../pipeline/model.js';
import { TEMPLATES } from '../pipeline/templates.js';
import { templateGallery, miniGraph } from '../pipeline/gallery.js';
import { drafts, edits, layouts } from '../pipeline/store.js';
import { observedBadge } from '../pipeline/status.js';
import { renderEditor } from '../pipeline/editor.js';

export { observedBadge };

const query = () => new URLSearchParams(location.hash.split('?')[1] || '');

export async function render(root, params, ctx) {
  const q = query();
  // 古いリンク（#/pipelines?id=…）は編集の経路へ
  if (!params[0] && q.get('id')) {
    history.replaceState(null, '', `#/pipelines/${encodeURIComponent(q.get('id'))}`);
    return renderEditor(root, { id: q.get('id'), ctx });
  }
  if (params[0]) return renderEditor(root, { id: params[0], ctx });
  return renderList(root, ctx);
}

/** ひな形から下書きを作って、編集を開く。 */
async function createFromTemplate(ctx, { streamList, existing, template = null }) {
  const r = await templateGallery({ streamList, initial: template, existingNames: existing });
  if (!r) return;
  const pl = fromGraph(r.graph, { name: r.name });
  const key = emptyPipeline().key;
  drafts.put({ key, name: r.name, graph: toGraph(pl), positions: positionsOf(pl), template: r.template });
  ctx.navigate(`#/pipelines/${key}`);
}

async function renderList(root, ctx) {
  root.classList.remove('full');
  let items = [];
  let streamList = [];
  let available = true;
  let dead = false;
  const listBox = h('div');
  const banner = h('div');
  const streamName = (id) => (streamList.find((x) => x.id === id) || {}).name || (id ? id.slice(0, 8) : '—');
  const existing = () => [...items.map((p) => p.name), ...drafts.all().map((d) => d.name)];
  const create = (template = null) => createFromTemplate(ctx, { streamList, existing: existing(), template });

  const setDesired = async (p, want) => {
    try {
      const cur = await api.get(p.id);
      await api.setDesired(p.id, want, cur.etag || `"${cur.data.generation}"`);
      toast(want === 'running' ? t('pipelines.ed.starting') : t('pipelines.stopped'), 'ok');
      await refresh();
    } catch (e) { reportError(e, want === 'running' ? t('pipelines.start') : t('pipelines.stop'), { retry: () => setDesired(p, want) }); }
  };

  const removeRow = async (row) => {
    if (row.draft) {
      if (!(await confirmDialog(t('pipelines.deleteDraftTitle'), t('pipelines.deleteDraftBody', { name: row.name }), { ok: t('common.delete') }))) return;
      drafts.remove(row.key);
    } else {
      if (!(await confirmDialog(t('pipelines.deleteTitle'), t('pipelines.deleteBody', { name: row.name }), { ok: t('common.delete'), requireText: row.name }))) return;
      try {
        await api.remove(row.id);
        layouts.remove(row.id);
        edits.remove(row.id);
        invalidatePalette();
      } catch (e) { reportError(e, t('common.delete')); return; }
    }
    toast(t('common.deleted'), 'ok');
    refresh();
  };

  const io = (graph) => {
    const { inputs, outputs } = graphStreams(graph);
    const chip = (id) => h('span', { class: 'pl-stream-chip', title: id }, streamName(id));
    return h('span', { class: 'pl-io' },
      inputs.length ? inputs.map(chip) : h('span', { class: 'muted' }, '—'),
      icon('arrow-right', { size: 'sm' }),
      outputs.length ? outputs.map(chip) : h('span', { class: 'muted' }, t('pipelines.list.noOutput')));
  };

  const rows = () => [
    ...drafts.all().map((d) => ({ draft: true, key: d.key, name: d.name, graph: d.graph, updated_at: d.updated_at, href: `#/pipelines/${d.key}` })),
    ...items.map((p) => ({ draft: false, id: p.id, key: p.id, name: p.name, graph: p.graph, updated_at: p.updated_at, p, href: `#/pipelines/${encodeURIComponent(p.id)}`, unsaved: !!edits.get(p.id) })),
  ];

  const columns = [
    { key: 'name', label: t('pipelines.name'), primary: true, sort: (r) => r.name, render: (r) => h('div', { class: 'pl-name-cell' },
      h('a', { href: r.href, class: 'pl-name-link', 'data-testid': `pipeline-row-${r.name}` }, r.name),
      r.draft ? badge(t('pipelines.draft'), 'outline') : null,
      r.unsaved ? badge(t('pipelines.unsaved'), 'warn') : null) },
    { key: 'state', label: t('common.state'), sort: (r) => (r.draft ? 'draft' : r.p.observed_state), render: (r) => (r.draft ? h('span', { class: 'muted small' }, t('pipelines.list.notSaved')) : observedBadge(r.p.observed_state)) },
    { label: t('pipelines.list.io'), wrap: true, render: (r) => io(r.graph) },
    { key: 'nodes', label: t('pipelines.list.parts'), class: 'num', sort: (r) => (r.graph.nodes || []).length, render: (r) => h('span', { class: 'row' }, String((r.graph.nodes || []).length), composeNodes(r.graph).length ? h('span', { title: t('pipelines.list.hasSwitcher') }, icon('switcher', { size: 'sm', label: t('pipelines.list.hasSwitcher') })) : null) },
    { label: t('pipelines.lastError'), wrap: true, render: (r) => (r.p && r.p.last_error ? h('span', { class: 'pl-last-error', title: r.p.last_error }, icon('error', { size: 'sm' }), r.p.last_error) : h('span', { class: 'muted' }, '—')) },
    { key: 'updated', label: t('common.updated'), sort: (r) => r.updated_at || '', render: (r) => h('span', { title: fmtTime(r.updated_at) }, fmtRelative(r.updated_at)) },
    {
      label: t('common.actions'), actions: true, render: (r) => {
        const out = [];
        if (!r.draft && composeNodes(r.graph).length) out.push(iconButton('switcher', t('pipelines.openSwitcher'), { onClick: () => ctx.navigate(`#/production?pipeline=${encodeURIComponent(r.id)}`), testid: `switcher-${r.name}` }));
        if (!r.draft) {
          out.push(r.p.desired_state === 'running'
            ? iconButton('stop', t('pipelines.stop'), { onClick: () => setDesired(r.p, 'stopped') })
            : iconButton('play', t('pipelines.start'), { onClick: () => setDesired(r.p, 'running'), testid: `start-${r.name}` }));
        }
        const more = iconButton('more', t('pipelines.ed.more'));
        attachMenu(more, [
          { label: t('pipelines.list.open'), icon: 'edit', onSelect: () => ctx.navigate(r.href) },
          { label: t('pipelines.ed.duplicatePipeline'), icon: 'copy', onSelect: () => {
            const key = emptyPipeline().key;
            drafts.put({ key, name: `${r.name}-copy`.slice(0, 64), graph: r.graph, positions: r.draft ? (drafts.get(r.key) || {}).positions : layouts.get(r.id) });
            ctx.navigate(`#/pipelines/${key}`);
          } },
          { separator: true },
          { label: t('common.delete'), icon: 'trash', danger: true, onSelect: () => removeRow(r) },
        ]);
        out.push(more);
        return h('div', { class: 'row' }, out);
      },
    },
  ];

  const templateStrip = () => h('div', { class: 'tpl-strip', role: 'list' }, TEMPLATES.filter((x) => x.id !== 'blank').map((tpl) => h('button', {
    type: 'button', class: 'tpl-chip', role: 'listitem', 'data-template': tpl.id, onClick: () => create(tpl.id),
  }, h('span', { class: 'tpl-chip-art' }, miniGraph(tpl.graph({}))), h('span', { class: 'tpl-chip-text' }, h('strong', null, t(`pipelines.tpl.${tpl.id}.title`)), h('span', { class: 'muted small' }, t(`pipelines.tpl.${tpl.id}.short`))))));

  const content = () => {
    const rs = rows();
    if (!rs.length) {
      return h('section', { class: 'panel pl-first-run', 'data-testid': 'pipelines-empty' },
        emptyState({
          icon: 'pipelines',
          title: t('pipelines.list.emptyTitle'),
          body: t('pipelines.list.emptyBody'),
          actions: [button(t('pipelines.list.startTemplate'), { variant: 'primary', icon: 'sparkles', onClick: () => create(), testid: 'pipelines-new-empty' })],
        }),
        h('h2', { class: 'pl-first-h' }, t('pipelines.list.popular')),
        templateStrip(),
        h('div', { class: 'pl-first-steps' },
          ...[1, 2, 3].map((i) => h('div', { class: 'pl-first-step' }, h('span', { class: 'pl-step-num' }, String(i)), h('div', null, h('strong', null, t(`pipelines.list.step${i}`)), h('p', { class: 'muted small' }, t(`pipelines.list.step${i}Body`)))))));
    }
    return h('div', null,
      dataTable({ columns, rows: rs, rowKey: (r) => r.key, caption: t('nav.pipelines'), sort: { key: 'updated', dir: 'desc' }, testid: 'pipelines-table' }),
      panel({ title: t('pipelines.list.fromTemplate'), icon: 'sparkles', description: t('pipelines.list.fromTemplateBody'), class: 'pl-tpl-panel section-gap' }, templateStrip()));
  };
  const draw = () => replace(listBox, content());

  const fetchItems = async () => {
    try {
      const { data } = await api.list();
      items = data.items;
      available = true;
    } catch (e) {
      if (e instanceof NotAvailableError) available = false;
      else throw e;
    }
    replace(banner, available ? null : alert('warn', t('pipelines.offlineBody'), { title: t('pipelines.offlineTitle') }));
  };
  const refresh = async () => { await fetchItems(); if (!dead) draw(); };

  root.append(
    pageHeader({
      title: t('nav.pipelines'),
      description: t('pipelines.lead'),
      help: helpHint({ title: t('nav.pipelines'), body: t('pipelines.help'), guide: '07-pipelines.md' }),
      actions: [button(t('pipelines.new'), { variant: 'primary', icon: 'plus', kbd: 'N', onClick: () => create(), testid: 'pipelines-new' })],
    }),
    banner,
    listBox);

  try { streamList = await listAll(streamsApi.list); } catch { /* 名前が出ないだけ */ }
  await load(listBox, async () => { await fetchItems(); return content(); }, { kind: 'table', title: t('pipelines.list') });
  const offKey = onPageKey('n', () => create());
  const want = q();
  if (want.get('new')) {
    history.replaceState(null, '', '#/pipelines');
    create(want.get('template'));
  }
  const timer = setInterval(() => { if (document.visibilityState === 'visible' && !document.querySelector('dialog[open]')) refresh().catch(() => {}); }, 5000);
  return () => { dead = true; offKey(); clearInterval(timer); };

  function q() { return query(); }
}
