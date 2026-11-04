// 監査ログ: 誰が・いつ・何を変えたか（GET /audit、新しい順）。tenant:admin が要る。
//
// - 条件（検索・期間・操作の種類・主体・資源・ノード・テナント）は URL（#/audit?…）に残す。変えたら最初から読み直す。
// - 1 回の応答の項目が limit より少なくても（0 件でも）next_cursor が返ることがあるので、見える分が埋まるまでたどる（api/ops.js の auditFill）。
//   下の端が見えたら続きを読む（IntersectionObserver）。「さらに読む」のボタンもある。
// - 時間内に答えなかったノード（unavailable_nodes）は警告に出す。node= で止まっているノードを選ぶと 503。
// - 書き出し（CSV・JSON）は今の条件で最後までたどる（上限 MAX_EXPORT 件）。
// - 知らない操作（新しく増えたもの）は lib/audit-actions.js が分類か ID のまま出す。

import { h, replace, select, debounce } from '../lib/dom.js';
import { errorBox, downloadBlob, reportError, toast, copyField } from '../lib/ui.js';
import { system } from '../api/resources.js';
import { audit as auditApi, auditFill } from '../api/ops.js';
import { loadNames, shortId } from '../lib/names.js';
import { CATEGORIES, categoryOf, actionLabel, resourceHref, subjectTokenId } from '../lib/audit-actions.js';
import { fmtTime, fmtClock, fmtDate, fmtNumber, wallToUtcIso, utcToWall, localZone } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { listNav } from '../lib/listnav.js';
import { t, tn } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { button, pageHeader, panel, badge, alert, helpHint, skeleton, emptyState, segmented, searchBox, attachMenu, kvList, loadingLine } from '../ui/index.js';

const PAGE_WANT = 50;
const MAX_EXPORT = 20000;
const RANGES = [['', 'all'], ['1h', 3600e3], ['24h', 86400e3], ['7d', 7 * 86400e3], ['30d', 30 * 86400e3], ['custom', null]];
const KEYS = ['q', 'range', 'since', 'until', 'action', 'subject', 'resource', 'node', 'tenant'];
const BASE_FIELDS = ['ts', 'action', 'subject', 'resource', 'request_id', 'tenant', 'node', 'node_name'];

function readParams() {
  const p = new URLSearchParams(location.hash.split('?')[1] || '');
  return Object.fromEntries(KEYS.map((k) => [k, p.get(k) || '']));
}

/** 条件 → API のクエリ。期間の頭は今から数える。 */
function toQuery(f) {
  const q = {};
  if (f.q) q.q = f.q;
  if (f.action) q.action = f.action;
  if (f.subject) q.subject = f.subject;
  if (f.resource) q.resource = f.resource;
  if (f.node) q.node = f.node;
  if (f.tenant) q.tenant = f.tenant;
  const r = RANGES.find(([k]) => k === f.range);
  if (r && typeof r[1] === 'number') q.since = new Date(Date.now() - r[1]).toISOString();
  if (f.range === 'custom') {
    if (f.since) q.since = f.since;
    if (f.until) q.until = f.until;
  }
  return q;
}

/** CSV の 1 欄（"…" で囲み、" は 2 つに）。 */
export function csvCell(v) {
  const s = v === null || v === undefined ? '' : typeof v === 'object' ? JSON.stringify(v) : String(v);
  return /[",\r\n]/.test(s) || /^[=+\-@]/.test(s) ? `"${(/^[=+\-@]/.test(s) ? `'${s}` : s).replace(/"/g, '""')}"` : s;
}

export async function render(root) {
  root = root.appendChild(h('div', { class: 'ops-view' }));
  const f = readParams();
  const zone = localZone();
  const warnBox = h('div');
  const filterBox = h('div', { class: 'filter-bar' });
  const chipsBox = h('div', { class: 'filter-chips', 'aria-live': 'polite' });
  const list = h('ol', { class: 'audit-list', 'data-testid': 'audit-list', 'aria-label': t('audit.listLabel') });
  const statusLine = h('div', { class: 'audit-status', role: 'status' });
  const moreBtn = button(t('audit.loadMore'), { icon: 'chevron-down', testid: 'audit-more', onClick: () => loadMore() });
  const sentinel = h('div', { class: 'audit-sentinel', 'aria-hidden': 'true' });
  const body = h('div', null, skeleton({ kind: 'lines', rows: 6 }));
  let state = null; // {gen, items, next, done, loading, unavailable:Set, lastDay}
  let gen = 0;
  let abort = null;
  let lookup = () => null;
  let nodes = []; // [{id, name}]
  let nodeAdmin = false;
  let disposed = false;

  // ---------------------------------------------------------------- 名前
  const tokenName = (subject) => {
    const id = subjectTokenId(subject);
    return id ? lookup('token', id) : null;
  };
  const resourceName = (e) => {
    const c = categoryOf(e.action);
    return c && c.names ? lookup(c.names, e.resource) : null;
  };

  // ---------------------------------------------------------------- 条件
  const syncUrl = () => {
    const u = new URLSearchParams();
    for (const k of KEYS) if (f[k]) u.set(k, f[k]);
    const qs = u.toString();
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/audit${qs ? `?${qs}` : ''}`);
  };
  const setFilter = (patch) => {
    Object.assign(f, patch);
    if (patch.range !== undefined && patch.range !== 'custom') { f.since = ''; f.until = ''; }
    syncUrl();
    drawFilters();
    restart();
  };
  const clearAll = () => {
    for (const k of KEYS) f[k] = '';
    search.input.value = '';
    syncUrl();
    drawFilters();
    restart();
  };
  const anyFilter = () => KEYS.some((k) => f[k]);

  const search = searchBox({
    placeholder: t('audit.searchPlaceholder'), value: f.q, shortcut: true, testid: 'audit-search', label: t('audit.searchLabel'),
    onInput: debounce((v) => setFilter({ q: v.trim() }), 300),
  });

  const actionOptions = () => {
    const opts = [['', t('audit.filter.allActions')], ...CATEGORIES.map((c) => [c.prefix, t(`audit.cat.${c.id}`)])];
    if (f.action && !opts.some(([v]) => v === f.action)) opts.push([f.action, actionLabel(f.action)]);
    return opts;
  };

  const drawFilters = () => {
    const range = segmented({
      label: t('audit.filter.range'), value: f.range, size: 'sm', testid: 'audit-range',
      options: RANGES.map(([k]) => [k, t(`audit.range.${k || 'all'}`)]),
      onChange: (v) => setFilter({ range: v }),
    });
    const act = select(actionOptions(), f.action, { 'aria-label': t('audit.filter.action'), 'data-testid': 'audit-action' });
    act.addEventListener('change', () => setFilter({ action: act.value }));
    const parts = [search, act];
    if (nodes.length > 1) {
      const ns = select([['', t('audit.filter.allNodes')], ...nodes.map((n) => [n.id, n.name])], f.node, { 'aria-label': t('audit.filter.node'), 'data-testid': 'audit-node' });
      ns.addEventListener('change', () => setFilter({ node: ns.value }));
      parts.push(ns);
    }
    if (nodeAdmin) {
      const ts = select([['', t('audit.filter.myTenant')], ['*', t('audit.filter.allTenants')]], f.tenant === '*' ? '*' : '', { 'aria-label': t('audit.filter.tenant'), 'data-testid': 'audit-tenant' });
      ts.addEventListener('change', () => setFilter({ tenant: ts.value }));
      parts.push(ts);
    }
    let custom = null;
    if (f.range === 'custom') {
      const since = h('input', { type: 'datetime-local', value: f.since ? utcToWall(f.since, zone) : '', 'aria-label': t('audit.filter.since', { zone }), 'data-testid': 'audit-since' });
      const until = h('input', { type: 'datetime-local', value: f.until ? utcToWall(f.until, zone) : '', 'aria-label': t('audit.filter.until', { zone }), 'data-testid': 'audit-until' });
      const apply = () => setFilter({ since: since.value ? wallToUtcIso(since.value, zone) : '', until: until.value ? wallToUtcIso(until.value, zone) : '' });
      since.addEventListener('change', apply);
      until.addEventListener('change', apply);
      custom = h('div', { class: 'filter-row custom-range' },
        h('label', { class: 'inline-field' }, h('span', null, t('audit.filter.from')), since),
        h('span', { class: 'muted' }, '–'),
        h('label', { class: 'inline-field' }, h('span', null, t('audit.filter.to')), until),
        h('span', { class: 'small muted' }, zone));
    }
    replace(filterBox, h('div', { class: 'filter-row' }, parts), h('div', { class: 'filter-row' }, range), custom);
    drawChips();
  };

  const chip = (label, value, onRemove, testid) => h('span', { class: 'filter-chip', 'data-testid': testid },
    h('span', { class: 'fc-label' }, label), h('span', { class: 'fc-value truncate' }, value),
    h('button', { type: 'button', class: 'ghost icon-only sm', 'aria-label': t('audit.filter.remove', { label }), onClick: onRemove }, icon('close')));

  const drawChips = () => {
    const chips = [];
    if (f.subject) chips.push(chip(t('audit.filter.subject'), tokenName(f.subject) || f.subject, () => setFilter({ subject: '' }), 'chip-subject'));
    if (f.resource) chips.push(chip(t('audit.filter.resource'), f.resource.length > 16 ? shortId(f.resource) : f.resource, () => setFilter({ resource: '' }), 'chip-resource'));
    if (f.action && !CATEGORIES.some((c) => c.prefix === f.action)) chips.push(chip(t('audit.filter.action'), actionLabel(f.action), () => setFilter({ action: '' }), 'chip-action'));
    if (anyFilter()) chips.push(button(t('audit.filter.clearAll'), { variant: 'link', size: 'sm', testid: 'audit-clear', onClick: clearAll }));
    replace(chipsBox, chips);
  };

  // ---------------------------------------------------------------- 1 件
  const actorEl = (e) => {
    const tn0 = tokenName(e.subject);
    const tok = subjectTokenId(e.subject);
    const text = tn0 || (tok ? `token:${shortId(tok)}` : e.subject);
    return button(text, {
      variant: 'link', size: 'sm', icon: tok ? 'key' : 'user', class: 'ae-actor',
      attrs: { title: t('audit.filterBy', { what: e.subject }), 'data-testid': 'audit-actor' },
      onClick: () => setFilter({ subject: e.subject }),
    });
  };

  const entryEl = (e) => {
    const c = categoryOf(e.action);
    const name = resourceName(e);
    const href = resourceHref(e.action, e.resource);
    const resText = name || (e.resource ? shortId(e.resource) : '');
    const details = h('div', { class: 'ae-details', hidden: true });
    const toggle = button(t('audit.details'), { variant: 'ghost', size: 'sm', iconOnly: true, icon: 'chevron-down', attrs: { 'aria-expanded': 'false' }, class: 'ae-toggle' });
    const li = h('li', { class: 'audit-entry', 'data-nav-item': '', tabindex: '-1', 'data-action': e.action, 'data-testid': 'audit-entry' },
      h('div', { class: 'ae-row' },
        h('time', { class: 'ae-time num', datetime: e.ts, title: fmtTime(e.ts) }, fmtClock(e.ts)),
        h('span', { class: `ae-icon cat-${c ? c.id : 'other'}` }, icon(c ? c.icon : 'activity')),
        h('div', { class: 'ae-main' },
          h('span', { class: 'ae-action' }, actionLabel(e.action)),
          resText ? h(href ? 'a' : 'span', { class: name ? 'ae-resource' : 'ae-resource is-id', href, title: e.resource }, resText) : null),
        h('div', { class: 'ae-who', 'data-time': fmtClock(e.ts) }, actorEl(e), nodes.length > 1 || f.tenant === '*' ? h('span', { class: 'tag' }, f.tenant === '*' ? `${e.tenant} · ${e.node_name}` : e.node_name) : null),
        toggle),
      details);
    const open = () => {
      const show = details.hidden;
      if (show && !details.childElementCount) {
        const extra = Object.fromEntries(Object.entries(e).filter(([k]) => !BASE_FIELDS.includes(k)));
        details.append(
          kvList([
            [t('audit.field.when'), fmtTime(e.ts)],
            [t('audit.field.action'), h('span', { class: 'row' }, h('code', null, e.action), button(t('audit.onlyThis'), { variant: 'link', size: 'sm', onClick: () => setFilter({ action: e.action }) }))],
            [t('audit.field.resource'), e.resource ? h('span', { class: 'row' }, h('code', { class: 'break' }, e.resource), button(t('audit.onlyThis'), { variant: 'link', size: 'sm', onClick: () => setFilter({ resource: e.resource }) })) : '—'],
            [t('audit.field.subject'), h('span', { class: 'row' }, h('code', { class: 'break' }, e.subject), button(t('audit.onlyThis'), { variant: 'link', size: 'sm', onClick: () => setFilter({ subject: e.subject }) }))],
            [t('audit.field.requestId'), copyField(e.request_id, t('audit.field.requestId'))],
            [t('audit.field.node'), `${e.node_name} (${shortId(e.node)})`],
            [t('audit.field.tenant'), e.tenant],
            Object.keys(extra).length ? [t('audit.field.extra'), h('pre', { class: 'ae-json' }, JSON.stringify(extra, null, 2))] : null,
          ]));
      }
      details.hidden = !show;
      toggle.setAttribute('aria-expanded', String(show));
      li.classList.toggle('open', show);
    };
    toggle.addEventListener('click', open);
    li.open = open;
    return li;
  };

  const dayLabel = (iso) => {
    const d = new Date(iso);
    const today = new Date();
    const y = new Date(Date.now() - 86400000);
    const same = (a, b) => a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
    if (same(d, today)) return t('audit.today');
    if (same(d, y)) return t('audit.yesterday');
    return fmtDate(iso);
  };

  const appendItems = (items) => {
    for (const e of items) {
      const day = new Date(e.ts).toDateString();
      if (day !== state.lastDay) {
        state.lastDay = day;
        list.append(h('li', { class: 'audit-day', role: 'presentation' }, h('h3', null, dayLabel(e.ts))));
      }
      list.append(entryEl(e));
    }
  };

  // ---------------------------------------------------------------- 読み込み
  const drawStatus = () => {
    const n = state.items.length;
    moreBtn.hidden = state.done;
    replace(statusLine,
      state.loading ? loadingLine(t('audit.loading')) : null,
      !state.loading && n ? h('span', { class: 'muted small' }, state.done ? tn('audit.countEnd', n) : tn('audit.countMore', n)) : null);
    const un = [...state.unavailable];
    replace(warnBox, un.length ? alert('warn', t('audit.unavailableBody', { nodes: un.map((id) => (nodes.find((x) => x.id === id) || {}).name || shortId(id)).join(', ') }), {
      title: t('audit.unavailableTitle'), testid: 'audit-unavailable',
      actions: [button(t('common.retry'), { size: 'sm', icon: 'refresh', onClick: () => restart() })],
    }) : null);
  };

  const inView = (el) => {
    const r = el.getBoundingClientRect();
    return r.top < window.innerHeight + 400 && r.bottom > -400;
  };

  const loadMore = async () => {
    if (!state || state.loading || state.done) return;
    const my = gen;
    state.loading = true;
    drawStatus();
    try {
      const r = await auditFill(toQuery(f), { cursor: state.next, want: PAGE_WANT, signal: abort.signal });
      if (my !== gen || disposed) return;
      state.loading = false;
      state.next = r.next;
      state.done = !r.next;
      r.unavailable.forEach((n) => state.unavailable.add(n));
      if (!state.items.length && !r.items.length && state.done) {
        replace(body, anyFilter()
          ? emptyState({ icon: 'filter', title: t('audit.noMatch'), body: t('audit.noMatchBody'), testid: 'audit-nomatch', actions: [button(t('audit.filter.clearAll'), { onClick: clearAll })] })
          : emptyState({ icon: 'audit', title: t('audit.empty'), body: t('audit.emptyBody'), testid: 'audit-empty' }));
        drawStatus();
        return;
      }
      if (!state.items.length) replace(body, list, statusLine, h('div', { class: 'audit-more' }, moreBtn), sentinel);
      state.items.push(...r.items);
      appendItems(r.items);
      drawStatus();
      // 見える分が埋まっていなければ続ける
      if (!state.done && inView(sentinel)) setTimeout(() => loadMore(), 0);
    } catch (e) {
      if (my !== gen || disposed || (e && e.name === 'AbortError')) return;
      state.loading = false;
      if (e.status === 401) return;
      const actions = [button(t('common.retry'), { size: 'sm', icon: 'refresh', onClick: () => restart() })];
      if (e.status === 503 && f.node) actions.unshift(button(t('audit.dropNode'), { size: 'sm', onClick: () => setFilter({ node: '' }) }));
      if (e.status === 403 && f.tenant) actions.unshift(button(t('audit.dropTenant'), { size: 'sm', onClick: () => setFilter({ tenant: '' }) }));
      const box = errorBox(e, { title: e.status === 403 ? t('audit.forbidden') : t('audit.loadFailed'), actions });
      if (!state.items.length) replace(body, box);
      else replace(statusLine, box);
    }
  };

  const restart = () => {
    gen += 1;
    if (abort) abort.abort();
    abort = new AbortController();
    state = { items: [], next: undefined, done: false, loading: false, unavailable: new Set(), lastDay: null };
    replace(list);
    replace(warnBox);
    replace(body, skeleton({ kind: 'lines', rows: 6 }));
    loadMore();
  };

  // ---------------------------------------------------------------- 書き出し
  const exportAs = async (fmt, btn) => {
    const q = toQuery(f);
    const all = [];
    let cursor;
    const unavailable = new Set();
    const label = btn.querySelector('.btn-label');
    const orig = label ? label.textContent : '';
    btn.disabled = true;
    try {
      do {
        const { data } = await auditApi.list({ ...q, limit: 1000, cursor });
        all.push(...data.items);
        (data.unavailable_nodes || []).forEach((n) => unavailable.add(n));
        cursor = data.next_cursor;
        if (label) label.textContent = t('audit.exporting', { n: fmtNumber(all.length) });
      } while (cursor && all.length < MAX_EXPORT && !disposed);
      const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
      if (fmt === 'json') {
        downloadBlob(new Blob([JSON.stringify({ exported_at: new Date().toISOString(), filter: q, truncated: !!cursor, unavailable_nodes: [...unavailable], items: all }, null, 2)], { type: 'application/json' }), `senca-audit-${stamp}.json`);
      } else {
        const extraKeys = [...new Set(all.flatMap((e) => Object.keys(e).filter((k) => !BASE_FIELDS.includes(k))))];
        const head = ['ts', 'action', 'action_label', 'subject', 'subject_name', 'resource', 'resource_name', 'node', 'node_name', 'tenant', 'request_id', ...extraKeys];
        const rows = all.map((e) => [e.ts, e.action, actionLabel(e.action), e.subject, tokenName(e.subject) || '', e.resource, resourceName(e) || '', e.node, e.node_name, e.tenant, e.request_id, ...extraKeys.map((k) => e[k])]);
        const csv = [head, ...rows].map((r) => r.map(csvCell).join(',')).join('\r\n');
        downloadBlob(new Blob(['﻿', csv], { type: 'text/csv;charset=utf-8' }), `senca-audit-${stamp}.csv`);
      }
      toast(cursor ? t('audit.exportedTruncated', { n: fmtNumber(all.length) }) : tn('audit.exported', all.length), cursor ? 'warn' : 'ok');
    } catch (e) {
      reportError(e, t('audit.export'));
    } finally {
      btn.disabled = false;
      if (label) label.textContent = orig;
    }
  };

  const exportBtn = button(t('audit.export'), { icon: 'download', iconRight: 'chevron-down', testid: 'audit-export' });
  attachMenu(exportBtn, [
    { label: t('audit.exportCsv'), icon: 'download', testid: 'audit-export-csv', onSelect: () => exportAs('csv', exportBtn) },
    { label: t('audit.exportJson'), icon: 'json', testid: 'audit-export-json', onSelect: () => exportAs('json', exportBtn) },
  ]);

  root.append(
    pageHeader({
      title: t('nav.audit'),
      description: t('audit.lead'),
      help: helpHint({ title: t('nav.audit'), body: t('audit.help'), guide: '12-security.md' }),
      actions: [exportBtn, button(t('common.refresh'), { iconOnly: true, icon: 'refresh', onClick: () => restart() })],
    }),
    filterBox,
    chipsBox,
    warnBox,
    panel({ testid: 'audit-panel', class: 'audit-panel' }, body));

  // 名前とノードを先に読む（失敗してもかまわない）
  const [nm, ns] = await Promise.all([
    loadNames(['stream', 'recording', 'schedule', 'token', 'secret', 'pipeline', 'node']),
    system.nodes().then((r) => r.data.items).catch(() => null),
  ]);
  if (disposed) return null;
  lookup = nm;
  nodeAdmin = !!ns;
  nodes = (ns || []).map((n) => ({ id: n.id, name: n.name }));
  drawFilters();
  restart();

  const io = new IntersectionObserver((es) => { if (es.some((x) => x.isIntersecting)) loadMore(); }, { rootMargin: '400px 0px' });
  io.observe(sentinel);
  const offNav = listNav(list, { selector: '.audit-entry', onOpen: (el) => el.open && el.open() });
  // 新しい操作が届いたら、先頭から読み直す（読み進めていないときだけ）
  const offLive = refreshOn(['stream.', 'recording.', 'schedule.', 'token.', 'secret.', 'input.', 'output.', 'pipeline.', 'node.updated', 'device.disabled', 'device.enabled', 'room.updated', 'room.deleted'], () => {
    if (state && state.items.length <= PAGE_WANT && window.scrollY < 200 && !list.querySelector('.audit-entry.open')) restart();
  }, { wait: 1500 });
  return () => {
    disposed = true;
    if (abort) abort.abort();
    io.disconnect();
    offNav();
    offLive();
  };
}
