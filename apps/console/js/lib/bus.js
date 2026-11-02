// 画面の間で知らせ合うための小さなイベントの通り道。
//
// 種類:
// - `sse`        SSE のイベント（detail = {id, type, data}）
// - `resync`     取りこぼした。一覧を読み直す
// - `sse-status` 接続の状態（detail = {state: 'connecting'|'open'|'closed'|'forbidden', retryIn}）
// - `unauthorized` 401（ログイン画面へ戻す）

const target = new EventTarget();

export function emit(type, detail) {
  target.dispatchEvent(new CustomEvent(type, { detail }));
}

/** 購読する。戻り値は解除する関数。 */
export function on(type, fn) {
  const l = (e) => fn(e.detail);
  target.addEventListener(type, l);
  return () => target.removeEventListener(type, l);
}
