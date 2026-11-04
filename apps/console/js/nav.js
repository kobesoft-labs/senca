// 画面の一覧（経路・メニュー・パンくず・コマンドパレット・「g ○」のショートカットの元）。
// 画面を足すときは ROUTES と NAV_GROUPS に 1 行ずつ足し、辞書（js/i18n/*/nav.js）に nav.<id> を足す。
// soon: true の画面は「準備中」のページになる（実装したら soon を消し、load を本物にする）。

export const NAV_GROUPS = [
  { id: 'overview', items: [
    { id: 'dashboard', href: '#/', icon: 'dashboard', key: 'd' },
  ] },
  { id: 'delivery', items: [
    { id: 'streams', href: '#/streams', icon: 'streams', key: 's' },
    { id: 'recordings', href: '#/recordings', icon: 'recordings', key: 'r' },
    { id: 'schedules', href: '#/schedules', icon: 'schedules', key: 'c' },
    { id: 'send', href: '#/send', icon: 'send', key: 'b' },
  ] },
  { id: 'production', items: [
    { id: 'pipelines', href: '#/pipelines', icon: 'pipelines', key: 'p' },
    { id: 'production', href: '#/production', icon: 'switcher', key: 'x' },
  ] },
  { id: 'realtime', items: [
    { id: 'rooms', href: '#/rooms', icon: 'rooms', key: 'm', soon: true },
    { id: 'media-pipelines', href: '#/media-pipelines', icon: 'media-pipelines', key: 'l', soon: true },
  ] },
  { id: 'operations', items: [
    { id: 'nodes', href: '#/nodes', icon: 'nodes', key: 'n' },
    { id: 'grid', href: '#/grid', icon: 'grid', key: 'g', soon: true },
    { id: 'audit', href: '#/audit', icon: 'audit', key: 'a', soon: true },
  ] },
  { id: 'settings', items: [
    { id: 'tokens', href: '#/tokens', icon: 'key', key: 'k' },
    { id: 'secrets', href: '#/secrets', icon: 'secrets', key: 'e' },
    { id: 'users', href: '#/users', icon: 'users', key: 'u', soon: true },
  ] },
];

export const NAV_ITEMS = NAV_GROUPS.flatMap((g) => g.items.map((it) => ({ ...it, group: g.id })));

const soon = () => import('./views/coming-soon.js');

/** 経路。nav はメニューで光らせる項目の id。 */
export const ROUTES = [
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
  { re: /^\/rooms(\/.*)?$/, nav: 'rooms', load: soon },
  { re: /^\/media-pipelines(\/.*)?$/, nav: 'media-pipelines', load: soon },
  { re: /^\/grid\/?$/, nav: 'grid', load: soon },
  { re: /^\/audit\/?$/, nav: 'audit', load: soon },
  { re: /^\/users(\/.*)?$/, nav: 'users', load: soon },
  { re: /^\/_styleguide\/?$/, nav: null, load: () => import('./views/styleguide.js') },
];

export function findRoute(path) {
  const r = ROUTES.find((x) => x.re.test(path));
  if (!r) return null;
  return { ...r, params: path.match(r.re).slice(1).filter((x) => x !== undefined).map(decodeURIComponent) };
}

export function navItem(id) {
  return NAV_ITEMS.find((x) => x.id === id) || null;
}
