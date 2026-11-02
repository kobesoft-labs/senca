// HLS のプレイリスト（RFC 8216）の最小限の解析。LL-HLS の part は読み飛ばし、完成したセグメントだけを使う。

function attrs(s) {
  const out = {};
  const re = /([A-Z0-9-]+)=("[^"]*"|[^,]*)/g;
  let m;
  while ((m = re.exec(s))) out[m[1]] = m[2].startsWith('"') ? m[2].slice(1, -1) : m[2];
  return out;
}

function byterange(s) {
  const m = /^(\d+)(?:@(\d+))?$/.exec(s || '');
  return m ? { length: Number(m[1]), offset: m[2] !== undefined ? Number(m[2]) : null } : null;
}

/**
 * @param {string} text
 * @param {string} baseUrl
 * @returns {{master: boolean, variants: Array, targetDuration: number, mediaSequence: number,
 *   discontinuitySequence: number, endList: boolean, playlistType: string|null,
 *   segments: Array<{uri:string, duration:number, msn:number, disc:number, start:number, map:{uri:string, range:object|null}|null, range:object|null}>}}
 */
export function parsePlaylist(text, baseUrl) {
  const lines = text.split(/\r?\n/);
  if (lines[0].trim() !== '#EXTM3U') throw new Error('not an HLS playlist');
  const pl = {
    master: false, variants: [], targetDuration: 6, mediaSequence: 0, discontinuitySequence: 0,
    endList: false, playlistType: null, segments: [], partTarget: null,
  };
  const resolve = (u) => new URL(u, baseUrl).toString();
  let map = null;
  let dur = null;
  let disc = 0;
  let pendingDisc = false;
  let range = null;
  let lastRangeEnd = 0;
  let pendingVariant = null;
  let start = 0;
  for (const raw of lines.slice(1)) {
    const line = raw.trim();
    if (!line) continue;
    if (line.startsWith('#')) {
      const c = line.indexOf(':');
      const tag = c === -1 ? line : line.slice(0, c);
      const val = c === -1 ? '' : line.slice(c + 1);
      switch (tag) {
        case '#EXT-X-STREAM-INF': pl.master = true; pendingVariant = attrs(val); break;
        case '#EXT-X-TARGETDURATION': pl.targetDuration = Number(val) || 6; break;
        case '#EXT-X-MEDIA-SEQUENCE': pl.mediaSequence = Number(val) || 0; break;
        case '#EXT-X-DISCONTINUITY-SEQUENCE': pl.discontinuitySequence = Number(val) || 0; break;
        case '#EXT-X-ENDLIST': pl.endList = true; break;
        case '#EXT-X-PLAYLIST-TYPE': pl.playlistType = val; break;
        case '#EXT-X-PART-INF': pl.partTarget = Number(attrs(val)['PART-TARGET']) || null; break;
        case '#EXT-X-MAP': {
          const a = attrs(val);
          map = { uri: resolve(a.URI), range: a.BYTERANGE ? byterange(a.BYTERANGE) : null };
          break;
        }
        case '#EXTINF': dur = parseFloat(val); break;
        case '#EXT-X-DISCONTINUITY': pendingDisc = true; break;
        case '#EXT-X-BYTERANGE': range = byterange(val); break;
        default: break; // PART・PRELOAD-HINT・PROGRAM-DATE-TIME などは使わない
      }
      continue;
    }
    if (pendingVariant) {
      pl.variants.push({
        uri: resolve(line), bandwidth: Number(pendingVariant.BANDWIDTH) || 0,
        codecs: pendingVariant.CODECS || '', resolution: pendingVariant.RESOLUTION || '',
      });
      pendingVariant = null;
      continue;
    }
    if (dur === null) continue;
    if (pendingDisc) { disc += 1; pendingDisc = false; }
    if (range && range.offset === null) range.offset = lastRangeEnd;
    if (range) lastRangeEnd = range.offset + range.length;
    pl.segments.push({
      uri: resolve(line), duration: dur, msn: pl.mediaSequence + pl.segments.length,
      disc: pl.discontinuitySequence + disc, start, map, range,
    });
    start += dur;
    dur = null;
    range = null;
  }
  return pl;
}

/** プレイリストのコンテナの種類（fmp4・ts・不明）。 */
export function containerOf(pl) {
  const s = pl.segments[0];
  if (!s) return 'unknown';
  if (s.map) return 'fmp4';
  const path = new URL(s.uri).pathname;
  if (/\.(m4s|mp4|m4v|m4a|cmfv|cmfa)$/i.test(path)) return 'fmp4';
  if (/\.ts$/i.test(path)) return 'ts';
  return 'unknown';
}
