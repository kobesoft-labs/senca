// SENCA 管理画面の入口。ハッシュの経路（#/streams など）で画面を切り替える。
// 使う API は公開の /api/v1 だけ（REQ-002）。トークンは sessionStorage にだけ置く。

import { h, replace } from './lib/dom.js';
import { on } from './lib/bus.js';
import { toast } from './lib/ui.js';
import { getToken, clearToken } from './api/client.js';
import { system } from './api/resources.js';
import { EventStream } from './api/sse.js';
import { renderLogin } from './views/login.js';
import './lib/activity.js';

const ROUTES = [
  { re: /^\/?$/, nav: 'dashboard', load: () => import('./views/dashboard.js') },
  { re: /^\/streams\/?$/, nav: 'streams', load: () => import('./views/streams.js') },
  { re: /^\/streams\/([^/]+)$/, nav: 'streams', load: () => import('./views/stream-detail.js') },
  { re: /^\/recordings\/?$/, nav: 'recordings', load: () => import('./views/recordings.js') },
  { re: /^\/recordings\/([^/]+)$/, nav: 'recordings', load: () => import('./views/recording-detail.js') },
  { re: /^\/schedules\/?$/, nav: 'schedules', load: () => import('./views/schedules.js') },
  { re: /^\/schedules\/([^/]+)$/, nav: 'schedules', load: () => import('./views/schedule-detail.js') },
  { re: /^\/tokens\/?$/, nav: 'tokens', load: () => import('./views/tokens.js') },
  { re: /^\/nodes\/?$/, nav: 'nodes', load: () => import('./views/nodes.js') },
  { re: /^\/pipelines\/?$/, nav: 'pipelines', load: () => import('./views/pipelines.js') },
  { re: /^\/production\/?$/, nav: 'production', load: () => import('./views/production.js') },
  { re: /^\/secrets\/?$/, nav: 'secrets', load: () => import('./views/secrets.js') },
  { re: /^\/send\/?$/, nav: 'send', load: () => import('./views/send.js') },
];

const NAV = [
  ['dashboard', '#/', 'ダッシュボード'],
  ['streams', '#/streams', '配信'],
  ['recordings', '#/recordings', '録画'],
  ['schedules', '#/schedules', '予約'],
  null,
  ['send', '#/send', 'ブラウザから送る'],
  null,
  ['pipelines', '#/pipelines', 'パイプライン'],
  ['production', '#/production', 'スイッチャー'],
  null,
  ['secrets', '#/secrets', '秘密値'],
  ['nodes', '#/nodes', 'ノードとデバイス'],
  ['tokens', '#/tokens', 'API トークン'],
];

const state = {
  caps: null,
  sse: null,
  cleanup: null,
  renderSeq: 0,
  shell: null,
  offSse: null,
};

export function navigate(path) {
  if (location.hash !== `#${path}`) location.hash = path;
  else route();
}

function buildShell() {
  const app = document.getElementById('app');
  app.classList.remove('login-mode');
  const main = h('main', { id: 'main', class: 'main', tabindex: '-1' });
  const sseDot = h('span', { class: 'dot', 'aria-hidden': 'true' });
  const sseText = h('span', null, '接続中');
  const menuBtn = h('button', { class: 'menu-toggle small', 'aria-expanded': 'false', 'aria-controls': 'nav' }, 'メニュー');
  const navList = h('ul', { class: 'nav', id: 'nav' }, NAV.map((n) => (n
    ? h('li', null, h('a', { href: n[1], dataset: { nav: n[0] } }, n[2], n[3] ? h('span', { class: 'tag' }, n[3]) : null))
    : h('li', { class: 'sep', role: 'separator' }))));
  const sidebar = h('nav', { class: 'sidebar', 'aria-label': 'メインメニュー' },
    h('div', { class: 'brand' }, 'SENCA', h('small', null, '管理画面')),
    menuBtn,
    navList,
    h('div', { class: 'sidebar-foot' },
      h('div', { class: 'row', title: 'SSE（イベント購読）の状態' }, sseDot, sseText),
      h('div', { class: 'small break' }, 'ノード: ', h('code', null, state.caps ? state.caps.node.slice(0, 12) : '—')),
      h('button', { class: 'small', onClick: logout }, 'ログアウト')));
  menuBtn.addEventListener('click', () => {
    const open = sidebar.classList.toggle('open');
    menuBtn.setAttribute('aria-expanded', String(open));
  });
  navList.addEventListener('click', (e) => {
    if (e.target.closest('a')) sidebar.classList.remove('open');
  });
  replace(app, sidebar, main);
  if (state.offSse) state.offSse();
  state.offSse = on('sse-status', (s) => {
    const map = {
      connecting: ['warn', '接続中'], open: ['ok', 'イベント受信中'], retry: ['warn', `再接続待ち（${Math.round((s.retryIn || 0) / 1000)} 秒）`],
      closed: ['', '停止'], forbidden: ['danger', 'イベントの権限なし'],
    };
    const [cls, text] = map[s.state] || ['', s.state];
    sseDot.className = `dot ${cls}`;
    sseText.textContent = text;
  });
  state.shell = { main, navList };
}

async function route() {
  if (!getToken()) {
    showLogin();
    return;
  }
  if (!state.shell) buildShell();
  const path = location.hash.replace(/^#/, '').split('?')[0] || '/';
  const r = ROUTES.find((x) => x.re.test(path));
  const seq = ++state.renderSeq;
  if (state.cleanup) {
    try { state.cleanup(); } catch { /* 何もしない */ }
    state.cleanup = null;
  }
  const { main, navList } = state.shell;
  for (const a of navList.querySelectorAll('a')) {
    if (r && a.dataset.nav === r.nav) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
  if (!r) {
    replace(main, h('h1', null, '見つかりません'), h('p', null, h('a', { href: '#/' }, 'ダッシュボードへ')));
    return;
  }
  const params = path.match(r.re).slice(1).map(decodeURIComponent);
  replace(main, h('div', { class: 'muted' }, '読み込み中…'));
  try {
    const mod = await r.load();
    if (seq !== state.renderSeq) return;
    replace(main);
    const cleanup = await mod.render(main, params, { caps: state.caps, navigate });
    if (seq !== state.renderSeq) {
      if (typeof cleanup === 'function') cleanup();
      return;
    }
    state.cleanup = typeof cleanup === 'function' ? cleanup : null;
    const h1 = main.querySelector('h1');
    document.title = `${h1 ? h1.textContent : 'SENCA'} — SENCA 管理画面`;
  } catch (e) {
    replace(main, h('div', { class: 'alert error' }, `画面を開けませんでした: ${e.message}`));
  }
}

function showLogin(message) {
  if (state.cleanup) {
    try { state.cleanup(); } catch { /* 何もしない */ }
    state.cleanup = null;
  }
  if (state.sse) state.sse.stop();
  state.sse = null;
  state.shell = null;
  const app = document.getElementById('app');
  app.classList.add('login-mode');
  renderLogin(app, { message, onLogin: (caps) => { state.caps = caps; startSession(); } });
  document.title = 'ログイン — SENCA 管理画面';
}

function startSession() {
  state.sse = new EventStream();
  buildShell();
  state.sse.start();
  route();
}

function logout() {
  clearToken();
  state.caps = null;
  showLogin('ログアウトしました。');
}

on('unauthorized', () => {
  if (!getToken()) return;
  clearToken();
  state.caps = null;
  toast('認証が切れました。トークンを入れ直してください', 'warn');
  showLogin('認証に失敗しました（トークンが無効・期限切れ・失効）。');
});

window.addEventListener('hashchange', () => {
  if (state.shell) route();
});

async function boot() {
  if (!getToken()) {
    showLogin();
    return;
  }
  try {
    const { data } = await system.capabilities({ quiet401: true });
    state.caps = data;
    startSession();
  } catch (e) {
    if (e.status === 401) {
      clearToken();
      showLogin('保存していたトークンは使えません。');
    } else {
      showLogin(`ノードに接続できません: ${e.title || e.message}`);
    }
  }
}

boot();
