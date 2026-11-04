// 監査ログの操作（`stream.create` など）→ 画面の言葉・アイコン・資源のページ。
// 知らない操作（新しく増えたもの）は、資源の種類が分かれば「配信: rotate_key」のように、分からなければ ID のまま出す。

import { t, has } from '../i18n/index.js';

/** 操作の種類の頭（`stream`・`room.participant` など）→ 分類。分類は絞り込みの選択肢とアイコンに使う。 */
export const CATEGORIES = [
  { id: 'stream', prefix: 'stream.', icon: 'streams', names: 'stream' },
  { id: 'recording', prefix: 'recording.', icon: 'recordings', names: 'recording' },
  { id: 'schedule', prefix: 'schedule.', icon: 'schedules', names: 'schedule' },
  { id: 'input', prefix: 'input.', icon: 'download', names: null },
  { id: 'output', prefix: 'output.', icon: 'upload', names: null },
  { id: 'pipeline', prefix: 'pipeline.', icon: 'pipelines', names: 'pipeline' },
  { id: 'room', prefix: 'room.', icon: 'rooms', names: null },
  { id: 'media_pipeline', prefix: 'media_pipeline.', icon: 'media-pipelines', names: null },
  { id: 'token', prefix: 'token.', icon: 'key', names: 'token' },
  { id: 'secret', prefix: 'secret.', icon: 'secrets', names: 'secret' },
  { id: 'node', prefix: 'node.', icon: 'nodes', names: 'node' },
  { id: 'device', prefix: 'device.', icon: 'gpu', names: null },
  { id: 'user', prefix: 'user.', icon: 'users', names: null },
  { id: 'auth', prefix: 'auth.', icon: 'login', names: null },
];

/** 操作の分類（知らなければ null）。`media_pipeline.` を `media.` より先に見る。 */
export function categoryOf(action) {
  const a = String(action || '');
  let best = null;
  for (const c of CATEGORIES) if (a.startsWith(c.prefix) && (!best || c.prefix.length > best.prefix.length)) best = c;
  return best;
}

/** 操作の言葉（audit.action.<操作>）。無ければ「分類: 残り」、分類も無ければ ID のまま。 */
export function actionLabel(action) {
  const key = `audit.action.${action}`;
  if (has(key)) return t(key);
  const c = categoryOf(action);
  if (c) return t('audit.action.unknownIn', { category: t(`audit.cat.${c.id}`), verb: String(action).slice(c.prefix.length) });
  return String(action);
}

/** 操作が資源を消したか（消した資源へのリンクは出さない）。 */
export function isDelete(action) {
  return /\.(delete|disconnect)$/.test(String(action || ''));
}

/**
 * 資源のページ（あれば）。resource は監査ログの `resource`（装置・参加者は `{ノード}/{装置}` の形）。
 * 消した操作には出さない。
 */
export function resourceHref(action, resource) {
  if (!resource || isDelete(action)) return null;
  const c = categoryOf(action);
  if (!c) return null;
  const id = encodeURIComponent(resource);
  switch (c.id) {
    case 'stream': return `#/streams/${id}`;
    case 'recording': return `#/recordings/${id}`;
    case 'schedule': return `#/schedules/${id}`;
    case 'pipeline': return `#/pipelines?id=${id}`;
    case 'room': return `#/rooms/${encodeURIComponent(String(resource).split('/')[0])}`;
    case 'media_pipeline': return '#/media-pipelines';
    case 'token': return '#/tokens';
    case 'secret': return '#/secrets';
    case 'node': return '#/nodes';
    case 'device': return '#/nodes';
    case 'user': return '#/users';
    default: return null;
  }
}

/** 主体（`token:{ID}`・JWT の sub）から、トークンの ID を取り出す（トークンでなければ null）。 */
export function subjectTokenId(subject) {
  const m = /^token:(.+)$/.exec(String(subject || ''));
  return m ? m[1] : null;
}
