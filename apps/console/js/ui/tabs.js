// タブ（WAI-ARIA の tabs。←→・Home・End で移動、選んだタブは URL の ?tab= に入る）。
//
//   tabs({ label: t('streams.detail.tabs'), tabs: [
//     { id: 'overview', label: t('…'), icon: 'info', render: () => node },
//     { id: 'outputs', label: t('…'), count: 3, render: async () => node },
//   ] })
// render は初めて開いたときに 1 回だけ呼ぶ（中身は残す）。URL を変えても画面は作り直さない（history.replaceState）。

import { h, nextId, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { loadingLine } from './feedback.js';

function hashParam(name) {
  const q = location.hash.split('?')[1] || '';
  return new URLSearchParams(q).get(name);
}

function setHashParam(name, value) {
  const [path, q] = location.hash.split('?');
  const p = new URLSearchParams(q || '');
  p.set(name, value);
  history.replaceState(history.state, '', `${location.pathname}${location.search}${path}?${p.toString()}`);
}

/**
 * @param {{tabs: {id: string, label: string, icon?: string, count?: number, render: () => Node|Promise<Node>}[], label: string, param?: string|null, initial?: string, onChange?: (id: string) => void}} o
 *   param を null にすると URL に入れない（ダイアログの中など）
 * @returns {HTMLElement & {select: (id: string) => void}}
 */
export function tabs({ tabs: items, label, param = 'tab', initial, onChange }) {
  const base = nextId('tabs');
  const list = h('div', { class: 'tablist', role: 'tablist', 'aria-label': label });
  const panels = h('div');
  const rendered = new Set();
  const btns = [];
  const pans = [];
  items.forEach((it, i) => {
    const tabId = `${base}-tab-${it.id}`;
    const panId = `${base}-panel-${it.id}`;
    const b = h('button', {
      type: 'button', role: 'tab', id: tabId, 'aria-controls': panId, 'aria-selected': 'false', tabindex: '-1', 'data-tab': it.id,
      onClick: () => select(it.id, true),
    }, it.icon ? icon(it.icon) : null, it.label, it.count !== undefined && it.count !== null ? h('span', { class: 'count' }, String(it.count)) : null);
    const p = h('div', { role: 'tabpanel', id: panId, 'aria-labelledby': tabId, tabindex: '0', hidden: true });
    btns.push(b);
    pans.push(p);
    list.appendChild(b);
    panels.appendChild(p);
    b.addEventListener('keydown', (e) => {
      let j = null;
      if (e.key === 'ArrowRight') j = (i + 1) % items.length;
      if (e.key === 'ArrowLeft') j = (i - 1 + items.length) % items.length;
      if (e.key === 'Home') j = 0;
      if (e.key === 'End') j = items.length - 1;
      if (j === null) return;
      e.preventDefault();
      select(items[j].id, true);
      btns[j].focus();
    });
  });
  const select = async (id, user = false) => {
    const i = Math.max(0, items.findIndex((x) => x.id === id));
    btns.forEach((b, k) => {
      b.setAttribute('aria-selected', String(k === i));
      b.tabIndex = k === i ? 0 : -1;
      pans[k].hidden = k !== i;
    });
    if (param && user) setHashParam(param, items[i].id);
    if (!rendered.has(i)) {
      rendered.add(i);
      const r = items[i].render();
      if (r && typeof r.then === 'function') {
        replace(pans[i], loadingLine());
        replace(pans[i], await r);
      } else replace(pans[i], r);
    }
    if (onChange && user) onChange(items[i].id);
  };
  const el = h('div', { class: 'tabs' }, list, panels);
  el.select = (id) => select(id, true);
  select((param && hashParam(param)) || initial || items[0].id);
  return el;
}
