// 表示の書式と時刻の変換。

const dtf = new Intl.DateTimeFormat('ja-JP', {
  year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit',
});

/** RFC 3339 → 利用者の地域の時刻表示。 */
export function fmtTime(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return String(iso);
  return dtf.format(d);
}

export function fmtTimeIn(iso, timeZone) {
  if (!iso) return '—';
  try {
    return new Intl.DateTimeFormat('ja-JP', {
      timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit',
    }).format(new Date(iso));
  } catch {
    return fmtTime(iso);
  }
}

export function fmtRelative(iso) {
  if (!iso) return '';
  const diff = (new Date(iso).getTime() - Date.now()) / 1000;
  const abs = Math.abs(diff);
  const rtf = new Intl.RelativeTimeFormat('ja', { numeric: 'auto' });
  if (abs < 60) return rtf.format(Math.round(diff), 'second');
  if (abs < 3600) return rtf.format(Math.round(diff / 60), 'minute');
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), 'hour');
  return rtf.format(Math.round(diff / 86400), 'day');
}

export function fmtBytes(n) {
  if (n === null || n === undefined) return '—';
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i += 1; }
  return `${v.toFixed(i ? 1 : 0)} ${u[i]}`;
}

export function fmtBps(n) {
  if (n === null || n === undefined) return '—';
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)} Mbps`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)} kbps`;
  return `${n} bps`;
}

export function fmtDuration(ms) {
  if (ms === null || ms === undefined) return '—';
  const s = Math.floor(ms / 1000);
  const hh = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  const p = (x) => String(x).padStart(2, '0');
  return hh ? `${hh}:${p(mm)}:${p(ss)}` : `${mm}:${p(ss)}`;
}

/** 利用者の地域のタイムゾーン名。 */
export function localZone() {
  try { return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'; } catch { return 'UTC'; }
}

/** 選べるタイムゾーン（短い一覧）。 */
export function zoneChoices() {
  const base = ['Asia/Tokyo', 'UTC', 'Asia/Seoul', 'Asia/Shanghai', 'Asia/Singapore', 'Australia/Sydney',
    'Europe/London', 'Europe/Berlin', 'America/New_York', 'America/Chicago', 'America/Los_Angeles'];
  const local = localZone();
  return base.includes(local) ? base : [local, ...base];
}

/** ある時刻（UTC ミリ秒）でのタイムゾーンの UTC からのずれ（ミリ秒）。 */
function zoneOffset(ms, timeZone) {
  const parts = new Intl.DateTimeFormat('en-US', {
    timeZone, hourCycle: 'h23', year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit',
  }).formatToParts(new Date(ms));
  const g = (t) => Number(parts.find((p) => p.type === t).value);
  const asUtc = Date.UTC(g('year'), g('month') - 1, g('day'), g('hour') % 24, g('minute'), g('second'));
  return asUtc - Math.floor(ms / 1000) * 1000;
}

/**
 * `datetime-local` の値（壁時計の時刻）を、指定のタイムゾーンで解釈して UTC の RFC 3339 にする。
 * 夏時間で存在しない時刻は後ろへずらし、重複する時刻は早い方を選ぶ。
 */
export function wallToUtcIso(value, timeZone) {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(value || '');
  if (!m) return null;
  const wall = Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +(m[6] || 0));
  // 前後 1 日のずれで候補を作り、壁時計に戻るものの早い方を選ぶ
  const before = wall - zoneOffset(wall - 86400_000, timeZone);
  const after = wall - zoneOffset(wall + 86400_000, timeZone);
  const ok = [before, after].filter((t) => t + zoneOffset(t, timeZone) === wall);
  // どちらも戻らなければ存在しない時刻（夏時間の開始）: 切り替え前のずれで解釈する（後ろへずれる）
  const t = ok.length ? Math.min(...ok) : before;
  return new Date(t).toISOString();
}

/** UTC の時刻 → 指定タイムゾーンの `datetime-local` の値。 */
export function utcToWall(iso, timeZone) {
  const ms = typeof iso === 'number' ? iso : new Date(iso).getTime();
  const d = new Date(ms + zoneOffset(ms, timeZone));
  return d.toISOString().slice(0, 16);
}

export const WEEKDAYS = ['日', '月', '火', '水', '木', '金', '土'];

/** POST の再送で同じ答えを得るための鍵。 */
export function idemKey() {
  if (crypto.randomUUID) return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}
