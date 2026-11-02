// Web のスイッチャー（REQ-008 R8-11）: プレビュー／プログラム、シーン、トランジション（カット・フェード）、
// 音声ミキサー（フェーダー・ミュート・遅延・メーター）。
//
// API（/productions）はまだノードに無い。操作は api/pipelines.js の productions を呼び、404 なら
// 「このノードでは使えません」を出して、この画面の中の状態だけで動く（操作の流れを確かめられる）。
// モニターはシーンの配置を描いた模式図で、実際の映像ではない。メーターも模擬の値。
//
// キーボード: 1〜9 = そのシーンをプレビューへ、C = カット、F = フェード（入力欄にいないとき）。

import { h, replace, field, select } from '../lib/dom.js';
import { toast, reportError, modal } from '../lib/ui.js';
import { productions as api, probe, NotAvailableError } from '../api/pipelines.js';
import { streams, listAll } from '../api/resources.js';

const STORE = 'senca.console.production.v1';
const PRODUCTION_ID = 'local';

const SOURCE_KINDS = {
  camera: 'ライブ入力',
  color: '色',
  text: '文字（テロップ）',
  clock: '時計',
  image: '画像（枠だけ）',
};

function defaultState() {
  return {
    scenes: [
      { id: 's1', name: 'カメラ A', sources: [
        { id: 'a', kind: 'camera', label: 'カメラ A', color: '#2f6f9f', x: 0, y: 0, w: 1, h: 1, visible: true },
        { id: 'b', kind: 'text', label: 'SENCA ライブ', color: '#ffffff', x: 0.05, y: 0.82, w: 0.6, h: 0.1, visible: true },
      ] },
      { id: 's2', name: 'カメラ B', sources: [
        { id: 'c', kind: 'camera', label: 'カメラ B', color: '#7a4f9f', x: 0, y: 0, w: 1, h: 1, visible: true },
      ] },
      { id: 's3', name: 'PinP', sources: [
        { id: 'd', kind: 'camera', label: 'スライド', color: '#3c7a4f', x: 0, y: 0, w: 1, h: 1, visible: true },
        { id: 'e', kind: 'camera', label: 'カメラ A', color: '#2f6f9f', x: 0.66, y: 0.64, w: 0.3, h: 0.3, visible: true },
        { id: 'f', kind: 'clock', label: '時計', color: '#ffffff', x: 0.04, y: 0.04, w: 0.25, h: 0.08, visible: true },
      ] },
      { id: 's4', name: '休憩', sources: [
        { id: 'g', kind: 'color', label: '背景', color: '#1d2330', x: 0, y: 0, w: 1, h: 1, visible: true },
        { id: 'h', kind: 'text', label: 'しばらくお待ちください', color: '#ffd166', x: 0.2, y: 0.42, w: 0.6, h: 0.14, visible: true },
      ] },
    ],
    preview: 's2',
    program: 's1',
    transition: { kind: 'fade', duration_ms: 500 },
    audio: [
      { id: 'cam_a', name: 'カメラ A', gain_db: 0, mute: false, delay_ms: 0 },
      { id: 'cam_b', name: 'カメラ B', gain_db: -6, mute: false, delay_ms: 0 },
      { id: 'mic', name: 'マイク', gain_db: 0, mute: false, delay_ms: 40 },
      { id: 'bgm', name: 'BGM', gain_db: -18, mute: true, delay_ms: 0 },
    ],
    master: { gain_db: 0, mute: false },
  };
}

function loadState() {
  try {
    const v = JSON.parse(localStorage.getItem(STORE) || 'null');
    if (v && Array.isArray(v.scenes) && v.scenes.length) return v;
  } catch { /* 壊れていれば最初から */ }
  return defaultState();
}

function drawScene(ctx, scene, alpha = 1) {
  const W = ctx.canvas.width;
  const H = ctx.canvas.height;
  ctx.save();
  ctx.globalAlpha = alpha;
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, W, H);
  if (scene) {
    for (const src of scene.sources) {
      if (!src.visible) continue;
      const x = src.x * W;
      const y = src.y * H;
      const w = src.w * W;
      const hh = src.h * H;
      if (src.kind === 'text' || src.kind === 'clock') {
        ctx.fillStyle = 'rgba(0,0,0,.45)';
        ctx.fillRect(x, y, w, hh);
        ctx.fillStyle = src.color;
        ctx.font = `bold ${Math.max(10, Math.round(hh * 0.6))}px system-ui, sans-serif`;
        ctx.textBaseline = 'middle';
        const text = src.kind === 'clock' ? new Date().toLocaleTimeString('ja-JP') : src.label;
        ctx.fillText(text, x + hh * 0.25, y + hh / 2, w - hh * 0.5);
      } else {
        ctx.fillStyle = src.color;
        ctx.fillRect(x, y, w, hh);
        if (src.kind !== 'color') {
          ctx.strokeStyle = 'rgba(255,255,255,.35)';
          ctx.lineWidth = 2;
          ctx.strokeRect(x + 1, y + 1, w - 2, hh - 2);
          ctx.fillStyle = 'rgba(255,255,255,.9)';
          ctx.font = `${Math.max(10, Math.round(Math.min(w, hh) * 0.09))}px system-ui, sans-serif`;
          ctx.textBaseline = 'bottom';
          ctx.fillText(`${src.kind === 'camera' ? '●' : '▣'} ${src.label}`, x + 8, y + hh - 6, w - 16);
        }
      }
    }
  }
  ctx.restore();
}

export async function render(root) {
  const st = loadState();
  let available = null;
  let fade = null; // {from, to, start, dur}
  let tbar = 0;
  let raf = 0;
  let streamList = [];
  const levels = new Map(); // メーターの模擬値

  const save = () => {
    try { localStorage.setItem(STORE, JSON.stringify(st)); } catch { /* 何もしない */ }
  };
  const scene = (id) => st.scenes.find((x) => x.id === id) || null;

  const banner = h('div');
  const renderBanner = () => replace(banner, available === false
    ? h('div', { class: 'alert warn offline-banner', role: 'status' }, h('strong', null, 'このノードでは使えません'),
      h('span', null, '制作の API（/productions）はまだ実装されていません。この画面はブラウザの中だけで動きます（モニターは配置の模式図、メーターは模擬）。'))
    : available === true ? h('div', { class: 'alert ok' }, '制作の API につながっています。') : null);

  /** API を呼ぶ。使えなければローカルだけで続ける。 */
  const remote = async (fn, what) => {
    if (available === false) return;
    try {
      await fn();
    } catch (e) {
      if (e instanceof NotAvailableError) {
        available = false;
        renderBanner();
        toast('制作の API はこのノードでは使えません。ローカルの操作だけを続けます', 'warn', 6000);
      } else reportError(e, what);
    }
  };

  // モニター
  const pvCanvas = h('canvas', { width: 640, height: 360, role: 'img', 'aria-label': 'プレビューのモニター' });
  const pgCanvas = h('canvas', { width: 640, height: 360, role: 'img', 'aria-label': 'プログラムのモニター' });
  const pvName = h('span');
  const pgName = h('span');
  const monitors = h('div', { class: 'sw-monitors' },
    h('div', { class: 'monitor preview' }, pvCanvas, h('span', { class: 'mlabel' }, 'PREVIEW ', pvName)),
    h('div', { class: 'monitor program' }, pgCanvas, h('span', { class: 'mlabel' }, 'PROGRAM ', pgName)));

  const frame = (now) => {
    const pv = pvCanvas.getContext('2d');
    const pg = pgCanvas.getContext('2d');
    drawScene(pv, scene(st.preview));
    if (fade) {
      const p = Math.min(1, (now - fade.start) / fade.dur);
      drawScene(pg, scene(fade.from));
      drawScene(pg, scene(fade.to), p);
      if (p >= 1) finishTransition();
    } else if (tbar > 0) {
      drawScene(pg, scene(st.program));
      drawScene(pg, scene(st.preview), tbar);
    } else drawScene(pg, scene(st.program));
    pvName.textContent = scene(st.preview) ? scene(st.preview).name : '';
    pgName.textContent = scene(st.program) ? scene(st.program).name : '';
    updateMeters();
    raf = requestAnimationFrame(frame);
  };

  // シーン
  const scenesBox = h('div', { class: 'sw-scenes', role: 'group', 'aria-label': 'シーン' });
  const renderScenes = () => {
    replace(scenesBox, st.scenes.map((sc, i) => {
      const b = h('button', {
        class: [sc.id === st.preview && 'preview', sc.id === st.program && 'program'],
        'aria-pressed': String(sc.id === st.preview),
        'aria-label': `${sc.name}${sc.id === st.program ? '（プログラム）' : ''}${sc.id === st.preview ? '（プレビュー）' : ''}`,
        onClick: () => setPreview(sc.id),
      }, h('strong', null, sc.name), h('span', { class: 'key' }, i < 9 ? `キー ${i + 1}` : ''),
      h('span', { class: 'key' }, sc.id === st.program ? '● PGM' : sc.id === st.preview ? '○ PVW' : ''));
      b.addEventListener('dblclick', () => editScene(sc));
      return b;
    }));
  };

  const setPreview = (id) => {
    if (!scene(id)) return;
    st.preview = id;
    save();
    renderScenes();
    remote(() => api.preview(PRODUCTION_ID, id), 'プレビュー');
  };

  const finishTransition = () => {
    if (!fade) return;
    const prevProgram = fade.from;
    st.program = fade.to;
    st.preview = prevProgram; // スタジオモードの通り、入れ替える
    fade = null;
    tbar = 0;
    tbarInput.value = '0';
    save();
    renderScenes();
  };

  const take = (kind) => {
    if (fade || st.preview === st.program) return;
    const dur = kind === 'cut' ? 0 : Math.max(50, Number(durInput.value) || st.transition.duration_ms);
    remote(() => api.transition(PRODUCTION_ID, kind, dur), 'トランジション');
    fade = { from: st.program, to: st.preview, start: performance.now(), dur: Math.max(1, dur) };
    if (kind === 'cut') finishTransition();
    live.textContent = `${kind === 'cut' ? 'カット' : 'フェード'}: ${scene(st.preview) ? scene(st.preview).name : ''} → プログラム`;
  };

  const durInput = h('input', { type: 'number', min: 50, max: 10000, step: 50, value: st.transition.duration_ms, style: { width: '90px' }, 'aria-label': 'フェードの時間（ミリ秒）' });
  durInput.addEventListener('change', () => { st.transition.duration_ms = Number(durInput.value) || 500; save(); });
  const tbarInput = h('input', { type: 'range', min: 0, max: 100, value: 0, 'aria-label': 'T バー（手動のフェード）', style: { width: '200px' } });
  tbarInput.addEventListener('input', () => {
    tbar = Number(tbarInput.value) / 100;
    if (tbar >= 1) {
      fade = { from: st.program, to: st.preview, start: performance.now() - 1, dur: 1 };
      remote(() => api.transition(PRODUCTION_ID, 'fade', 0), 'トランジション');
    }
  });
  tbarInput.addEventListener('change', () => {
    if (tbar < 1) { tbar = 0; tbarInput.value = '0'; }
  });
  const live = h('div', { class: 'visually-hidden', 'aria-live': 'polite' });
  const trans = h('div', { class: 'sw-trans' },
    h('button', { class: 'big danger solid', onClick: () => take('cut'), title: 'キー C' }, 'CUT'),
    h('button', { class: 'big primary', onClick: () => take('fade'), title: 'キー F' }, 'FADE'),
    h('label', { class: 'row small' }, '時間', durInput, 'ms'),
    h('label', { class: 'row small' }, 'T バー', tbarInput),
    live);

  // シーンの編集
  const editScene = async (sc) => {
    const copy = JSON.parse(JSON.stringify(sc));
    const list = h('div', { class: 'form' });
    const draw = () => {
      replace(list, copy.sources.map((src, i) => {
        const num = (k) => {
          const inp = h('input', { type: 'number', min: 0, max: 1, step: 0.01, value: src[k], style: { width: '72px' }, 'aria-label': `${src.label} の ${k}` });
          inp.addEventListener('change', () => { src[k] = Math.max(0, Math.min(1, Number(inp.value))); });
          return inp;
        };
        const label = h('input', { type: 'text', value: src.label, 'aria-label': 'ソースの名前' });
        label.addEventListener('input', () => { src.label = label.value; });
        const color = h('input', { type: 'color', value: src.color, 'aria-label': '色' });
        color.addEventListener('input', () => { src.color = color.value; });
        const vis = h('input', { type: 'checkbox', checked: src.visible, 'aria-label': '表示' });
        vis.addEventListener('change', () => { src.visible = vis.checked; });
        const kindSel = select([...Object.entries(SOURCE_KINDS), ...streamList.map((x) => [`stream:${x.id}`, `配信: ${x.name}`])], src.stream ? `stream:${src.stream}` : src.kind);
        kindSel.addEventListener('change', () => {
          if (kindSel.value.startsWith('stream:')) { src.kind = 'camera'; src.stream = kindSel.value.slice(7); src.label = (streamList.find((x) => x.id === src.stream) || {}).name || src.label; draw(); } else { src.kind = kindSel.value; delete src.stream; }
        });
        return h('fieldset', null, h('legend', null, `${i + 1}. ${src.label}`),
          h('div', { class: 'inline' }, kindSel, label, color, h('label', { class: 'check' }, vis, '表示')),
          h('div', { class: 'inline small' }, 'x', num('x'), 'y', num('y'), '幅', num('w'), '高さ', num('h')),
          h('div', { class: 'inline' },
            h('button', { type: 'button', class: 'small', disabled: i === 0, onClick: () => { copy.sources.splice(i - 1, 0, copy.sources.splice(i, 1)[0]); draw(); } }, '↑ 奥へ'),
            h('button', { type: 'button', class: 'small', disabled: i === copy.sources.length - 1, onClick: () => { copy.sources.splice(i + 1, 0, copy.sources.splice(i, 1)[0]); draw(); } }, '↓ 手前へ'),
            h('button', { type: 'button', class: 'small danger', onClick: () => { copy.sources.splice(i, 1); draw(); } }, '削除')));
      }));
    };
    draw();
    const name = h('input', { type: 'text', value: copy.name });
    const ok = await modal(`シーン「${sc.name}」`, () => [
      field('名前', name),
      h('p', { class: 'small muted' }, '位置と大きさは画面に対する割合（0〜1）。下のものほど手前に重なります。'),
      list,
      h('button', { type: 'button', onClick: () => { copy.sources.push({ id: `src${Date.now().toString(36)}`, kind: 'text', label: '新しいソース', color: '#ffffff', x: 0.1, y: 0.1, w: 0.4, h: 0.1, visible: true }); draw(); } }, '＋ ソースを追加'),
    ], {
      wide: true,
      actions: (close) => [
        h('button', { class: 'danger', onClick: () => close('delete') }, 'シーンを削除'),
        h('button', { onClick: () => close(null) }, 'やめる'),
        h('button', { class: 'primary', onClick: () => close('ok') }, '保存'),
      ],
    });
    if (ok === 'delete') {
      if (st.scenes.length <= 1 || sc.id === st.program) { toast('プログラムに出ているシーン・最後のシーンは削除できません', 'warn'); return; }
      st.scenes = st.scenes.filter((x) => x.id !== sc.id);
      if (st.preview === sc.id) st.preview = st.program;
    } else if (ok === 'ok') {
      copy.name = name.value || copy.name;
      Object.assign(sc, copy);
      remote(() => api.updateScene(PRODUCTION_ID, sc.id, sc, null), 'シーンの保存');
    } else return;
    save();
    renderScenes();
  };

  // 音声ミキサー
  const mixer = h('div', { class: 'mixer', role: 'group', 'aria-label': '音声ミキサー' });
  const meterEls = new Map();
  const strip = (ch, isMaster) => {
    const lvl = h('div', { class: 'lvl' });
    const peak = h('div', { class: 'peak' });
    const meter = h('div', { class: 'meter', role: 'meter', 'aria-label': `${ch.name} のレベル`, 'aria-valuemin': '-60', 'aria-valuemax': '0', 'aria-valuenow': '-60' }, lvl, peak);
    meterEls.set(ch.id, { lvl, peak, meter, peakV: 0, peakT: 0 });
    const db = h('div', { class: 'db' }, `${ch.gain_db > 0 ? '+' : ''}${ch.gain_db} dB`);
    const fader = h('input', { type: 'range', class: 'fader', min: -60, max: 10, step: 0.5, value: ch.gain_db, 'aria-label': `${ch.name} の音量（dB）`, 'aria-valuetext': `${ch.gain_db} dB` });
    const send = debounceSend(ch, isMaster);
    fader.addEventListener('input', () => {
      ch.gain_db = Number(fader.value);
      db.textContent = `${ch.gain_db > 0 ? '+' : ''}${ch.gain_db} dB`;
      fader.setAttribute('aria-valuetext', `${ch.gain_db} dB`);
      save();
      send();
    });
    fader.addEventListener('dblclick', () => { fader.value = '0'; fader.dispatchEvent(new Event('input')); });
    const mute = h('button', { class: ['small', ch.mute && 'muted-on'], 'aria-pressed': String(ch.mute) }, ch.mute ? 'ミュート中' : 'ミュート');
    mute.addEventListener('click', () => {
      ch.mute = !ch.mute;
      mute.classList.toggle('muted-on', ch.mute);
      mute.setAttribute('aria-pressed', String(ch.mute));
      mute.textContent = ch.mute ? 'ミュート中' : 'ミュート';
      save();
      send();
    });
    let delay = null;
    if (!isMaster) {
      delay = h('input', { type: 'number', min: 0, max: 2000, step: 10, value: ch.delay_ms, 'aria-label': `${ch.name} の遅延（ミリ秒）`, title: '映像との同期のための遅延（ミリ秒）' });
      delay.addEventListener('change', () => { ch.delay_ms = Math.max(0, Number(delay.value) || 0); save(); send(); });
    }
    return h('div', { class: ['strip', isMaster && 'master'] },
      h('div', { class: 'name', title: ch.name }, ch.name),
      h('div', { class: 'fader-row' }, meter, fader),
      db, mute,
      delay ? h('label', { class: 'small muted' }, '遅延 ms', delay) : h('div', { class: 'small muted' }, 'マスター'));
  };
  function debounceSend(ch, isMaster) {
    let t = null;
    return () => {
      clearTimeout(t);
      t = setTimeout(() => remote(() => api.audio(PRODUCTION_ID, isMaster ? 'master' : ch.id, { gain_db: ch.gain_db, mute: ch.mute, delay_ms: ch.delay_ms ?? 0 }), '音声'), 120);
    };
  }
  const renderMixer = () => {
    meterEls.clear();
    replace(mixer, ...st.audio.map((ch) => strip(ch, false)), strip({ id: 'master', name: 'マスター', ...st.master, get gain_db() { return st.master.gain_db; }, set gain_db(v) { st.master.gain_db = v; }, get mute() { return st.master.mute; }, set mute(v) { st.master.mute = v; } }, true));
  };

  let lastMeter = 0;
  const updateMeters = () => {
    const now = performance.now();
    if (now - lastMeter < 50) return;
    lastMeter = now;
    let sum = 0;
    for (const ch of st.audio) {
      // 模擬の信号: ゆっくり揺れる値。音量・ミュートを反映する
      const base = (levels.get(ch.id) ?? -24) + (Math.random() - 0.5) * 6;
      const v = Math.max(-40, Math.min(-6, base));
      levels.set(ch.id, v);
      const out = ch.mute ? -60 : Math.min(0, v + ch.gain_db);
      setMeter(ch.id, out, now);
      if (!ch.mute) sum += 10 ** (out / 10);
    }
    const master = st.master.mute || sum === 0 ? -60 : Math.min(0, 10 * Math.log10(sum) + st.master.gain_db);
    setMeter('master', master, now);
  };
  const setMeter = (id, db, now) => {
    const m = meterEls.get(id);
    if (!m) return;
    const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
    m.lvl.style.height = `${pct}%`;
    if (pct >= m.peakV || now - m.peakT > 1500) { m.peakV = pct; m.peakT = now; }
    m.peak.style.bottom = `${m.peakV}%`;
    m.meter.setAttribute('aria-valuenow', String(Math.round(db)));
  };

  // キーボード
  const onKey = (e) => {
    if (e.target.closest && e.target.closest('input, textarea, select, dialog')) return;
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (/^[1-9]$/.test(e.key)) {
      const sc = st.scenes[Number(e.key) - 1];
      if (sc) { setPreview(sc.id); e.preventDefault(); }
    } else if (e.key === 'c' || e.key === 'C') { take('cut'); e.preventDefault(); } else if (e.key === 'f' || e.key === 'F') { take('fade'); e.preventDefault(); }
  };
  document.addEventListener('keydown', onKey);

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'スイッチャー'), h('span', { class: 'spacer' }),
      h('button', { onClick: () => {
        const id = `s${Date.now().toString(36)}`;
        st.scenes.push({ id, name: `シーン ${st.scenes.length + 1}`, sources: [{ id: `src${id}`, kind: 'color', label: '背景', color: '#333a48', x: 0, y: 0, w: 1, h: 1, visible: true }] });
        save();
        renderScenes();
        remote(() => api.createScene(PRODUCTION_ID, st.scenes[st.scenes.length - 1]), 'シーンの作成');
      } }, '＋ シーン'),
      h('button', { onClick: () => { const p = scene(st.preview); if (p) editScene(p); } }, 'プレビューのシーンを編集'),
      h('button', { onClick: () => { Object.assign(st, defaultState()); save(); renderScenes(); renderMixer(); } }, '初期状態に戻す')),
    banner,
    h('section', { class: 'panel' }, monitors),
    h('section', { class: 'panel', 'aria-labelledby': 'p-tr' }, h('h2', { id: 'p-tr' }, 'トランジション'), trans,
      h('p', { class: 'small muted' }, 'キー: 1〜9 = プレビューへ、C = カット、F = フェード。シーンをダブルクリックで編集。')),
    h('section', { class: 'panel', 'aria-labelledby': 'p-sc' }, h('h2', { id: 'p-sc' }, 'シーン'), scenesBox),
    h('section', { class: 'panel', 'aria-labelledby': 'p-mx' }, h('h2', { id: 'p-mx' }, '音声ミキサー'), mixer,
      h('p', { class: 'small muted' }, 'フェーダーはダブルクリックで 0 dB。遅延は映像との同期用。メーターは API 接続後に実測値になります（今は模擬）。')));

  renderScenes();
  renderMixer();
  raf = requestAnimationFrame(frame);

  try { streamList = await listAll(streams.list); } catch { /* 選べないだけ */ }
  try { available = await probe('productions'); } catch { available = null; }
  renderBanner();

  return () => {
    cancelAnimationFrame(raf);
    document.removeEventListener('keydown', onKey);
    save();
  };
}
