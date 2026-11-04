// 最近開いた資源（コマンドパレットの「最近」）。名前と経路だけを localStorage に置く（秘密は置かない）。

const STORE = 'senca.console.recent';
const MAX = 8;

export function recentItems() {
  try {
    const v = JSON.parse(localStorage.getItem(STORE) || '[]');
    return Array.isArray(v) ? v.filter((x) => x && x.href && x.label) : [];
  } catch {
    return [];
  }
}

/** 開いた資源を覚える。kind は 'stream'・'recording'・'schedule'・'pipeline' など（アイコンと種類の名前に使う）。 */
export function rememberRecent({ kind, id, label, href }) {
  const items = recentItems().filter((x) => x.href !== href);
  items.unshift({ kind, id, label, href, at: Date.now() });
  try { localStorage.setItem(STORE, JSON.stringify(items.slice(0, MAX))); } catch { /* 覚えなくても困らない */ }
}

export function forgetRecent(href) {
  try { localStorage.setItem(STORE, JSON.stringify(recentItems().filter((x) => x.href !== href))); } catch { /* 何もしない */ }
}
