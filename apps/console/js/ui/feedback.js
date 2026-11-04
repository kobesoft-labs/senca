// 状態を伝えるもの: バッジ・状態のピル・お知らせ・空の状態・スケルトン・メーター。

import { h } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';

/**
 * 小さなラベル。kind: ok・warn・danger・info・accent・outline・live（ON AIR）・rec（録画中）、無しは中立。
 * @param {{dot?: boolean, icon?: string, size?: 'lg', testid?: string, title?: string}} [o]
 */
export function badge(text, kind, o = {}) {
  return h('span', {
    class: ['badge', kind, o.dot && 'has-dot', o.size].filter(Boolean).join(' '), 'data-testid': o.testid, title: o.title,
  }, o.icon ? icon(o.icon) : null, text);
}

// 状態 → [見た目, 点を付けるか]。文言は status.<状態>。
const STATUS = {
  live: ['live', false],
  recording: ['rec', false],
  idle: [null, true],
  waiting: [null, true],
  stopped: ['outline', true],
  running: ['ok', true],
  ready: ['ok', true],
  ok: ['ok', true],
  available: ['ok', true],
  finalized: ['ok', true],
  accepting: ['ok', true],
  pending: ['info', true],
  starting: ['info', true],
  stopping: ['warn', true],
  draining: ['warn', true],
  interrupted: ['warn', true],
  degraded: ['warn', true],
  warning: ['warn', true],
  provisional: ['warn', true],
  cancelled: ['outline', true],
  completed: ['ok', true],
  disabled: ['outline', true],
  unavailable: ['danger', true],
  failed: ['danger', true],
  error: ['danger', true],
  unknown: [null, true],
};

/** 状態のピル。知っている状態は決まった色と言葉（status.<state>）、知らないものは中立で生の値を出す。 */
export function statusPill(state, { label, testid } = {}) {
  const [kind, dot] = STATUS[state] || [null, true];
  const text = label || (STATUS[state] ? t(`status.${state}`) : String(state ?? '—'));
  return badge(text, kind, { dot, testid, title: state && !label && STATUS[state] ? String(state) : undefined });
}

export const STATUS_KINDS = Object.keys(STATUS);

const ALERT_ICON = { info: 'info', ok: 'success', warn: 'warning', error: 'error' };

/**
 * お知らせの帯。kind: info・ok・warn・error。
 * @param {'info'|'ok'|'warn'|'error'} kind
 * @param {string|Node|Array} body
 * @param {{title?: string, actions?: Node[], small?: boolean, role?: string, testid?: string}} [o]
 */
export function alert(kind, body, o = {}) {
  return h('div', {
    class: ['alert', kind, o.small && 'small'].filter(Boolean).join(' '),
    role: o.role || (kind === 'error' ? 'alert' : null), 'data-testid': o.testid,
  },
  icon(ALERT_ICON[kind] || 'info'),
  h('div', { class: 'alert-body' },
    o.title ? h('div', { class: 'alert-title' }, o.title) : null,
    h('div', null, body),
    o.actions && o.actions.length ? h('div', { class: 'alert-actions' }, o.actions) : null));
}

/**
 * 空の状態: 何も無い理由と、次にすること（主の操作）を出す。
 * @param {{icon?: string, title: string, body?: string|Node, actions?: Node[], compact?: boolean, testid?: string}} o
 */
export function emptyState({ icon: name = 'inbox', title, body, actions, compact, testid }) {
  return h('div', { class: ['empty-state', compact && 'compact'].filter(Boolean).join(' '), 'data-testid': testid },
    h('div', { class: 'es-icon' }, icon(name)),
    h('h3', null, title),
    body ? h('p', null, body) : null,
    actions && actions.length ? h('div', { class: 'es-actions' }, actions) : null);
}

/**
 * 読み込み中の形（スケルトン）。kind: lines（既定）・table・cards・block。
 */
export function skeleton({ kind = 'lines', rows = 4 } = {}) {
  const label = t('common.loading');
  if (kind === 'table') {
    return h('div', { class: 'skeleton', 'aria-busy': 'true', role: 'status', 'aria-label': label },
      Array.from({ length: rows }, () => h('div', { class: 'sk-row' }, h('div', { class: 'sk' }), h('div', { class: 'sk' }), h('div', { class: 'sk' }), h('div', { class: 'sk' }))));
  }
  if (kind === 'cards') {
    return h('div', { class: 'grid cards', 'aria-busy': 'true', role: 'status', 'aria-label': label },
      Array.from({ length: rows }, () => h('div', { class: 'card skeleton' }, h('div', { class: 'sk' }), h('div', { class: 'sk lg' }), h('div', { class: 'sk' }))));
  }
  if (kind === 'block') return h('div', { class: 'skeleton', 'aria-busy': 'true', role: 'status', 'aria-label': label }, h('div', { class: 'sk block' }));
  const widths = ['92%', '76%', '84%', '60%', '88%', '70%'];
  return h('div', { class: 'skeleton', 'aria-busy': 'true', role: 'status', 'aria-label': label },
    Array.from({ length: rows }, (_, i) => h('div', { class: 'sk', style: { width: widths[i % widths.length] } })));
}

/** 回るアイコンと短い文（「読み込み中…」）。 */
export function loadingLine(text = t('common.loading')) {
  return h('div', { class: 'loading-line', role: 'status', 'aria-busy': 'true' }, icon('spinner', { spin: true }), text);
}

/**
 * 量のメーター（使用率など）。value / max、kind は ok・warn・danger（無しはアクセント）。
 * value が null なら途中の量が分からない動き（indeterminate）。
 */
export function meter({ label, value, max = 1, kind, valueText }) {
  const indeterminate = value === null || value === undefined;
  const ratio = indeterminate ? 0 : Math.max(0, Math.min(1, value / max));
  const fill = h('div', { class: 'fill' });
  fill.style.width = `${(ratio * 100).toFixed(1)}%`;
  return h('div', { class: ['meter-bar', kind, indeterminate && 'indeterminate'].filter(Boolean).join(' ') },
    label || valueText ? h('div', { class: 'meter-head' }, h('span', null, label), h('strong', null, valueText ?? `${Math.round(ratio * 100)}%`)) : null,
    h('div', {
      class: 'track', role: indeterminate ? 'progressbar' : 'meter', 'aria-label': label,
      'aria-valuemin': indeterminate ? null : '0', 'aria-valuemax': indeterminate ? null : String(max), 'aria-valuenow': indeterminate ? null : String(value),
      'aria-valuetext': valueText,
    }, fill));
}
