// 選択のカード（大きなラジオボタン）。はじめての人に「どれを選ぶか」を説明つきで見せるときに使う。
// 中身は本物の <input type="radio">（←→↑↓で動く・読み上げも普通のラジオ）。2〜6 個まで。

import { h, nextId } from '../lib/dom.js';
import { icon } from '../lib/icons.js';

/**
 * @param {{label: string, options: {value: string, title: string, description?: string, icon?: string, disabled?: boolean, note?: string}[],
 *   value?: string, onChange?: (v: string) => void, testid?: string, compact?: boolean, hideLegend?: boolean}} o
 * @returns {HTMLFieldSetElement & {value: string, set: (v: string) => void}}
 */
export function choiceCards({ label, options, value, onChange, testid, compact, hideLegend }) {
  const name = nextId('choice');
  const el = h('fieldset', { class: ['choice-cards', compact && 'compact'].filter(Boolean).join(' '), 'data-testid': testid });
  el.append(h('legend', { class: hideLegend ? 'visually-hidden' : 'choice-legend' }, label));
  const grid = h('div', { class: 'choice-grid' });
  const inputs = options.map((o) => {
    const input = h('input', { type: 'radio', name, value: o.value, disabled: o.disabled, 'data-testid': testid ? `${testid}-${o.value}` : null });
    input.addEventListener('change', () => {
      if (!input.checked) return;
      el.value = o.value;
      if (onChange) onChange(o.value);
    });
    grid.append(h('label', { class: ['choice-card', o.disabled && 'disabled'].filter(Boolean).join(' ') },
      input,
      o.icon ? h('span', { class: 'choice-icon' }, icon(o.icon)) : null,
      h('span', { class: 'choice-text' },
        h('strong', null, o.title),
        o.description ? h('span', null, o.description) : null,
        o.note ? h('em', null, o.note) : null),
      h('span', { class: 'choice-check', 'aria-hidden': 'true' }, icon('check'))));
    return input;
  });
  el.append(grid);
  el.set = (v) => {
    el.value = v;
    for (const i of inputs) i.checked = i.value === v;
  };
  el.set(value ?? (options.find((o) => !o.disabled) || options[0]).value);
  return el;
}
