// 外枠: 左のメニュー（グループ・アイコン・畳める・狭い画面では引き出し）と上の帯
// （パンくず・検索・接続の状態・言語・明暗・利用者のメニュー）。画面の中身は <main id="main">。

import { h, replace } from './lib/dom.js';
import { icon } from './lib/icons.js';
import { on } from './lib/bus.js';
import { t, getLang, setLang, LANGS } from './i18n/index.js';
import { getThemePref, setThemePref, resolvedTheme } from './lib/theme.js';
import { attachMenu, tooltip } from './ui/overlay.js';
import { breadcrumbs } from './ui/layout.js';
import { NAV_GROUPS, navItem } from './nav.js';
import { MOD, showShortcutHelp } from './shortcuts.js';
import { getToken } from './api/client.js';
import { request } from './api/client.js';
import { guideUrl } from './lib/guide.js';

const SIDEBAR_STORE = 'senca.console.sidebar';
const narrow = () => globalThis.matchMedia && globalThis.matchMedia('(max-width: 768px)').matches;

/** トークンから分かる利用者の情報（JWT なら claim、API トークンなら末尾 4 文字だけ）。 */
export function identity(tok = getToken()) {
  if (!tok) return null;
  const parts = tok.split('.');
  if (parts.length === 3) {
    try {
      const json = JSON.parse(atob(parts[1].replace(/-/g, '+').replace(/_/g, '/')));
      const scopes = Array.isArray(json.scopes) ? json.scopes : typeof json.scope === 'string' ? json.scope.split(/\s+/).filter(Boolean) : null;
      return {
        kind: 'jwt', name: json.name || json.preferred_username || json.email || json.sub || 'JWT',
        tenant: json.tenant || json.tenant_id || json.org || null, scopes, exp: json.exp ? new Date(json.exp * 1000) : null,
      };
    } catch { /* API トークンとして扱う */ }
  }
  return { kind: 'token', name: `senca_…${tok.slice(-4)}`, tenant: null, scopes: null };
}

let probed = null;
/** 権限を確かめる（JWT の claim に無いとき。メニューを初めて開いたときに 1 回）。 */
async function probeScopes() {
  if (probed) return probed;
  const ok = async (p) => { try { await request('GET', p, { query: { limit: 1 } }); return true; } catch { return false; } };
  const [admin, node] = await Promise.all([ok('/tokens'), ok('/nodes')]);
  let scopes = [];
  if (admin) scopes.push('tenant:admin');
  else {
    const [sr, rr] = await Promise.all([ok('/streams'), ok('/recordings')]);
    if (sr) scopes.push('stream:read');
    if (rr) scopes.push('recording:read');
  }
  if (node) scopes.push('node:admin');
  probed = scopes;
  return scopes;
}

/**
 * @param {{caps: object, navigate: (href: string) => void, logout: () => void, palette: () => void}} o
 * @returns {{main: HTMLElement, setActive: (nav: string|null) => void, setCrumbs: (items) => void, toggleSidebar: () => void, closeDrawer: () => void, destroy: () => void}}
 */
export function buildShell(root, { caps, logout, palette }) {
  probed = null;
  const offs = [];
  const shell = h('div', { class: 'shell', 'data-sidebar': localStorage.getItem(SIDEBAR_STORE) === 'collapsed' ? 'collapsed' : 'expanded', 'data-drawer': 'closed' });

  // ---------------------------------------------------------------- 左のメニュー
  const links = [];
  const navEl = h('nav', { class: 'nav', id: 'nav', 'aria-label': t('shell.mainNav') },
    NAV_GROUPS.map((g) => h('div', { class: 'nav-group', role: 'group', 'aria-labelledby': g.id === 'overview' ? null : `ng-${g.id}`, 'aria-label': g.id === 'overview' ? t(`navGroup.${g.id}`) : null },
      g.id === 'overview' ? null : h('span', { class: 'nav-group-label', id: `ng-${g.id}` }, t(`navGroup.${g.id}`)),
      h('ul', null, g.items.map((it) => {
        const a = h('a', { href: it.href, dataset: { nav: it.id }, 'data-testid': `nav-${it.id}` },
          icon(it.icon), h('span', { class: 'nav-label' }, t(`nav.${it.id}`)),
          it.soon ? h('span', { class: 'tag' }, t('nav.soon')) : null);
        tooltip(a, () => (shell.dataset.sidebar === 'collapsed' && !narrow() ? t(`nav.${it.id}`) : null));
        links.push(a);
        return h('li', null, a);
      })))));
  const collapseBtn = h('button', { type: 'button', class: 'ghost icon-only sm collapse-btn', 'data-testid': 'sidebar-collapse' });
  const syncCollapse = () => {
    const collapsed = shell.dataset.sidebar === 'collapsed';
    collapseBtn.setAttribute('aria-label', collapsed ? t('shell.expandSidebar') : t('shell.collapseSidebar'));
    collapseBtn.setAttribute('aria-expanded', String(!collapsed));
    replace(collapseBtn, icon(collapsed ? 'sidebar-open' : 'sidebar-close'));
  };
  tooltip(collapseBtn, () => collapseBtn.getAttribute('aria-label'), { kbd: '[' });
  const toggleSidebar = () => {
    if (narrow()) { setDrawer(shell.dataset.drawer !== 'open'); return; }
    shell.dataset.sidebar = shell.dataset.sidebar === 'collapsed' ? 'expanded' : 'collapsed';
    try { localStorage.setItem(SIDEBAR_STORE, shell.dataset.sidebar); } catch { /* 残せなくてもよい */ }
    syncCollapse();
  };
  collapseBtn.addEventListener('click', toggleSidebar);
  syncCollapse();
  const sidebar = h('aside', { class: 'sidebar', id: 'sidebar', 'aria-label': t('shell.sidebar') },
    h('a', { class: 'brand', href: '#/', 'aria-label': t('shell.home') },
      h('span', { class: 'brand-mark', 'aria-hidden': 'true' }, 'S'),
      h('span', { class: 'brand-text' }, h('strong', null, 'SENCA'), h('small', null, t('shell.product')))),
    navEl,
    h('div', { class: 'sidebar-foot' },
      h('span', { class: 'node-id', title: caps ? caps.node : '' }, t('shell.node'), ' ', h('code', null, caps ? caps.node.slice(0, 12) : '—')),
      collapseBtn));
  const scrim = h('div', { class: 'scrim', 'aria-hidden': 'true' });

  // ---------------------------------------------------------------- 上の帯
  const menuBtn = h('button', { type: 'button', class: 'ghost icon-only menu-btn', 'aria-controls': 'sidebar', 'aria-expanded': 'false', 'aria-label': t('shell.openMenu'), 'data-testid': 'menu-button' }, icon('menu'));
  const crumbBox = h('div', { class: 'crumbs-host' });
  crumbBox.style.flex = '1';
  crumbBox.style.minWidth = '0';
  const searchBtn = h('button', { type: 'button', class: 'cmdk-trigger', 'aria-label': t('shell.search'), 'data-testid': 'palette-trigger', onClick: palette },
    icon('search'), h('span', { class: 'cmdk-text' }, t('shell.searchHint')), h('kbd', null, MOD), h('kbd', null, 'K'));
  const connDot = h('span', { class: 'dot warn', 'aria-hidden': 'true' });
  const connText = h('span', { class: 'conn-text' }, t('sse.connecting'));
  const conn = h('div', { class: 'conn', role: 'status', 'data-testid': 'sse-status', 'data-state': 'connecting', tabindex: '0', 'aria-label': t('sse.connecting') }, connDot, connText);
  let connDetail = t('sse.connectingDetail');
  tooltip(conn, () => connDetail);

  const langBtn = h('button', { type: 'button', class: 'ghost icon-only', 'aria-label': t('shell.language'), 'data-testid': 'lang-button' }, icon('language'));
  tooltip(langBtn, t('shell.language'));
  attachMenu(langBtn, () => [
    { heading: t('shell.language') },
    ...LANGS.map(([code, name]) => ({ label: name, checked: getLang() === code, testid: `lang-${code}`, onSelect: () => setLang(code) })),
  ]);
  const themeBtn = h('button', { type: 'button', class: 'ghost icon-only', 'aria-label': t('shell.theme'), 'data-testid': 'theme-button' });
  const syncThemeIcon = () => replace(themeBtn, icon(getThemePref() === 'system' ? 'monitor' : resolvedTheme() === 'dark' ? 'moon' : 'sun'));
  syncThemeIcon();
  tooltip(themeBtn, t('shell.theme'));
  attachMenu(themeBtn, () => [
    { heading: t('shell.theme') },
    ...[['system', 'monitor'], ['light', 'sun'], ['dark', 'moon']].map(([v, ic]) => ({ label: t(`theme.${v}`), icon: ic, checked: getThemePref() === v, testid: `theme-${v}`, onSelect: () => setThemePref(v) })),
  ]);
  offs.push(on('theme', syncThemeIcon));

  const who = identity();
  const userBtn = h('button', { type: 'button', class: 'user-btn', 'aria-label': t('shell.account'), 'data-testid': 'user-menu' },
    h('span', { class: 'avatar', 'aria-hidden': 'true' }, icon('user')), icon('chevron-down', { size: 'sm' }));
  tooltip(userBtn, t('shell.account'));
  attachMenu(userBtn, async () => {
    const scopes = (who && who.scopes) || await probeScopes();
    return [
      { node: h('div', { class: 'stack', 'data-testid': 'identity' },
        h('div', { class: 'row' }, h('span', { class: 'avatar' }, icon('user')),
          h('div', { class: 'stack', style: { gap: '0' } },
            h('strong', null, who ? who.name : '—'),
            h('span', { class: 'small muted' }, who && who.kind === 'jwt' ? t('identity.jwt') : t('identity.apiToken')))),
        h('div', { class: 'small muted' }, t('identity.tenant'), ': ', who && who.tenant ? h('code', null, who.tenant) : t('identity.tenantFromToken')),
        h('div', { class: 'small muted' }, t('identity.node'), ': ', h('code', null, caps ? caps.node.slice(0, 12) : '—')),
        h('div', { class: 'row', style: { gap: '4px' } }, scopes.length
          ? scopes.map((s) => h('span', { class: 'tag' }, s))
          : h('span', { class: 'small muted' }, t('identity.noScopes')))) },
      { separator: true },
      { label: t('shell.shortcuts'), icon: 'keyboard', kbd: '?', onSelect: () => showShortcutHelp() },
      { label: t('shell.guide'), icon: 'book', onSelect: () => window.open(guideUrl('README.md'), '_blank', 'noopener,noreferrer') },
      { separator: true },
      { label: t('shell.logout'), icon: 'logout', onSelect: logout, testid: 'logout' },
    ];
  }, { label: t('shell.account') });

  const topbar = h('header', { class: 'topbar' },
    menuBtn, crumbBox,
    h('div', { class: 'tb-actions' }, searchBtn, conn, langBtn, themeBtn, userBtn));
  const main = h('main', { id: 'main', class: 'main', tabindex: '-1' });
  const workspace = h('div', { class: 'workspace' }, topbar, main);
  shell.append(sidebar, scrim, workspace);
  replace(root, shell);

  // ---------------------------------------------------------------- 引き出し（狭い画面）
  const setDrawer = (open) => {
    shell.dataset.drawer = open ? 'open' : 'closed';
    menuBtn.setAttribute('aria-expanded', String(open));
    menuBtn.setAttribute('aria-label', open ? t('shell.closeMenu') : t('shell.openMenu'));
    workspace.inert = open;
    if (open) {
      const cur = navEl.querySelector('[aria-current="page"]') || navEl.querySelector('a');
      if (cur) cur.focus();
    } else if (narrow()) menuBtn.focus();
  };
  menuBtn.addEventListener('click', () => setDrawer(true));
  scrim.addEventListener('click', () => setDrawer(false));
  sidebar.addEventListener('keydown', (e) => { if (e.key === 'Escape' && shell.dataset.drawer === 'open') { e.stopPropagation(); setDrawer(false); } });
  navEl.addEventListener('click', (e) => { if (e.target.closest('a') && shell.dataset.drawer === 'open') { shell.dataset.drawer = 'closed'; workspace.inert = false; menuBtn.setAttribute('aria-expanded', 'false'); } });
  const mq = globalThis.matchMedia('(max-width: 768px)');
  const onMq = () => { if (!mq.matches && shell.dataset.drawer === 'open') { shell.dataset.drawer = 'closed'; workspace.inert = false; } };
  mq.addEventListener('change', onMq);
  offs.push(() => mq.removeEventListener('change', onMq));

  // ---------------------------------------------------------------- 接続の状態
  offs.push(on('sse-status', (s) => {
    const map = {
      connecting: ['warn', 'sse.connecting', 'sse.connectingDetail'],
      open: ['ok', 'sse.open', 'sse.openDetail'],
      retry: ['warn', 'sse.retry', 'sse.retryDetail'],
      closed: ['', 'sse.closed', 'sse.closedDetail'],
      forbidden: ['danger', 'sse.forbidden', 'sse.forbiddenDetail'],
    };
    const [cls, key, detail] = map[s.state] || ['', 'sse.closed', 'sse.closedDetail'];
    connDot.className = `dot ${cls}`;
    const text = t(key, { s: Math.round((s.retryIn || 0) / 1000) });
    connText.textContent = text;
    conn.dataset.state = s.state;
    conn.setAttribute('aria-label', text);
    connDetail = t(detail, { s: Math.round((s.retryIn || 0) / 1000) });
  }));

  // ---------------------------------------------------------------- パンくず
  let defaultCrumbs = [];
  let activeItem = null;
  const setCrumbs = (items) => replace(crumbBox, breadcrumbs(items));
  // 画面が渡すパンくず（pageHeader の crumbs）の前に、グループの名前を足す
  const withGroup = (items) => {
    if (!activeItem || activeItem.group === 'overview') return items;
    const g = t(`navGroup.${activeItem.group}`);
    // グループと画面が同じ名前（配信 › 配信）なら重ねない
    return items.length && items[0].label === g ? items : [{ label: g }, ...items];
  };
  offs.push(on('crumbs', (items) => setCrumbs(withGroup(items))));

  const setActive = (navId) => {
    for (const a of links) {
      if (a.dataset.nav === navId) a.setAttribute('aria-current', 'page');
      else a.removeAttribute('aria-current');
    }
    const it = navId ? navItem(navId) : null;
    activeItem = it;
    // グループの名前（リンクなし）› 画面。概要（ダッシュボード）はグループを出さない
    defaultCrumbs = it ? withGroup([{ label: t(`nav.${it.id}`), href: it.href }]) : [];
    setCrumbs(defaultCrumbs);
  };

  return {
    main,
    setActive,
    setCrumbs,
    toggleSidebar,
    closeDrawer: () => { if (shell.dataset.drawer === 'open') setDrawer(false); },
    destroy: () => offs.forEach((off) => off()),
  };
}
