// fMP4 の最小限の解析: 初期化セグメントからトラック（ID・timescale・コーデック文字列）、
// メディアセグメントから最初の tfdt（秒）を読む。MSE の SourceBuffer を作るのに使う。

const CONTAINERS = new Set(['moov', 'trak', 'mdia', 'minf', 'stbl', 'mvex', 'moof', 'traf', 'edts', 'dinf']);

function type4(v, o) {
  return String.fromCharCode(v.getUint8(o), v.getUint8(o + 1), v.getUint8(o + 2), v.getUint8(o + 3));
}

/** [start, end) にある箱を順に返す。 */
export function* boxes(v, start = 0, end = v.byteLength) {
  let o = start;
  while (o + 8 <= end) {
    let size = v.getUint32(o);
    const type = type4(v, o + 4);
    let hdr = 8;
    if (size === 1) {
      if (o + 16 > end) return;
      size = Number(v.getBigUint64(o + 8));
      hdr = 16;
    } else if (size === 0) {
      size = end - o;
    }
    if (size < hdr || o + size > end) return;
    yield { type, start: o, body: o + hdr, end: o + size };
    o += size;
  }
}

function find(v, start, end, path) {
  const [head, ...rest] = path;
  for (const b of boxes(v, start, end)) {
    if (b.type === head) return rest.length ? find(v, b.body, b.end, rest) : b;
  }
  return null;
}

function findAll(v, start, end, type) {
  const out = [];
  for (const b of boxes(v, start, end)) if (b.type === type) out.push(b);
  return out;
}

const hex2 = (n) => n.toString(16).toUpperCase().padStart(2, '0');

function avcCodec(v, b, type) {
  const c = find(v, b.start + 8 + 78, b.end, ['avcC']);
  if (!c) return type;
  const p = (i) => v.getUint8(c.body + i).toString(16).padStart(2, '0');
  return `${type}.${p(1)}${p(2)}${p(3)}`;
}

function hevcCodec(v, b, type) {
  const c = find(v, b.start + 8 + 78, b.end, ['hvcC']);
  if (!c) return type;
  const o = c.body;
  const b1 = v.getUint8(o + 1);
  const space = ['', 'A', 'B', 'C'][b1 >> 6];
  const tier = (b1 >> 5) & 1 ? 'H' : 'L';
  const profile = b1 & 0x1f;
  // 互換フラグはビットの並びを逆にして 16 進
  let compat = v.getUint32(o + 2);
  let rev = 0;
  for (let i = 0; i < 32; i += 1) { rev = (rev << 1) | (compat & 1); compat >>>= 1; }
  rev >>>= 0;
  const level = v.getUint8(o + 12);
  const cons = [];
  for (let i = 0; i < 6; i += 1) cons.push(v.getUint8(o + 6 + i));
  while (cons.length && cons[cons.length - 1] === 0) cons.pop();
  const cstr = cons.map((x) => x.toString(16).toUpperCase()).join('.');
  return `${type}.${space}${profile}.${rev.toString(16).toUpperCase()}.${tier}${level}${cstr ? `.${cstr}` : ''}`;
}

function av1Codec(v, b) {
  const c = find(v, b.start + 8 + 78, b.end, ['av1C']);
  if (!c) return 'av01';
  const b1 = v.getUint8(c.body + 1);
  const b2 = v.getUint8(c.body + 2);
  const profile = b1 >> 5;
  const level = b1 & 0x1f;
  const tier = (b2 >> 7) & 1 ? 'H' : 'M';
  const high = (b2 >> 6) & 1;
  const twelve = (b2 >> 5) & 1;
  const depth = high ? (twelve ? 12 : 10) : 8;
  return `av01.${profile}.${String(level).padStart(2, '0')}${tier}.${String(depth).padStart(2, '0')}`;
}

function vp9Codec(v, b) {
  const c = find(v, b.start + 8 + 78, b.end, ['vpcC']);
  if (!c) return 'vp09.00.10.08';
  const o = c.body + 4; // FullBox
  const profile = v.getUint8(o);
  const level = v.getUint8(o + 1);
  const depth = v.getUint8(o + 2) >> 4;
  const p2 = (n) => String(n).padStart(2, '0');
  return `vp09.${p2(profile)}.${p2(level)}.${p2(depth)}`;
}

function audioHeader(v, b) {
  // AudioSampleEntry: 版 0 は 28 バイト、QuickTime の版 1 は +16、版 2 は +36
  const ver = v.getUint16(b.start + 8 + 8);
  return b.start + 8 + 28 + (ver === 1 ? 16 : ver === 2 ? 36 : 0);
}

function readDescLen(v, o) {
  let len = 0;
  let i = 0;
  for (; i < 4; i += 1) {
    const x = v.getUint8(o + i);
    len = (len << 7) | (x & 0x7f);
    if (!(x & 0x80)) break;
  }
  return { len, size: i + 1 };
}

function mp4aCodec(v, b) {
  const esds = find(v, audioHeader(v, b), b.end, ['esds']);
  if (!esds) return 'mp4a.40.2';
  let o = esds.body + 4;
  try {
    if (v.getUint8(o) !== 0x03) return 'mp4a.40.2';
    o += 1 + readDescLen(v, o + 1).size;
    o += 2;
    const flags = v.getUint8(o); o += 1;
    if (flags & 0x80) o += 2;
    if (flags & 0x40) o += 1 + v.getUint8(o);
    if (flags & 0x20) o += 2;
    if (v.getUint8(o) !== 0x04) return 'mp4a.40.2';
    o += 1 + readDescLen(v, o + 1).size;
    const oti = v.getUint8(o);
    o += 13;
    if (oti !== 0x40) return `mp4a.${hex2(oti).toLowerCase()}`;
    if (v.getUint8(o) !== 0x05) return 'mp4a.40.2';
    o += 1 + readDescLen(v, o + 1).size;
    let aot = v.getUint8(o) >> 3;
    if (aot === 31) aot = 32 + (((v.getUint8(o) & 7) << 3) | (v.getUint8(o + 1) >> 5));
    return `mp4a.40.${aot}`;
  } catch {
    return 'mp4a.40.2';
  }
}

function sampleEntryCodec(v, b) {
  switch (b.type) {
    case 'avc1': case 'avc3': return avcCodec(v, b, b.type);
    case 'hvc1': case 'hev1': return hevcCodec(v, b, b.type);
    case 'av01': return av1Codec(v, b);
    case 'vp09': return vp9Codec(v, b);
    case 'vp08': return 'vp8';
    case 'mp4a': return mp4aCodec(v, b);
    case 'Opus': return 'opus';
    case 'fLaC': return 'flac';
    case 'ac-3': return 'ac-3';
    case 'ec-3': return 'ec-3';
    default: return null;
  }
}

/**
 * 初期化セグメントを読む。
 * @returns {{tracks: Array<{id:number, timescale:number, kind:'video'|'audio'|'other', codec:string|null, entry:string, width?:number, height?:number}>}}
 */
export function parseInit(buf) {
  const v = new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer, buf.byteOffset || 0, buf.byteLength);
  const moov = find(v, 0, v.byteLength, ['moov']);
  if (!moov) throw new Error('init segment has no moov');
  const tracks = [];
  for (const trak of findAll(v, moov.body, moov.end, 'trak')) {
    const tkhd = find(v, trak.body, trak.end, ['tkhd']);
    const mdhd = find(v, trak.body, trak.end, ['mdia', 'mdhd']);
    const hdlr = find(v, trak.body, trak.end, ['mdia', 'hdlr']);
    const stsd = find(v, trak.body, trak.end, ['mdia', 'minf', 'stbl', 'stsd']);
    if (!tkhd || !mdhd || !hdlr || !stsd) continue;
    const tv = v.getUint8(tkhd.body);
    const id = v.getUint32(tkhd.body + (tv === 1 ? 20 : 12));
    const mv = v.getUint8(mdhd.body);
    const timescale = v.getUint32(mdhd.body + (mv === 1 ? 20 : 12));
    const handler = type4(v, hdlr.body + 8);
    const kind = handler === 'vide' ? 'video' : handler === 'soun' ? 'audio' : 'other';
    // stsd: FullBox(4) + entry_count(4) の後に sample entry
    const entry = [...boxes(v, stsd.body + 8, stsd.end)][0];
    const t = { id, timescale, kind, codec: entry ? sampleEntryCodec(v, entry) : null, entry: entry ? entry.type : '' };
    if (entry && kind === 'video') {
      t.width = v.getUint16(entry.start + 8 + 24);
      t.height = v.getUint16(entry.start + 8 + 26);
    }
    tracks.push(t);
  }
  return { tracks };
}

/** メディアセグメントの最初の decode time（秒、トラックの中で最小）。読めなければ null。 */
export function firstDecodeTime(buf, init) {
  const v = new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer, buf.byteOffset || 0, buf.byteLength);
  let best = null;
  for (const moof of boxes(v, 0, v.byteLength)) {
    if (moof.type !== 'moof') continue;
    for (const traf of findAll(v, moof.body, moof.end, 'traf')) {
      const tfhd = find(v, traf.body, traf.end, ['tfhd']);
      const tfdt = find(v, traf.body, traf.end, ['tfdt']);
      if (!tfhd || !tfdt) continue;
      const id = v.getUint32(tfhd.body + 4);
      const ver = v.getUint8(tfdt.body);
      const t = ver === 1 ? Number(v.getBigUint64(tfdt.body + 4)) : v.getUint32(tfdt.body + 4);
      const tr = init.tracks.find((x) => x.id === id);
      if (!tr || !tr.timescale) continue;
      const sec = t / tr.timescale;
      if (best === null || sec < best) best = sec;
    }
    break; // 最初の moof だけ
  }
  return best;
}

/** SourceBuffer に渡す MIME。 */
export function mimeFor(init) {
  const codecs = init.tracks.filter((t) => t.kind !== 'other' && t.codec).map((t) => t.codec);
  const hasVideo = init.tracks.some((t) => t.kind === 'video');
  return `${hasVideo ? 'video' : 'audio'}/mp4; codecs="${codecs.join(',')}"`;
}

// ---------------------------------------------------------------- 幅・高さが 0 の初期化セグメントの補正
//
// 送り手によっては（Enhanced RTMP の AV1 など）初期化セグメントの幅・高さが 0 のことがあり、
// Chrome は「Invalid video decoder config: coded size [0,0]」で拒否する。最初のメディアセグメントの
// キーフレームにある AV1 の sequence header OBU から大きさを読み、初期化セグメントを書き換える。

class Bits {
  constructor(u8, start, end) { this.u8 = u8; this.pos = start * 8; this.end = end * 8; }
  bit() {
    if (this.pos >= this.end) throw new Error('eof');
    const b = (this.u8[this.pos >> 3] >> (7 - (this.pos & 7))) & 1;
    this.pos += 1;
    return b;
  }
  f(n) { let v = 0; for (let i = 0; i < n; i += 1) v = v * 2 + this.bit(); return v; }
  uvlc() {
    let lz = 0;
    while (!this.bit()) { lz += 1; if (lz >= 32) return 2 ** 32 - 1; }
    return this.f(lz) + 2 ** lz - 1;
  }
}

function leb128(u8, o) {
  let v = 0;
  for (let i = 0; i < 8; i += 1) {
    const b = u8[o + i];
    v += (b & 0x7f) * 2 ** (7 * i);
    if (!(b & 0x80)) return { v, n: i + 1 };
  }
  return { v, n: 8 };
}

/** AV1 の sequence header OBU（payload）から max_frame_width・height を読む。 */
export function av1SequenceSize(u8, start, end) {
  const r = new Bits(u8, start, end);
  r.f(3); // seq_profile
  r.f(1); // still_picture
  const reduced = r.f(1);
  if (reduced) {
    r.f(5);
  } else {
    let decoderModel = 0;
    let bufferDelayLen = 0;
    if (r.f(1)) { // timing_info_present_flag
      r.f(32); r.f(32);
      if (r.f(1)) r.uvlc();
      decoderModel = r.f(1);
      if (decoderModel) {
        bufferDelayLen = r.f(5) + 1;
        r.f(32); r.f(5); r.f(5);
      }
    }
    const initialDisplay = r.f(1);
    const ops = r.f(5) + 1;
    for (let i = 0; i < ops; i += 1) {
      r.f(12);
      const level = r.f(5);
      if (level > 7) r.f(1);
      if (decoderModel && r.f(1)) { r.f(bufferDelayLen); r.f(bufferDelayLen); r.f(1); }
      if (initialDisplay && r.f(1)) r.f(4);
    }
  }
  const wb = r.f(4) + 1;
  const hb = r.f(4) + 1;
  return { width: r.f(wb) + 1, height: r.f(hb) + 1 };
}

/** 範囲の中の OBU を順に見て、sequence header の大きさを返す。 */
function findAv1Size(u8, start, end) {
  let o = start;
  while (o < end) {
    const hdr = u8[o];
    const type = (hdr >> 3) & 0xf;
    const ext = (hdr >> 2) & 1;
    const hasSize = (hdr >> 1) & 1;
    let p = o + 1 + ext;
    let size = end - p;
    if (hasSize) { const l = leb128(u8, p); size = l.v; p += l.n; }
    if (type === 1) return av1SequenceSize(u8, p, Math.min(end, p + size));
    if (!hasSize) break;
    o = p + size;
  }
  return null;
}

/** メディアセグメントの、指定トラックの最初のサンプルの [start, end)。 */
function firstSampleRange(u8, init, trackId) {
  const v = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  for (const moof of boxes(v, 0, v.byteLength)) {
    if (moof.type !== 'moof') continue;
    for (const traf of findAll(v, moof.body, moof.end, 'traf')) {
      const tfhd = find(v, traf.body, traf.end, ['tfhd']);
      const trun = find(v, traf.body, traf.end, ['trun']);
      if (!tfhd || !trun || v.getUint32(tfhd.body + 4) !== trackId) continue;
      const tf = v.getUint32(tfhd.body) & 0xffffff;
      let p = tfhd.body + 8;
      let base = moof.start;
      if (tf & 0x1) { base = Number(v.getBigUint64(p)); p += 8; }
      if (tf & 0x2) p += 4;
      if (tf & 0x8) p += 4;
      const defSize = tf & 0x10 ? v.getUint32(p) : null;
      const rf = v.getUint32(trun.body) & 0xffffff;
      let q = trun.body + 8;
      const dataOffset = rf & 0x1 ? v.getInt32(q) : 0;
      if (rf & 0x1) q += 4;
      if (rf & 0x4) q += 4;
      if (rf & 0x100) q += 4;
      const size = rf & 0x200 ? v.getUint32(q) : defSize;
      if (!size) return null;
      const start = base + dataOffset;
      return [start, start + size];
    }
    break;
  }
  return null;
}

/**
 * 幅・高さが 0 の映像トラックを、メディアセグメントから読んだ大きさで補正した初期化セグメントを返す。
 * 補正できなければ（または不要なら）null。
 */
export function fixZeroSize(initBuf, init, segBuf) {
  const tracks = init.tracks.filter((t) => t.kind === 'video' && (!t.width || !t.height) && t.entry === 'av01');
  if (!tracks.length) return null;
  const out = new Uint8Array(initBuf);
  const v = new DataView(out.buffer, out.byteOffset, out.byteLength);
  const moov = find(v, 0, v.byteLength, ['moov']);
  let fixed = false;
  for (const t of tracks) {
    const range = firstSampleRange(segBuf, init, t.id);
    if (!range) continue;
    let size = null;
    try { size = findAv1Size(segBuf, range[0], Math.min(range[1], segBuf.byteLength)); } catch { size = null; }
    if (!size) continue;
    for (const trak of findAll(v, moov.body, moov.end, 'trak')) {
      const tkhd = find(v, trak.body, trak.end, ['tkhd']);
      const tv = v.getUint8(tkhd.body);
      if (v.getUint32(tkhd.body + (tv === 1 ? 20 : 12)) !== t.id) continue;
      const wOff = tkhd.body + (tv === 1 ? 88 : 76);
      v.setUint32(wOff, size.width * 65536);
      v.setUint32(wOff + 4, size.height * 65536);
      const stsd = find(v, trak.body, trak.end, ['mdia', 'minf', 'stbl', 'stsd']);
      const entry = [...boxes(v, stsd.body + 8, stsd.end)][0];
      v.setUint16(entry.start + 8 + 24, size.width);
      v.setUint16(entry.start + 8 + 26, size.height);
      t.width = size.width;
      t.height = size.height;
      fixed = true;
    }
  }
  return fixed ? out : null;
}
