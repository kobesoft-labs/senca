// スタイルガイド（#/_styleguide、メニューには出さない）: 色・文字・アイコン・部品をすべて並べる。
// 動かないもの（色・ボタン・表など）は明・暗を左右に並べ、動くもの（ダイアログ・メニュー・通知）は今のテーマで試す。
// 部品を足したらここにも足す（ほかの画面を作る人の見本で、画面写真の元）。

import { h } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import {
  button, iconButton, buttonGroup, badge, statusPill, STATUS_KINDS, alert, emptyState, skeleton, meter, loadingLine,
  breadcrumbs, pageHeader, panel, statTile, kvList, dataTable, pagination, modal, confirmDialog, attachMenu, helpHint, tooltip,
  formField, toggle, segmented, searchBox, copyField, tabs, toast, checklist, icon, ICON_NAMES,
} from '../ui/index.js';
import { fmtBps, fmtBytes, fmtDuration, fmtDurationText, fmtNumber, fmtPercent, fmtTime, fmtRelative } from '../lib/format.js';

const COLORS = [
  ['bg', 'text'], ['surface', 'text'], ['surface-2', 'text-2'], ['surface-3', 'text'], ['accent', 'accent-fg'], ['accent-soft', 'accent-soft-text'],
  ['ok-soft', 'ok-text'], ['warn-soft', 'warn-text'], ['danger-soft', 'danger-text'], ['info-soft', 'info-text'], ['live', 'live-fg'], ['danger-solid', 'live-fg'],
  ['surface-inverse', 'text-inverse'], ['neutral-soft', 'neutral-text'],
];

function lum(rgb) {
  const c = rgb.map((v) => v / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
}
function parseColor(s) {
  const m = /rgba?\((\d+),\s*(\d+),\s*(\d+)/.exec(s);
  return m ? [+m[1], +m[2], +m[3]] : [0, 0, 0];
}

/** 色の見本（その場で計算したコントラスト比つき）。 */
function swatches(scope) {
  const grid = h('div', { class: 'sg-swatches' });
  for (const [bg, fg] of COLORS) {
    const sw = h('div', { class: 'sg-swatch' }, h('strong', null, 'Aa'), h('code', null, `--${bg}`), h('code', null, `--${fg}`), h('span', { class: 'sg-ratio' }));
    sw.style.background = `var(--${bg})`;
    sw.style.color = `var(--${fg})`;
    grid.appendChild(sw);
  }
  requestAnimationFrame(() => {
    for (const sw of grid.children) {
      const cs = getComputedStyle(sw);
      const a = lum(parseColor(cs.color));
      const b = lum(parseColor(cs.backgroundColor));
      const r = (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
      sw.querySelector('.sg-ratio').textContent = `${r.toFixed(2)} ${r >= 7 ? 'AAA' : r >= 4.5 ? 'AA' : '—'}`;
    }
  });
  return grid;
  // scope は呼び出し側で data-theme を付けた要素
}

/** 明・暗を左右に並べる。build は要素を返す関数（左右で 1 回ずつ呼ぶ）。 */
function bothThemes(build) {
  return h('div', { class: 'sg-both' },
    h('div', { class: 'sg-theme', 'data-theme': 'light' }, h('div', { class: 'sg-theme-label' }, icon('sun'), t('theme.light')), build('light')),
    h('div', { class: 'sg-theme', 'data-theme': 'dark' }, h('div', { class: 'sg-theme-label' }, icon('moon'), t('theme.dark')), build('dark')));
}

const SAMPLE_ROWS = [
  { name: 'studio-a', state: 'live', bitrate: 6_200_000, viewers: 1240, updated: Date.now() - 120000 },
  { name: 'news-desk', state: 'waiting', bitrate: null, viewers: 0, updated: Date.now() - 3600000 },
  { name: 'event-2026-hall-b', state: 'stopped', bitrate: null, viewers: 0, updated: Date.now() - 86400000 * 3 },
  { name: 'sports-cam-3', state: 'live', bitrate: 12_400_000, viewers: 18902, updated: Date.now() - 4000 },
];

function sampleTable() {
  return dataTable({
    caption: t('sg.tableCaption'),
    sort: { key: 'name', dir: 'asc' },
    columns: [
      { key: 'name', label: t('common.name'), primary: true, sort: (r) => r.name, render: (r) => h('a', { href: '#/_styleguide' }, r.name) },
      { key: 'state', label: t('common.state'), sort: (r) => r.state, render: (r) => statusPill(r.state) },
      { key: 'bitrate', label: t('sg.bitrate'), class: 'num', sort: (r) => r.bitrate, render: (r) => (r.bitrate ? fmtBps(r.bitrate) : '—') },
      { key: 'viewers', label: t('sg.viewers'), class: 'num', sort: (r) => r.viewers, render: (r) => fmtNumber(r.viewers) },
      { key: 'updated', label: t('common.updated'), sort: (r) => r.updated, render: (r) => fmtRelative(new Date(r.updated).toISOString()) },
      { label: t('common.actions'), actions: true, render: () => h('div', { class: 'row' }, iconButton('edit', t('common.edit'), { size: 'sm' }), iconButton('trash', t('common.delete'), { size: 'sm' })) },
    ],
    rows: SAMPLE_ROWS,
  });
}

function staticGallery() {
  return h('div', { class: 'stack sg-gallery' },
    h('h3', null, t('sg.colors')), swatches(),
    h('h3', null, t('sg.buttons')),
    buttonGroup(button(t('sg.primary'), { variant: 'primary', icon: 'plus' }), button(t('sg.secondary')), button(t('sg.ghost'), { variant: 'ghost' }),
      button(t('common.delete'), { variant: 'danger', icon: 'trash' }), button(t('sg.dangerSolid'), { variant: 'danger-solid' }), button(t('sg.link'), { variant: 'link' })),
    buttonGroup(button(t('sg.small'), { size: 'sm', icon: 'refresh' }), button(t('sg.medium')), button(t('sg.large'), { size: 'lg', variant: 'primary' }),
      button(t('sg.disabled'), { attrs: { disabled: true } }), (() => { const b = button(t('sg.loading'), { icon: 'save', variant: 'primary' }); b.setAttribute('aria-busy', 'true'); b.disabled = true; b.querySelector('svg').replaceWith(icon('spinner')); return b; })(),
      iconButton('settings', t('sg.iconOnly')), iconButton('more', t('common.more'), { variant: 'secondary' })),
    h('h3', null, t('sg.badges')),
    h('div', { class: 'row' }, badge(t('status.live'), 'live'), badge(t('status.recording'), 'rec'), badge('ok', 'ok'), badge('warn', 'warn'), badge('danger', 'danger'), badge('info', 'info'), badge('accent', 'accent'), badge('outline', 'outline'), badge('neutral'), h('span', { class: 'tag' }, 'tag')),
    h('div', { class: 'row' }, STATUS_KINDS.map((s) => statusPill(s))),
    h('h3', null, t('sg.alerts')),
    alert('info', t('sg.alertInfo'), { title: t('sg.alertInfoTitle') }),
    alert('ok', t('sg.alertOk')),
    alert('warn', t('sg.alertWarn'), { actions: [button(t('common.retry'), { size: 'sm' })] }),
    alert('error', [t('sg.alertError'), h('div', { class: 'meta' }, 'HTTP 409 · CONFLICT · request_id: 01J…')]),
    h('h3', null, t('sg.cards')),
    h('div', { class: 'grid stat-grid' },
      statTile({ label: t('nav.streams'), value: 12, sub: t('sg.statSub'), icon: 'streams', href: '#/_styleguide' }),
      statTile({ label: t('status.live'), value: 3, sub: 'studio-a, sports-cam-3', icon: 'live', kind: 'live' }),
      statTile({ label: t('nav.recordings'), value: '248', sub: fmtBytes(1.8e12), icon: 'recordings' })),
    panel({ title: t('sg.panelTitle'), icon: 'info', description: t('sg.panelDesc'), actions: [button(t('common.edit'), { size: 'sm', icon: 'edit' })] },
      kvList([['ID', h('code', null, '01JC8Q5K3V')], [t('common.state'), statusPill('running')], [t('sg.duration'), `${fmtDuration(3723000)} (${fmtDurationText(3723000)})`], [t('common.updated'), fmtTime(new Date().toISOString())], [t('sg.ratio'), fmtPercent(0.734, 1)]])),
    h('h3', null, t('sg.table')), sampleTable(),
    pagination({ page: 1, hasPrev: true, hasNext: true, onPrev: () => {}, onNext: () => {} }),
    h('h3', null, t('sg.forms')),
    h('div', { class: 'form' },
      formField({ label: t('common.name'), input: h('input', { type: 'text', placeholder: 'studio-a' }), required: true, hint: t('sg.nameHint') }),
      formField({ label: t('sg.withError'), input: h('input', { type: 'text', value: 'bad name!' }), error: t('sg.errorMsg') }),
      h('div', { class: 'cols' },
        formField({ label: t('sg.select'), input: h('select', null, h('option', null, 'standard'), h('option', null, 'low')) }),
        formField({ label: t('sg.number'), input: h('input', { type: 'number', value: 2000 }), optional: true })),
      formField({ label: t('sg.textarea'), input: h('textarea', null, '{ "version": 1 }') }),
      h('label', { class: 'check' }, h('input', { type: 'checkbox', checked: true }), t('sg.checkbox')),
      toggle({ label: t('sg.toggle'), description: t('sg.toggleDesc'), checked: true }).el,
      segmented({ label: t('sg.segmented'), options: [['auto', t('sg.auto')], ['webrtc', 'WebRTC'], ['hls', 'HLS']], value: 'auto' }),
      searchBox({ placeholder: t('sg.searchPlaceholder') }),
      copyField('rtmp://203.0.113.10/live', t('sg.copyUrl')),
      copyField('sk_live_8f2c9e1d7a6b5c4d', t('sg.copySecret'), { secret: true })),
    h('h3', null, t('sg.meters')),
    h('div', { class: 'grid cols-3' }, meter({ label: 'GPU', value: 0.42, kind: 'ok' }), meter({ label: t('sg.disk'), value: 0.81, kind: 'warn' }), meter({ label: t('sg.upload'), value: null, valueText: t('common.loading') })),
    h('h3', null, t('sg.empty')),
    h('div', { class: 'panel flush' }, emptyState({ icon: 'streams', title: t('sg.emptyTitle'), body: t('sg.emptyBody'), actions: [button(t('palette.newStream'), { variant: 'primary', icon: 'plus' })] })),
    h('h3', null, t('sg.skeleton')),
    h('div', { class: 'panel' }, skeleton({ kind: 'table', rows: 3 })),
    loadingLine(),
    h('h3', null, t('sg.crumbs')),
    breadcrumbs([{ label: t('nav.streams'), href: '#/_styleguide' }, { label: 'studio-a' }]));
}

export async function render(root) {
  const icons = h('div', { class: 'sg-icons' }, ICON_NAMES.map((n) => h('div', { class: 'sg-icon', title: n }, icon(n), h('code', null, n))));

  const menuBtn = button(t('sg.openMenu'), { icon: 'more', iconRight: 'chevron-down' });
  attachMenu(menuBtn, [
    { heading: t('sg.menuHeading') },
    { label: t('common.edit'), icon: 'edit', kbd: 'E' },
    { label: t('copy.copy'), icon: 'copy' },
    { label: t('sg.radioA'), checked: true },
    { label: t('sg.radioB'), checked: false },
    { separator: true },
    { label: t('common.delete'), icon: 'trash', danger: true },
  ]);
  const tipBtn = button(t('sg.hoverMe'), { icon: 'info' });
  tooltip(tipBtn, t('sg.tooltipText'));

  const interactive = h('div', { class: 'stack' },
    buttonGroup(
      button(t('sg.openModal'), { icon: 'plus', onClick: () => modal(t('sg.modalTitle'), () => h('div', { class: 'form' },
        formField({ label: t('common.name'), input: h('input', { type: 'text' }), required: true }),
        formField({ label: t('sg.select'), input: h('select', null, h('option', null, 'standard')) })), {
        icon: 'streams',
        actions: (close) => [button(t('common.cancel'), { onClick: () => close(false) }), button(t('common.create'), { variant: 'primary', onClick: () => close(true) })],
      }) }),
      button(t('sg.openConfirm'), { onClick: () => confirmDialog(t('sg.confirmTitle'), t('sg.confirmBody'), { ok: t('sg.confirmOk'), danger: false }) }),
      button(t('sg.openTypeConfirm'), { variant: 'danger', icon: 'trash', onClick: () => confirmDialog(t('sg.deleteTitle'), t('sg.deleteBody'), { ok: t('common.delete'), requireText: 'studio-a', consequences: [t('sg.consequence1'), t('sg.consequence2')] }) }),
      button(t('sg.toastOk'), { onClick: () => toast(t('sg.toastSaved'), 'ok') }),
      button(t('sg.toastUndo'), { onClick: () => toast(t('sg.toastDeleted'), 'info', { action: { label: t('common.undo'), onClick: () => toast(t('sg.toastRestored'), 'ok') } }) }),
      button(t('sg.toastError'), { onClick: () => toast(t('sg.toastFailed'), 'error', { action: { label: t('common.retry'), onClick: () => {} } }) }),
      menuBtn, tipBtn,
      h('span', { class: 'row' }, t('sg.helpLabel'), helpHint({ title: t('sg.helpTitle'), body: t('sg.helpBody'), guide: '04-ingest-playback.md' }))),
    tabs({ label: t('sg.tabs'), param: 'sgtab', tabs: [
      { id: 'one', label: t('sg.tabOne'), icon: 'info', render: () => h('p', null, t('sg.tabOneBody')) },
      { id: 'two', label: t('sg.tabTwo'), count: 3, render: () => h('p', null, t('sg.tabTwoBody')) },
      { id: 'three', label: t('sg.tabThree'), render: async () => h('p', null, t('sg.tabThreeBody')) },
    ] }),
    checklist({ id: 'styleguide-demo', dismissible: false, title: t('sg.checklistTitle'), description: t('sg.checklistDesc'), steps: [
      { id: 'a', title: t('sg.step1'), description: t('sg.step1d'), done: true },
      { id: 'b', title: t('sg.step2'), description: t('sg.step2d'), done: false, action: { label: t('palette.newStream'), href: '#/_styleguide' } },
      { id: 'c', title: t('sg.step3'), description: t('sg.step3d'), done: false, action: { label: t('sg.step3a'), href: '#/_styleguide' } },
    ] }));

  root.append(
    pageHeader({ title: t('sg.title'), description: t('sg.lead'), badges: [h('span', { class: 'tag' }, 'dev')] }),
    panel({ title: t('sg.interactive'), icon: 'zap', description: t('sg.interactiveDesc') }, interactive),
    panel({ title: t('sg.icons'), icon: 'layout-grid', description: t('sg.iconsDesc', { n: ICON_NAMES.length }) }, icons),
    panel({ title: t('sg.typography'), icon: 'edit' },
      h('div', { class: 'stack sg-type' },
        ['3xl', '2xl', 'xl', 'lg', 'md', 'sm', 'xs', '2xs'].map((s) => {
          const el = h('div', null, h('code', null, `--fs-${s}`), ' ', h('span', null, t('sg.typeSample')));
          el.lastChild.style.fontSize = `var(--fs-${s})`;
          return el;
        }),
        h('div', { class: 'row' }, ['1', '2', '3', '4', '5', '6', '8', '10', '12'].map((n) => {
          const b = h('span', { class: 'sg-space', title: `--sp-${n}` }, h('code', null, n));
          b.style.width = `var(--sp-${n})`;
          return b;
        })),
        h('div', { class: 'row' }, ['xs', 'sm', 'md', 'lg', 'xl'].map((r) => {
          const b = h('span', { class: 'sg-radius' }, h('code', null, r));
          b.style.borderRadius = `var(--r-${r})`;
          return b;
        }), ['1', '2', '3'].map((e) => {
          const b = h('span', { class: 'sg-radius' }, h('code', null, `shadow-${e}`));
          b.style.boxShadow = `var(--shadow-${e})`;
          return b;
        })))),
    h('h2', { class: 'section-gap' }, t('sg.both')),
    bothThemes(() => staticGallery()),
    h('h2', { class: 'section-gap' }, t('sg.pageHeader')),
    bothThemes(() => h('div', { class: 'sg-ph' }, pageHeader({ title: 'studio-a', description: t('sg.phDesc'), badges: [statusPill('live')], actions: [button(t('common.edit'), { icon: 'edit' }), button(t('palette.newStream'), { variant: 'primary', icon: 'plus' })] }))));
  return null;
}
