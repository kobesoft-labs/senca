// コマンドパレット（Ctrl/⌘+K）: 画面・よく使う操作・資源（配信・録画・予約・パイプライン）へ、名前の一部で飛ぶ。
// 入力欄は role="combobox"、候補は role="listbox"（↑↓で選び、Enter で実行、Escape で閉じる）。
// 資源は開いたときに API から読む（60 秒は使い回す）。最近開いたものは lib/recent.js。

import { h, replace } from './lib/dom.js';
import { icon } from './lib/icons.js';
import { t, getLang, setLang } from './i18n/index.js';
import { fuzzyFilter } from './lib/fuzzy.js';
import { NAV_ITEMS } from './nav.js';
import { recentItems } from './lib/recent.js';
import { setThemePref } from './lib/theme.js';
import { streams, recordings, schedules, listAll } from './api/resources.js';
import { pipelines } from './api/pipelines.js';
import { MOD, showShortcutHelp } from './shortcuts.js';
import { trapFocus } from './ui/overlay.js';
import { fmtTime } from './lib/format.js';

const KIND_ICON = { stream: 'streams', recording: 'recordings', schedule: 'schedules', pipeline: 'pipelines', page: 'arrow-right' };
let cache = { at: 0, items: [] };
let openDlg = null;

async function loadResources() {
  if (Date.now() - cache.at < 60000) return cache.items;
  const settle = async (p) => { try { return await p; } catch { return null; } };
  const [st, rec, sch, pl] = await Promise.all([
    settle(listAll(streams.list)),
    settle(recordings.list({ limit: 50 })),
    settle(listAll(schedules.list)),
    settle(pipelines.list().then((r) => r.data)),
  ]);
  const items = [];
  for (const s of st || []) {
    items.push({ id: `stream:${s.id}`, kind: 'stream', label: s.name, hint: s.observed_state === 'live' ? t('status.live') : t('palette.kind.stream'), href: `#/streams/${encodeURIComponent(s.id)}`, keywords: [s.id], boost: s.observed_state === 'live' ? 30 : 0 });
  }
  for (const r of (rec && rec.data.items) || []) {
    items.push({ id: `recording:${r.id}`, kind: 'recording', label: r.name || fmtTime(r.started_at), hint: t('palette.kind.recording'), href: `#/recordings/${encodeURIComponent(r.id)}`, keywords: [r.id] });
  }
  for (const s of sch || []) {
    items.push({ id: `schedule:${s.id}`, kind: 'schedule', label: s.name || s.id.slice(0, 8), hint: t('palette.kind.schedule'), href: `#/schedules/${encodeURIComponent(s.id)}`, keywords: [s.id] });
  }
  for (const p of (pl && pl.items) || []) {
    items.push({ id: `pipeline:${p.id}`, kind: 'pipeline', label: p.name, hint: t('palette.kind.pipeline'), href: `#/pipelines?id=${encodeURIComponent(p.id)}`, keywords: [p.id] });
  }
  cache = { at: Date.now(), items };
  return items;
}

/** 資源の候補を捨てる（作成・削除の後に）。 */
export function invalidatePalette() {
  cache = { at: 0, items: [] };
}

function staticItems({ navigate, logout }) {
  const go = (href) => () => navigate(href);
  const pages = NAV_ITEMS.map((n) => ({
    id: `page:${n.id}`, kind: 'page', icon: n.icon, label: t(`nav.${n.id}`), hint: n.soon ? t('nav.soon') : t(`navGroup.${n.group}`),
    keywords: [n.id, t(`navKeywords.${n.id}`)], run: go(n.href), group: 'pages',
  }));
  const other = getLang() === 'ja' ? 'en' : 'ja';
  const actions = [
    { id: 'act:new-stream', icon: 'plus', label: t('palette.newStream'), keywords: ['new stream', 'create'], run: go('#/streams?new=1') },
    { id: 'act:new-schedule', icon: 'calendar', label: t('palette.newSchedule'), keywords: ['new schedule'], run: go('#/schedules?new=1') },
    { id: 'act:new-token', icon: 'key', label: t('palette.newToken'), keywords: ['new token'], run: go('#/tokens?new=1') },
    { id: 'act:new-secret', icon: 'secrets', label: t('palette.newSecret'), keywords: ['secret', 'new secret'], run: go('#/secrets?new=1') },
    { id: 'act:onboarding', icon: 'rocket', label: t('palette.onboarding'), keywords: ['getting started', 'onboarding', 'checklist'], run: go('#/?onboarding=1') },
    { id: 'act:grid-unplaced', icon: 'ban', label: t('palette.gridUnplaced'), keywords: ['grid', 'unschedulable', 'pending'], run: go('#/grid?state=unschedulable') },
    { id: 'act:audit-24h', icon: 'audit', label: t('palette.audit24h'), keywords: ['audit', 'log', 'today'], run: go('#/audit?range=24h') },
    { id: 'act:tokens-expiring', icon: 'clock', label: t('palette.tokensExpiring'), keywords: ['token', 'expire'], run: go('#/tokens?filter=expiring') },
    { id: 'act:new-pipeline', icon: 'pipelines', label: t('palette.newPipeline'), keywords: ['pipeline'], run: go('#/pipelines') },
    { id: 'act:send', icon: 'camera', label: t('palette.sendFromBrowser'), keywords: ['whip', 'camera'], run: go('#/send') },
    { id: 'act:live-streams', icon: 'live', label: t('palette.liveStreams'), keywords: ['live', 'on air', 'streams'], run: go('#/streams?state=live') },
    { id: 'act:upcoming', icon: 'calendar', label: t('palette.upcoming'), keywords: ['schedule', 'calendar', 'upcoming'], run: go('#/schedules') },
    { id: 'act:legal-hold', icon: 'shield', label: t('palette.legalHold'), keywords: ['legal hold', 'recordings'], run: go('#/recordings?state=held') },
    { id: 'act:theme-light', icon: 'sun', label: t('palette.themeLight'), keywords: ['theme', 'light'], run: () => setThemePref('light') },
    { id: 'act:theme-dark', icon: 'moon', label: t('palette.themeDark'), keywords: ['theme', 'dark'], run: () => setThemePref('dark') },
    { id: 'act:theme-system', icon: 'monitor', label: t('palette.themeSystem'), keywords: ['theme', 'system'], run: () => setThemePref('system') },
    { id: 'act:lang', icon: 'language', label: t('palette.switchLang', { lang: other === 'ja' ? '日本語' : 'English' }), keywords: ['language', 'english', 'japanese', '言語'], run: () => setLang(other) },
    { id: 'act:shortcuts', icon: 'keyboard', label: t('palette.shortcuts'), keywords: ['keyboard', 'help', '?'], run: () => showShortcutHelp() },
    { id: 'act:styleguide', icon: 'layout-grid', label: t('palette.styleguide'), keywords: ['style guide', 'components'], run: go('#/_styleguide'), boost: -40 },
    { id: 'act:logout', icon: 'logout', label: t('palette.logout'), keywords: ['logout', 'sign out'], run: logout, boost: -20 },
  ].map((a) => ({ ...a, kind: 'action', group: 'actions' }));
  return { pages, actions };
}

function highlight(label, positions) {
  if (!positions || !positions.length) return label;
  const set = new Set(positions);
  const out = [];
  let buf = '';
  let inMark = false;
  [...label].forEach((ch, i) => {
    const m = set.has(i);
    if (m !== inMark) {
      if (buf) out.push(inMark ? h('mark', null, buf) : buf);
      buf = '';
      inMark = m;
    }
    buf += ch;
  });
  if (buf) out.push(inMark ? h('mark', null, buf) : buf);
  return out;
}

/** パレットを開く（開いていれば閉じる）。 */
export function openPalette({ navigate, logout }) {
  if (openDlg) { openDlg.close(); return; }
  const opener = document.activeElement;
  const { pages, actions } = staticItems({ navigate, logout });
  let resources = [];
  let flat = [];
  let active = 0;
  const listId = 'cmdk-list';
  const input = h('input', {
    class: 'cmdk-input', type: 'text', role: 'combobox', 'aria-expanded': 'true', 'aria-controls': listId, 'aria-autocomplete': 'list',
    autocomplete: 'off', spellcheck: 'false', placeholder: t('palette.placeholder'), 'aria-label': t('palette.label'), 'data-testid': 'palette-input',
  });
  const list = h('div', { class: 'cmdk-list', id: listId, role: 'listbox', 'aria-label': t('palette.results') });
  const status = h('div', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
  const dlg = h('dialog', { class: 'cmdk', 'aria-label': t('palette.label'), 'data-testid': 'palette' },
    h('div', { class: 'cmdk-input-row' }, icon('search'), input, h('kbd', null, 'Esc')),
    list,
    h('div', { class: 'cmdk-foot', 'aria-hidden': 'true' },
      h('span', null, h('kbd', null, '↑'), h('kbd', null, '↓'), t('palette.footMove')),
      h('span', null, h('kbd', null, 'Enter'), t('palette.footOpen')),
      h('span', null, h('kbd', null, MOD), h('kbd', null, 'K'), t('palette.footToggle'))),
    status);

  const close = () => {
    if (!openDlg) return;
    openDlg = null;
    dlg.close();
    dlg.remove();
    if (opener && opener.isConnected && typeof opener.focus === 'function') opener.focus();
  };
  const run = (it) => {
    close();
    if (it.run) it.run();
    else if (it.href) navigate(it.href);
  };
  const setActive = (i) => {
    const opts = list.querySelectorAll('[role="option"]');
    if (!opts.length) { input.removeAttribute('aria-activedescendant'); return; }
    active = (i + opts.length) % opts.length;
    opts.forEach((o, k) => o.setAttribute('aria-selected', String(k === active)));
    input.setAttribute('aria-activedescendant', opts[active].id);
    opts[active].scrollIntoView({ block: 'nearest' });
  };
  const draw = () => {
    const q = input.value;
    const recent = q.trim() ? [] : recentItems().map((r) => ({ id: `recent:${r.href}`, kind: r.kind, label: r.label, hint: t(`palette.kind.${r.kind}`), href: r.href, group: 'recent' }));
    // 打ち込んだら、画面と資源（行き先）を先に、操作を後に
    const groups = q.trim()
      ? [['pages', fuzzyFilter(q, pages, 8)], ['resources', fuzzyFilter(q, resources, 12)], ['actions', fuzzyFilter(q, actions, 6)]]
      : [['recent', recent], ['actions', fuzzyFilter(q, actions, 4)], ['pages', fuzzyFilter(q, pages, 20)]];
    flat = [];
    const nodes = [];
    for (const [g, items] of groups) {
      if (!items.length) continue;
      nodes.push(h('div', { class: 'cmdk-group', role: 'presentation' }, t(`palette.group.${g}`)));
      for (const it of items) {
        const idx = flat.length;
        flat.push(it);
        nodes.push(h('div', {
          role: 'option', id: `cmdk-opt-${idx}`, 'aria-selected': 'false', 'data-id': it.id,
          onMousemove: () => { if (active !== idx) setActive(idx); },
          onClick: () => run(it),
        }, icon(it.icon || KIND_ICON[it.kind] || 'arrow-right'), h('span', { class: 'opt-label' }, highlight(it.label, it.positions)), it.hint ? h('span', { class: 'opt-hint' }, it.hint) : null));
      }
    }
    if (!flat.length) nodes.push(h('div', { class: 'cmdk-empty' }, t('palette.empty', { q })));
    replace(list, nodes);
    status.textContent = flat.length ? t('palette.count', { n: flat.length }) : t('palette.empty', { q });
    setActive(0);
  };
  input.addEventListener('input', draw);
  input.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); setActive(active + 1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); setActive(active - 1); }
    else if (e.key === 'Home' && e.ctrlKey) { e.preventDefault(); setActive(0); }
    else if (e.key === 'Enter') { e.preventDefault(); if (flat[active]) run(flat[active]); }
  });
  dlg.addEventListener('cancel', (e) => { e.preventDefault(); close(); });
  dlg.addEventListener('mousedown', (e) => {
    if (e.target !== dlg) return;
    const r = dlg.getBoundingClientRect();
    if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) close();
  });
  document.body.appendChild(dlg);
  dlg.showModal();
  trapFocus(dlg);
  openDlg = { close };
  draw();
  input.focus();
  loadResources().then((items) => {
    resources = items;
    if (openDlg && input.value.trim()) draw();
  });
}
