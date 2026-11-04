// ページの組み立て: ページの見出し・パンくず・パネル（節）・統計のタイル・項目と値。

import { h, nextId } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { emit } from '../lib/bus.js';
import { t } from '../i18n/index.js';

/**
 * パンくず（上の帯にも出す）。items: [{label, href}]、最後が今のページ。
 */
export function breadcrumbs(items, { label } = {}) {
  return h('nav', { 'aria-label': label || t('shell.breadcrumbs') },
    h('ol', { class: 'crumbs' }, items.map((it, i) => h('li', null,
      i === items.length - 1 || !it.href
        ? h('span', { 'aria-current': i === items.length - 1 ? 'page' : null, class: 'truncate' }, it.label)
        : h('a', { href: it.href }, it.label)))));
}

/**
 * ページの見出し。title（h1）・description・actions（右のボタン）・badges（題の横）。
 * crumbs を渡すと上の帯のパンくずを置き換える（詳細ページで「配信 › studio-a」のように）。
 * 狭い画面では帯のパンくずを畳むので、crumbs の 1 つ前へ戻るリンクを題の上に出す。
 * @param {{title: string|Node, description?: string|Node, actions?: Node[], badges?: Node[], crumbs?: {label:string, href?:string}[], help?: Node, testid?: string}} o
 */
export function pageHeader({ title, description, actions, badges, crumbs, help, testid }) {
  if (crumbs) emit('crumbs', crumbs);
  const back = crumbs && crumbs.length > 1 ? crumbs[crumbs.length - 2] : null;
  return h('header', { class: 'page-header', 'data-testid': testid },
    back && back.href ? h('a', { class: 'page-back', href: back.href }, icon('arrow-left', { size: 'sm' }), back.label) : null,
    h('div', { class: 'ph-row' },
      h('div', { class: 'ph-main' },
        h('div', { class: 'ph-title' }, h('h1', null, title), help || null, badges || null),
        description ? h('p', { class: 'ph-desc' }, description) : null),
      actions && actions.length ? h('div', { class: 'ph-actions' }, actions) : null));
}

/**
 * 節（パネル）。見出し（h2）・説明・右の操作・本文。aria-labelledby を自動で付ける。
 * @param {{title?: string|Node, icon?: string, description?: string|Node, actions?: Node[], id?: string, flush?: boolean, help?: Node, testid?: string, class?: string}} o
 */
export function panel(o, ...children) {
  const id = o.id || nextId('p');
  return h('section', { class: ['panel', o.flush && 'flush', o.class].filter(Boolean).join(' '), 'aria-labelledby': o.title ? id : null, 'data-testid': o.testid },
    o.title ? h('div', { class: 'panel-head' },
      h('div', { class: 'panel-title' },
        h('h2', { id }, o.icon ? icon(o.icon) : null, o.title, o.help || null),
        o.description ? h('p', { class: 'panel-desc' }, o.description) : null),
      o.actions && o.actions.length ? h('div', { class: 'panel-actions' }, o.actions) : null) : null,
    children);
}

/**
 * 統計のタイル。href があればリンクのカード。kind: live（赤い数）。
 * @param {{label: string, value: string|number, sub?: string|Node, icon?: string, href?: string, kind?: 'live', testid?: string}} o
 */
export function statTile({ label, value, sub, icon: name, href, kind, testid }) {
  const inner = [
    h('div', { class: 'stat-label' }, name ? icon(name) : null, label),
    h('div', { class: 'stat-value' }, value),
    h('div', { class: 'stat-sub' }, sub ?? ''),
  ];
  const cls = ['card', 'stat', kind].filter(Boolean).join(' ');
  return href ? h('a', { class: cls, href, 'data-testid': testid }, inner) : h('div', { class: cls, 'data-testid': testid }, inner);
}

/** 項目と値（dl）。pairs: [[項目, 値], …]、null は飛ばす。 */
export function kvList(pairs) {
  return h('dl', { class: 'kv' }, pairs.filter(Boolean).flatMap(([k, v]) => [h('dt', null, k), h('dd', null, v ?? '—')]));
}
