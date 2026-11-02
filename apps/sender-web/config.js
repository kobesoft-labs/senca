// 送り先の設定文字列を読む（crates/senca-sdk/src/uri.rs と同じ規則の WHIP 部分）。
//
// 受け付ける形:
//   senca://ingest?v=1&proto=whip&url=https://h/whip/ID&token=...&name=...
//   https://h/whip/ID（token は別の欄で）
// 規則: v=1 必須、知らない鍵は拒否（x- で始まる鍵は読み飛ばす）、重複・空の値は拒否、
//       URL は https://（http:// は localhost・127.0.0.1・::1 だけ）、userinfo と fragment は拒否。
// ブラウザでは WHIP だけを送れる。srt / rtmp の設定は理由付きで断る。

const MAX_LEN = 2048;

export class ConfigError extends Error {}

function decodeValue(key, v) {
  if (/%(?![0-9A-Fa-f]{2})/.test(v)) throw new ConfigError(`'${key}' の % の書き方が正しくありません`);
  let s;
  try {
    s = decodeURIComponent(v);
  } catch {
    throw new ConfigError(`'${key}' が UTF-8 ではありません`);
  }
  // 制御文字は拒否する
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(s)) throw new ConfigError(`'${key}' に制御文字があります`);
  return s;
}

function isLoopback(host) {
  return host === 'localhost' || host === '[::1]' || /^127\.\d+\.\d+\.\d+$/.test(host);
}

/** WHIP の URL を検査して正規の文字列にする。 */
export function checkWhipUrl(s) {
  let u;
  if (!/^[\x21-\x7e]+$/.test(s)) throw new ConfigError('URL に空白や ASCII 以外の文字があります');
  try {
    u = new URL(s);
  } catch {
    throw new ConfigError('URL の形が正しくありません');
  }
  if (u.username || u.password) throw new ConfigError('URL に利用者名・パスワードは入れられません');
  if (u.hash || s.includes('#')) throw new ConfigError('URL に # 以降は入れられません');
  if (u.protocol === 'http:' && !isLoopback(u.hostname)) {
    throw new ConfigError('WHIP は https:// が必要です（http:// は同じ端末の試験だけ）');
  }
  if (u.protocol !== 'https:' && u.protocol !== 'http:') throw new ConfigError('WHIP の URL は https:// です');
  if (u.pathname.replace(/\//g, '') === '') throw new ConfigError('URL にパスがありません');
  return u.href;
}

/**
 * 設定文字列を読む。戻り値: { url, token, name }
 * @param {string} input
 */
export function parseIngest(input) {
  const s = (input || '').trim();
  if (s.length > MAX_LEN) throw new ConfigError(`設定が長すぎます（${MAX_LEN} バイトまで）`);
  const lower = s.slice(0, 15).toLowerCase();
  if (lower === 'senca://ingest?') {
    const fields = new Map();
    for (const part of s.slice(15).split('&')) {
      const i = part.indexOf('=');
      if (i < 0) throw new ConfigError('設定の項目に = がありません');
      const k = decodeValue('key', part.slice(0, i));
      if (!/^[a-z0-9_-]{1,32}$/.test(k)) throw new ConfigError('設定の鍵の名前が正しくありません');
      const v = decodeValue(k, part.slice(i + 1));
      if (v === '') throw new ConfigError(`'${k}' が空です`);
      if (fields.has(k)) throw new ConfigError(`'${k}' が重複しています`);
      fields.set(k, v);
    }
    const take = (k) => {
      const v = fields.get(k);
      fields.delete(k);
      return v;
    };
    const v = take('v');
    if (v === undefined) throw new ConfigError("'v' がありません");
    if (v !== '1') throw new ConfigError('この版の設定には対応していません（v=1 だけ）');
    const proto = take('proto');
    if (proto === undefined) throw new ConfigError("'proto' がありません");
    if (proto === 'srt' || proto === 'rtmp') {
      throw new ConfigError(`ブラウザからは ${proto.toUpperCase()} で送れません。WHIP の設定を使うか、アプリを使ってください`);
    }
    if (proto !== 'whip') throw new ConfigError("'proto' は srt・rtmp・whip のどれかです");
    const url = take('url');
    if (url === undefined) throw new ConfigError("'url' がありません");
    const token = take('token');
    if (token !== undefined && !/^[\x21-\x7e]{1,4096}$/.test(token)) {
      throw new ConfigError('token は空白のない ASCII で 4096 文字までです');
    }
    const name = take('name');
    if (name !== undefined && [...name].length > 64) throw new ConfigError('name は 64 文字までです');
    for (const k of fields.keys()) {
      if (!k.startsWith('x-')) throw new ConfigError(`知らない鍵 '${k}' があります`);
    }
    return { url: checkWhipUrl(url), token: token ?? '', name: name ?? '' };
  }
  const scheme = s.split('://')[0].toLowerCase();
  if (scheme === 'srt' || scheme === 'rtmp' || scheme === 'rtmps') {
    throw new ConfigError(`ブラウザからは ${scheme.toUpperCase()} で送れません`);
  }
  if (scheme === 'https' || scheme === 'http') return { url: checkWhipUrl(s), token: '', name: '' };
  throw new ConfigError('設定の形が分かりません（senca://ingest?... か https://... ）');
}
