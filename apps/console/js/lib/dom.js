// DOM の小さな道具。文字列は常にテキストノードにする（HTML の文字列は組み立てない）。

const SVG_NS = 'http://www.w3.org/2000/svg';

function apply(el, attrs) {
  if (!attrs) return;
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.setAttribute('class', Array.isArray(v) ? v.filter(Boolean).join(' ') : v);
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
    else if (k === 'value' && 'value' in el && el.namespaceURI !== SVG_NS) el.value = v;
    else if (k === 'checked' || k === 'selected' || k === 'disabled' || k === 'multiple' || k === 'required') el[k] = !!v;
    else if (k === 'ref' && typeof v === 'function') v(el);
    else el.setAttribute(k, v === true ? '' : String(v));
  }
}

function append(el, children) {
  for (const c of children) {
    if (c === null || c === undefined || c === false || c === true) continue;
    if (Array.isArray(c)) append(el, c);
    else if (c instanceof Node) el.appendChild(c);
    else el.appendChild(document.createTextNode(String(c)));
  }
}

/** 要素を作る。h('button', {class:'primary', onClick}, '保存') */
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs && (typeof attrs !== 'object' || attrs instanceof Node || Array.isArray(attrs))) {
    children.unshift(attrs);
    attrs = null;
  }
  apply(el, attrs);
  append(el, children);
  return el;
}

/** SVG の要素を作る。 */
export function s(tag, attrs, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  apply(el, attrs);
  append(el, children);
  return el;
}

export function clear(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
  return el;
}

export function replace(el, ...children) {
  clear(el);
  append(el, children);
  return el;
}

let uid = 0;
export function nextId(prefix = 'id') {
  uid += 1;
  return `${prefix}-${uid}`;
}

/** ラベル付きの入力欄。 */
export function field(label, input, hint) {
  if (!input.id) input.id = nextId('f');
  return h('div', { class: 'field' },
    h('label', { for: input.id }, label),
    input,
    hint ? h('div', { class: 'hint', id: `${input.id}-hint` }, hint) : null);
}

export function select(options, value, attrs = {}) {
  return h('select', attrs, options.map((o) => {
    const [v, label] = Array.isArray(o) ? o : [o, o];
    return h('option', { value: v, selected: v === value }, label);
  }));
}

export function checkbox(label, checked, attrs = {}) {
  const input = h('input', { type: 'checkbox', checked, ...attrs });
  return { input, el: h('label', { class: 'check' }, input, label) };
}

/** 表を作る。columns: [{label, render(row) -> node|string, class}] */
export function table(columns, rows, { empty = 'ありません', caption } = {}) {
  if (!rows.length) return h('div', { class: 'empty' }, empty);
  return h('div', { class: 'table-wrap' },
    h('table', null,
      caption ? h('caption', { class: 'visually-hidden' }, caption) : null,
      h('thead', null, h('tr', null, columns.map((c) => h('th', { scope: 'col', class: c.class }, c.label)))),
      h('tbody', null, rows.map((r) => h('tr', null, columns.map((c) => h('td', { class: c.class }, c.render(r))))))));
}

export function kv(pairs) {
  return h('dl', { class: 'kv' }, pairs.filter(Boolean).flatMap(([k, v]) => [h('dt', null, k), h('dd', null, v ?? '—')]));
}

export function badge(text, kind) {
  return h('span', { class: ['badge', kind] }, text);
}

/** 関数呼び出しをまとめる（SSE で連続した更新を 1 回にする）。 */
export function debounce(fn, ms = 300) {
  let t = null;
  const d = (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
  d.cancel = () => clearTimeout(t);
  return d;
}
