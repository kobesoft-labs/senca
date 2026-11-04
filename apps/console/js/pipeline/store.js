// このブラウザに置くもの（localStorage）: 保存前の下書き・部品の位置・保存済みのパイプラインの未保存の変更・写した部品。
// グラフは知らない項目を拒否するので、位置や下書きはグラフに入れられない。

const DRAFTS = 'senca.console.pipelines.v2';
const LAYOUT = 'senca.console.pipeline-layout.v1';
const EDITS = 'senca.console.pipeline-edits.v1';
const CLIP = 'senca.console.pipeline-clipboard.v1';

const read = (k, d) => {
  try { const v = JSON.parse(localStorage.getItem(k) || 'null'); return v ?? d; } catch { return d; }
};
const write = (k, v) => {
  try { localStorage.setItem(k, JSON.stringify(v)); return true; } catch { return false; }
};

/** 下書き: [{key, name, graph, positions, updated_at, template?}] */
export const drafts = {
  all: () => read(DRAFTS, []).filter((d) => d && d.key && d.graph),
  get: (key) => drafts.all().find((d) => d.key === key) || null,
  put: (d) => write(DRAFTS, [{ ...d, updated_at: new Date().toISOString() }, ...drafts.all().filter((x) => x.key !== d.key)]),
  remove: (key) => write(DRAFTS, drafts.all().filter((x) => x.key !== key)),
};

/** 保存済みのパイプラインの部品の位置: {id: {部品名: [x, y]}} */
export const layouts = {
  get: (id) => read(LAYOUT, {})[id] || null,
  put: (id, pos) => { const all = read(LAYOUT, {}); all[id] = pos; write(LAYOUT, all); },
  remove: (id) => { const all = read(LAYOUT, {}); delete all[id]; write(LAYOUT, all); },
};

/** 保存済みのパイプラインの、まだ保存していない変更: {id: {name, graph, positions, generation, at}} */
export const edits = {
  get: (id) => read(EDITS, {})[id] || null,
  put: (id, v) => { const all = read(EDITS, {}); all[id] = { ...v, at: new Date().toISOString() }; write(EDITS, all); },
  remove: (id) => { const all = read(EDITS, {}); delete all[id]; write(EDITS, all); },
  ids: () => Object.keys(read(EDITS, {})),
};

/** 写した部品（別のパイプラインにも貼り付けられる）。 */
export const clipboard = {
  get: () => read(CLIP, null),
  put: (v) => write(CLIP, v),
};
