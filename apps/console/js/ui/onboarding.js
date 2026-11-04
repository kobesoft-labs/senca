// はじめの手順（チェックリスト）。進み具合は実際の状態から決める（呼ぶ側が done を計算して渡す）。
// 全部終わるか「閉じる」を押すと、このブラウザでは出さない（localStorage、id ごと）。
//
//   checklist({ id: 'getting-started', title: t('…'), steps: [
//     { id: 'stream', title: t('…'), description: t('…'), done: streams.length > 0, action: { label: t('…'), href: '#/streams?new=1' } },
//   ] })

import { h } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { meter } from './feedback.js';

const KEY = (id) => `senca.console.checklist.${id}.dismissed`;

export function checklistDismissed(id) {
  try { return localStorage.getItem(KEY(id)) === '1'; } catch { return false; }
}

/**
 * @param {{id: string, title: string, description?: string, steps: {id: string, title: string, description?: string, done: boolean, action?: {label: string, href?: string, onClick?: () => any}}[], dismissible?: boolean, onDismiss?: () => void}} o
 * @returns {HTMLElement|null} 閉じてあれば null
 */
export function checklist({ id, title, description, steps, dismissible = true, onDismiss }) {
  if (dismissible && checklistDismissed(id)) return null;
  const done = steps.filter((s) => s.done).length;
  const el = h('section', { class: 'panel checklist', 'aria-labelledby': `cl-${id}`, 'data-testid': `checklist-${id}` });
  const close = () => {
    try { localStorage.setItem(KEY(id), '1'); } catch { /* 出し続けるだけ */ }
    el.remove();
    if (onDismiss) onDismiss();
  };
  el.append(
    h('div', { class: 'cl-head' },
      h('div', { class: 'cl-title' }, h('h2', { id: `cl-${id}` }, title), description ? h('p', null, description) : null),
      h('div', { class: 'cl-progress' }, meter({ label: t('checklist.progress'), value: done, max: steps.length, kind: done === steps.length ? 'ok' : undefined, valueText: t('checklist.count', { done, total: steps.length }) })),
      dismissible ? h('button', { type: 'button', class: 'ghost icon-only sm', 'aria-label': t('checklist.dismiss'), onClick: close }, icon('close')) : null),
    h('ol', null, steps.map((s, i) => h('li', { class: s.done ? 'done' : null, 'data-step': s.id },
      h('span', { class: 'cl-mark', 'aria-hidden': 'true' }, s.done ? icon('check') : String(i + 1)),
      h('div', { class: 'cl-text' },
        h('strong', null, s.title, h('span', { class: 'visually-hidden' }, ` (${s.done ? t('checklist.done') : t('checklist.todo')})`)),
        s.description ? h('span', null, s.description) : null),
      !s.done && s.action
        ? (s.action.href
          ? h('a', { class: `btn sm ${i === steps.findIndex((x) => !x.done) ? 'primary' : ''}`, href: s.action.href }, s.action.label)
          : h('button', { type: 'button', class: `sm ${i === steps.findIndex((x) => !x.done) ? 'primary' : ''}`, onClick: s.action.onClick }, s.action.label))
        : h('span')))));
  return el;
}
