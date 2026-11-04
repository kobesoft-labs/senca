// 表（並べ替えられる・狭い画面ではカードの並びになる）とページ送り。
//
// 狭い画面（768px 以下）では CSS が各行をカードにし、セルの data-label を見出しにする。
// primary: true の列はカードの題（太字・見出し無し）になる。actions: true の列は右寄せの操作。

import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { emptyState } from './feedback.js';

/**
 * @typedef {object} Column
 * @property {string} [key] 並べ替えの鍵（sort を使うときは必須）
 * @property {string} label 見出し（狭い画面ではカードの項目名）
 * @property {(row: any) => any} render セルの中身
 * @property {(row: any) => (string|number|null)} [sort] 並べ替えの値。あれば見出しがボタンになる
 * @property {string} [class] 'num' など
 * @property {boolean} [primary] カードの題にする列
 * @property {boolean} [actions] 操作の列
 * @property {boolean} [wrap] 長い文を折り返す列（既定は折り返さない。primary の列は折り返す）
 */

/**
 * @param {object} o
 * @param {Column[]} o.columns
 * @param {any[]} o.rows
 * @param {string|object} [o.empty] 空のときの文、または emptyState() の引数
 * @param {string} [o.caption] 読み上げ用の表題
 * @param {{key: string, dir: 'asc'|'desc'}} [o.sort] 最初の並び
 * @param {(row: any) => string} [o.rowKey] 行の data-key
 * @param {string} [o.testid]
 * @returns {HTMLElement & {update: (rows: any[]) => void}}
 */
export function dataTable({ columns, rows, empty, caption, sort, rowKey, testid }) {
  const wrap = h('div', { class: 'table-wrap', 'data-testid': testid });
  let state = sort ? { ...sort } : null;
  let data = rows;

  const sorted = () => {
    if (!state) return data;
    const col = columns.find((c) => c.key === state.key);
    if (!col || !col.sort) return data;
    const coll = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
    const out = data.slice().sort((a, b) => {
      const x = col.sort(a);
      const y = col.sort(b);
      if (x === y) return 0;
      if (x === null || x === undefined) return 1;
      if (y === null || y === undefined) return -1;
      const r = typeof x === 'number' && typeof y === 'number' ? x - y : coll.compare(String(x), String(y));
      return state.dir === 'desc' ? -r : r;
    });
    return out;
  };

  const head = (c) => {
    if (!c.sort) return h('th', { scope: 'col', class: c.class }, c.label);
    const active = state && state.key === c.key;
    const ariaSort = active ? (state.dir === 'asc' ? 'ascending' : 'descending') : 'none';
    return h('th', { scope: 'col', class: c.class, 'aria-sort': ariaSort },
      h('button', {
        type: 'button', class: 'sort-btn',
        onClick: () => {
          state = { key: c.key, dir: active && state.dir === 'asc' ? 'desc' : 'asc' };
          draw();
          const b = wrap.querySelector(`th[data-key="${c.key}"] .sort-btn`);
          if (b) b.focus();
        },
      }, c.label, icon(active ? (state.dir === 'asc' ? 'arrow-up' : 'arrow-down') : 'sort')));
  };

  const draw = () => {
    if (!data.length) {
      const e = typeof empty === 'object' && empty !== null && !(empty instanceof Node)
        ? emptyState({ compact: true, ...empty })
        : empty instanceof Node ? empty : h('div', { class: 'empty' }, empty || t('common.none'));
      replace(wrap, e);
      return;
    }
    replace(wrap, h('table', null,
      caption ? h('caption', { class: 'visually-hidden' }, caption) : null,
      h('thead', null, h('tr', null, columns.map((c) => {
        const th = head(c);
        if (c.key) th.dataset.key = c.key;
        return th;
      }))),
      h('tbody', null, sorted().map((r) => h('tr', { 'data-key': rowKey ? rowKey(r) : null },
        columns.map((c) => h('td', {
          class: [c.class, c.primary && 'cell-primary', c.actions && 'actions-cell', !c.wrap && !c.primary && 'nw'].filter(Boolean).join(' ') || null,
          'data-label': c.actions || c.primary ? '' : c.label,
        }, h('div', { class: 'cell' }, c.render(r)))))))));
  };
  draw();
  wrap.update = (next) => { data = next; draw(); };
  return wrap;
}

/**
 * ページ送り（カーソルのページ送りにも使える: 総数が分からなくてもよい）。
 * @param {{page: number, hasPrev: boolean, hasNext: boolean, onPrev: () => void, onNext: () => void, note?: string}} o page は 0 始まり
 */
export function pagination({ page, hasPrev, hasNext, onPrev, onNext, note }) {
  return h('nav', { class: 'pagination', 'aria-label': t('pager.label') },
    h('button', { type: 'button', class: 'sm', disabled: !hasPrev, onClick: onPrev }, icon('chevron-left'), t('pager.prev')),
    h('span', { class: 'page-label', 'aria-live': 'polite' }, t('pager.page', { n: page + 1 }), note ? ` ${note}` : ''),
    h('button', { type: 'button', class: 'sm', disabled: !hasNext, onClick: onNext }, t('pager.next'), icon('chevron-right')));
}
