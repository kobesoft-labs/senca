// Web のスイッチャー（REQ-008 R8-11）: 動いているパイプラインの合成（compose）と音声ミキサー（audio_mix）を操作する。
//
// - シーンはグラフの compose.scenes。プログラム・プレビュー・トランジション中・スレートは GET /pipelines/{id} の
//   status（約 5 Hz で読む）が正。操作は POST /pipelines/{id}/control（set_preview・switch_program・take）。
// - CUT = take（cut）、TAKE = take（選んだトランジション）、FADE = take（fade）。T バーは途中の位置を送れないので、
//   端まで動かしたら、動かすのにかかった時間のトランジションで take する。
// - 音声: audio_mix の入力ごとのフェーダー・ミュート・ソロ（set_audio）とマスター（set_master）。メーターは meters。
// - モニター: 出力の stream（sink）を WHEP（WebRTC）で再生する。プレビューの出力が無ければ、シーンの配置を描く。
//
// キーボード: 1〜9 = そのシーンをプレビューへ、C = CUT、T / Enter = TAKE、F = FADE（入力欄にいないとき）。

import { h, replace, select, badge } from '../lib/dom.js';
import { toast, reportError } from '../lib/ui.js';
import { pipelines as api, ops, runtimeOf, NotAvailableError } from '../api/pipelines.js';
import { streams } from '../api/resources.js';
import { composeNodes, sinkOrigins, TRANSITIONS } from '../pipeline/model.js';
import { playerWidget } from '../player/widget.js';
import { observedBadge } from './pipelines.js';

const LAST = 'senca.console.production.pipeline';

function drawScene(ctx, scene, compose, tally) {
  const W = ctx.canvas.width;
  const H = ctx.canvas.height;
  const cw = (compose.canvas && compose.canvas.width) || 1280;
  const ch = (compose.canvas && compose.canvas.height) || 720;
  const sx = W / cw;
  const sy = H / ch;
  ctx.fillStyle = (compose.canvas && compose.canvas.background) || '#000';
  ctx.fillRect(0, 0, W, H);
  if (!scene) return;
  const layers = [...(scene.layers || [])].map((l, i) => ({ ...l, i })).sort((a, b) => (a.z || 0) - (b.z || 0) || a.i - b.i);
  for (const l of layers) {
    if (l.visible === false) continue;
    const x = l.x * sx;
    const y = l.y * sy;
    const w = l.width * sx;
    const hh = l.height * sy;
    ctx.globalAlpha = l.opacity ?? 1;
    if (l.color) {
      ctx.fillStyle = l.color;
      ctx.fillRect(x, y, w, hh);
    } else {
      const missing = l.source && tally.missing.has(l.source);
      ctx.fillStyle = missing ? '#5a1d1d' : '#2f4f6f';
      ctx.fillRect(x, y, w, hh);
      ctx.strokeStyle = 'rgba(255,255,255,.5)';
      ctx.lineWidth = 2;
      ctx.strokeRect(x + 1, y + 1, w - 2, hh - 2);
      ctx.fillStyle = '#fff';
      ctx.font = `${Math.max(10, Math.round(Math.min(w, hh) * 0.12))}px system-ui, sans-serif`;
      ctx.textBaseline = 'bottom';
      ctx.fillText(`${l.source ? `● ${l.source}` : `▣ ${l.image}`}${missing ? '（信号なし）' : ''}`, x + 6, y + hh - 4, w - 12);
    }
    ctx.globalAlpha = 1;
  }
}

export async function render(root) {
  let list = [];
  let pid = null;
  let data = null; // GET /pipelines/{id}
  let etag = null;
  let composeId = null;
  let status = null; // event = status
  let meters = {}; // node → event = meters
  let pending = null; // {program, preview, until}
  let polling = false;
  let pollTimer = null;
  let dead = false;
  let monitorsFor = null;
  let players = [];
  let tbarStart = 0;

  const head = h('div', { class: 'page-head' });
  const banner = h('div');
  const monitors = h('div', { class: 'sw-monitors' });
  const scenesBox = h('div', { class: 'sw-scenes', role: 'group', 'aria-label': 'シーン' });
  const sourcesBox = h('div', { class: 'sw-sources' });
  const mixer = h('div', { class: 'mixer', role: 'group', 'aria-label': '音声ミキサー' });
  const liveRegion = h('div', { class: 'visually-hidden', 'aria-live': 'polite' });
  const tallyText = h('div', { class: 'row small', 'data-tally': '' });

  // status が新しいか（ノードは status とメーターを同じ欄に入れるので、ミキサーがあると status が見えにくい。
  // 古ければ、操作の返事から覚えた状態 known を使う）
  let statusAt = 0;
  let statusUs = null;
  let known = null; // {program, preview}
  const graph = () => (data && data.graph) || { nodes: [] };
  const compose = () => composeNodes(graph()).find((n) => n.id === composeId) || null;
  const composeStatus = () => (status && (status.compose || []).find((c) => c.node === composeId)) || null;
  const fresh = () => performance.now() - statusAt < 1500;
  const base = () => {
    const cs = composeStatus();
    if (cs && (fresh() || !known)) return { program: cs.program, preview: cs.preview };
    if (known) return known;
    const c = compose() || {};
    return { program: c.program, preview: c.preview };
  };
  const program = () => (pending && performance.now() < pending.until && pending.program ? pending.program : base().program);
  const preview = () => (pending && performance.now() < pending.until && pending.preview !== undefined ? pending.preview : base().preview);

  // ---------------------------------------------------------------- 操作
  const control = async (op, what) => {
    if (!pid) return null;
    try {
      const r = await api.control(pid, op);
      return r.data;
    } catch (e) {
      reportError(e, what);
      return null;
    }
  };

  const setPreview = async (scene) => {
    if (!compose() || !compose().scenes[scene]) return;
    const pg = program();
    pending = { preview: scene, program: pg, until: performance.now() + 1500 };
    renderScenes();
    if (await control(ops.setPreview(composeId, scene), 'プレビュー')) {
      known = { program: pg, preview: scene };
      liveRegion.textContent = `プレビュー: ${scene}`;
    } else pending = null;
    renderScenes();
  };

  const transition = () => ({ kind: kindSel.value, duration_ms: kindSel.value === 'cut' ? 0 : Math.max(0, Number(durInput.value) || 0) });

  const take = async (tr, what = 'TAKE') => {
    const pv = preview();
    const pg = program();
    if (!pv) { toast('プレビューのシーンがありません', 'warn'); return; }
    pending = { program: pv, preview: pg, until: performance.now() + (tr ? tr.duration_ms : 0) + 1500 };
    renderScenes();
    if (await control(ops.take(composeId, tr), what)) {
      known = { program: pv, preview: pg };
      liveRegion.textContent = `${what}: ${pv} → プログラム`;
    } else pending = null;
    renderScenes();
  };

  const cut = () => take({ kind: 'cut', duration_ms: 0 }, 'CUT');
  const fade = () => take({ kind: 'fade', duration_ms: Math.max(50, Number(durInput.value) || 500) }, 'FADE');
  const takeSel = () => take(transition(), 'TAKE');

  const kindSel = select(TRANSITIONS, 'fade', { 'aria-label': 'TAKE のトランジション' });
  const durInput = h('input', { type: 'number', min: 0, max: 10000, step: 50, value: 500, style: { width: '90px' }, 'aria-label': 'トランジションの時間（ミリ秒）' });
  const tbar = h('input', { type: 'range', min: 0, max: 100, value: 0, 'aria-label': 'T バー', style: { width: '220px' } });
  tbar.addEventListener('pointerdown', () => { tbarStart = performance.now(); });
  tbar.addEventListener('input', () => { if (!tbarStart) tbarStart = performance.now(); });
  tbar.addEventListener('change', () => {
    const v = Number(tbar.value);
    if (v >= 95) {
      const dur = Math.max(100, Math.min(5000, Math.round(performance.now() - (tbarStart || performance.now()))));
      take({ kind: kindSel.value === 'cut' ? 'fade' : kindSel.value, duration_ms: dur }, 'T バー');
    }
    tbar.value = '0';
    tbarStart = 0;
  });
  const trans = h('div', { class: 'sw-trans' },
    h('button', { class: 'big danger solid', onClick: cut, title: 'キー C' }, 'CUT'),
    h('button', { class: 'big primary', onClick: takeSel, title: 'キー T / Enter' }, 'TAKE'),
    h('button', { class: 'big', onClick: fade, title: 'キー F' }, 'FADE'),
    h('label', { class: 'row small' }, '種類', kindSel),
    h('label', { class: 'row small' }, '時間', durInput, 'ms'),
    h('label', { class: 'row small' }, 'T バー', tbar),
    liveRegion);

  // ---------------------------------------------------------------- 描画
  const renderScenes = () => {
    const c = compose();
    if (!c) { replace(scenesBox, h('div', { class: 'empty' }, '合成の部品がありません')); return; }
    const pg = program();
    const pv = preview();
    const cs = composeStatus();
    replace(scenesBox, Object.keys(c.scenes || {}).map((name, i) => {
      const b = h('button', {
        class: [name === pv && 'preview', name === pg && 'program'],
        'aria-pressed': String(name === pv),
        'data-scene': name,
        'aria-label': `${name}${name === pg ? '（プログラム）' : ''}${name === pv ? '（プレビュー）' : ''}`,
        title: 'クリックでプレビュー、ダブルクリックでプログラムへカット',
        onClick: () => setPreview(name),
      }, h('strong', null, name), h('span', { class: 'key' }, i < 9 ? `キー ${i + 1}` : ''),
      h('span', { class: 'key' }, name === pg ? '● PGM' : name === pv ? '○ PVW' : ''));
      b.addEventListener('dblclick', async () => {
        const pv2 = preview();
        if (await control(ops.switchProgram(composeId, name), 'プログラムへ')) { known = { program: name, preview: pv2 }; pending = null; renderScenes(); }
      });
      return b;
    }));
    replace(tallyText,
      h('span', null, 'PGM: ', h('strong', { 'data-program': pg || '' }, pg || '—')),
      h('span', null, ' / PVW: ', h('strong', { 'data-preview': pv || '' }, pv || '—')),
      fresh() && cs ? badge('ノードの状態', 'ok') : badge('操作の返事から（状態は未着）', 'warn'),
      cs && cs.in_transition ? badge('トランジション中', 'warn') : null,
      cs && cs.missing && cs.missing.length ? badge(`信号なし: ${cs.missing.join('・')}`, 'danger') : null,
      cs ? h('span', { class: 'muted' }, ` 出力 ${cs.frames} フレーム`) : h('span', { class: 'muted' }, ' 状態はまだ届いていません'));
  };

  const renderSources = () => {
    const srcs = (status && status.sources) || [];
    replace(sourcesBox, srcs.length ? srcs.map((x) => h('div', { class: ['sw-source', x.on_program && 'program', x.on_preview && 'preview'], 'data-source': x.id },
      h('strong', null, x.id), ' ',
      badge(x.state, x.state === 'live' ? 'ok' : x.state === 'stalled' ? 'warn' : ''),
      h('div', { class: 'small muted' }, `${(x.fps || 0).toFixed(1)} fps・復号 ${x.decoded_frames}・捨て ${x.dropped_aus}`),
      x.on_program ? badge('PGM', 'live') : null, x.on_preview ? badge('PVW', 'ok') : null))
      : h('div', { class: 'muted small' }, '入力の状態はまだありません'));
  };

  // 音声
  const strips = new Map(); // `${node}/${input}` → {setMeter, setServer}
  const strip = (mixNode, inp, isMaster) => {
    const key = `${mixNode}/${isMaster ? '@master' : inp.name}`;
    const lvl = h('div', { class: 'lvl' });
    const peak = h('div', { class: 'peak' });
    const title = isMaster ? `${mixNode} マスター` : inp.name;
    const meter = h('div', { class: 'meter', role: 'meter', 'aria-label': `${title} のレベル`, 'aria-valuemin': '-60', 'aria-valuemax': '0', 'aria-valuenow': '-60' }, lvl, peak);
    const gain = isMaster ? (inp.master_gain_db || 0) : (inp.gain_db || 0);
    const db = h('div', { class: 'db' }, `${gain > 0 ? '+' : ''}${gain} dB`);
    const server = h('div', { class: 'small muted', 'data-server-gain': '' }, 'サーバー —');
    const fader = h('input', { type: 'range', class: 'fader', min: -60, max: 10, step: 0.5, value: gain, 'aria-label': `${title} の音量（dB）`, 'aria-valuetext': `${gain} dB` });
    let t = null;
    fader.addEventListener('input', () => {
      const v = Number(fader.value);
      db.textContent = `${v > 0 ? '+' : ''}${v} dB`;
      fader.setAttribute('aria-valuetext', `${v} dB`);
      clearTimeout(t);
      t = setTimeout(() => control(isMaster ? ops.setMaster(mixNode, v) : ops.setAudio(mixNode, inp.name, { gain_db: v }), '音量'), 80);
    });
    fader.addEventListener('dblclick', () => { fader.value = '0'; fader.dispatchEvent(new Event('input')); });
    let muted = !!inp.mute;
    let solo = !!inp.solo;
    const muteBtn = isMaster ? null : h('button', { class: ['small', muted && 'muted-on'], 'aria-pressed': String(muted) }, 'ミュート');
    const soloBtn = isMaster ? null : h('button', { class: ['small', solo && 'muted-on'], 'aria-pressed': String(solo) }, 'ソロ');
    const paint = () => {
      if (muteBtn) { muteBtn.classList.toggle('muted-on', muted); muteBtn.setAttribute('aria-pressed', String(muted)); }
      if (soloBtn) { soloBtn.classList.toggle('muted-on', solo); soloBtn.setAttribute('aria-pressed', String(solo)); }
    };
    if (muteBtn) muteBtn.addEventListener('click', async () => { muted = !muted; paint(); if (!(await control(ops.setAudio(mixNode, inp.name, { mute: muted }), 'ミュート'))) { muted = !muted; paint(); } });
    if (soloBtn) soloBtn.addEventListener('click', async () => { solo = !solo; paint(); if (!(await control(ops.setAudio(mixNode, inp.name, { solo }), 'ソロ'))) { solo = !solo; paint(); } });
    let peakV = 0;
    let peakT = 0;
    strips.set(key, {
      setMeter: (reading) => {
        const pk = Math.max(...(reading.peak_db || [-120]));
        const pct = Math.max(0, Math.min(100, ((pk + 60) / 60) * 100));
        lvl.style.height = `${pct}%`;
        const now = performance.now();
        if (pct >= peakV || now - peakT > 1500) { peakV = pct; peakT = now; }
        peak.style.bottom = `${peakV}%`;
        meter.setAttribute('aria-valuenow', String(Math.round(Math.max(-60, pk))));
        if (typeof reading.gain_db === 'number') {
          server.textContent = `サーバー ${reading.gain_db > 0 ? '+' : ''}${reading.gain_db.toFixed(1)} dB${reading.mute ? '・ミュート' : ''}${reading.solo ? '・ソロ' : ''}`;
          server.dataset.serverGain = String(reading.gain_db);
        }
      },
    });
    return h('div', { class: ['strip', isMaster && 'master'], 'data-strip': key },
      h('div', { class: 'name', title }, title),
      h('div', { class: 'fader-row' }, meter, fader),
      db, server, muteBtn, soloBtn, isMaster ? h('div', { class: 'small muted' }, 'マスター') : null);
  };

  const renderMixer = () => {
    strips.clear();
    const mixes = graph().nodes.filter((n) => n.type === 'audio_mix');
    if (!mixes.length) { replace(mixer, h('div', { class: 'empty' }, '音声ミキサー（audio_mix）がありません')); return; }
    replace(mixer, ...mixes.flatMap((m) => [
      ...Object.entries(m.inputs || {}).map(([name, v]) => strip(m.id, { name, ...v }, false)),
      strip(m.id, m, true)]));
  };

  const updateMeters = () => {
    for (const [node, ev] of Object.entries(meters)) {
      for (const r of ev.inputs || []) { const st = strips.get(`${node}/${r.name}`); if (st) st.setMeter(r); }
      const ms = strips.get(`${node}/@master`);
      if (ms && ev.master) ms.setMeter(ev.master);
    }
  };

  // モニター
  const pvCanvas = h('canvas', { width: 640, height: 360, role: 'img', 'aria-label': 'プレビューのシーンの配置' });
  const stopPlayers = () => { players.forEach((p) => p.stop()); players = []; };
  const renderMonitors = () => {
    const key = `${pid}/${composeId}`;
    if (monitorsFor === key) return;
    monitorsFor = key;
    stopPlayers();
    const origins = sinkOrigins(graph());
    const pgm = origins.find((o) => o.compose === composeId && o.from === 'program') || origins[0];
    const pvw = origins.find((o) => o.compose === composeId && o.from === 'preview');
    const mon = (o, kind) => {
      const w = playerWidget({ getPlayback: async () => (await streams.playbackUrl(o.stream)).data.playback, live: true, label: `${kind === 'program' ? 'プログラム' : 'プレビュー'}のモニター`, mode: 'auto' });
      players.push(w);
      return h('div', { class: ['monitor', kind] }, w.el, h('span', { class: 'mlabel' }, kind === 'program' ? 'PROGRAM' : 'PREVIEW', ` · ${o.sink} → `, h('code', null, o.stream.slice(0, 8))));
    };
    replace(monitors,
      pvw ? mon(pvw, 'preview') : h('div', { class: 'monitor preview' }, pvCanvas, h('span', { class: 'mlabel' }, 'PREVIEW（配置）')),
      pgm ? mon(pgm, 'program') : h('div', { class: 'monitor program' }, h('div', { class: 'empty' }, '出力（sink）がありません')));
    autoStarted = false;
  };
  let autoStarted = false;

  const paintPreview = () => {
    const c = compose();
    if (!c || !pvCanvas.isConnected) return;
    const cs = composeStatus();
    drawScene(pvCanvas.getContext('2d'), (c.scenes || {})[preview()], c, { missing: new Set((cs && cs.missing) || []) });
  };

  const renderHead = () => {
    const pickSel = select(list.length ? list.map((p) => [p.id, `${p.name}（${p.observed_state}）`]) : [['', 'パイプラインがありません']], pid || '', { 'aria-label': 'パイプライン', id: 'sw-pick' });
    pickSel.addEventListener('change', () => choose(pickSel.value));
    const comps = composeNodes(graph());
    const compSel = comps.length > 1 ? select(comps.map((c) => [c.id, c.id]), composeId, { 'aria-label': '合成の部品' }) : null;
    if (compSel) compSel.addEventListener('change', () => { composeId = compSel.value; monitorsFor = null; renderAll(); });
    const running = data && data.desired_state === 'running';
    replace(head, h('h1', null, 'スイッチャー'), h('span', { class: 'spacer' }), pickSel, compSel,
      data ? observedBadge(data.observed_state) : null,
      data && !running ? h('button', { class: 'primary', onClick: async () => {
        try {
          const r = await api.setDesired(pid, 'running', etag);
          data = r.data; etag = r.etag;
          renderAll();
          toast('開始しました', 'ok');
        } catch (e) { reportError(e, '開始'); }
      } }, '▶ パイプラインを開始') : null,
      pid ? h('a', { class: 'btn', href: `#/pipelines?id=${encodeURIComponent(pid)}` }, 'グラフを編集') : null);
  };

  const renderBanner = () => {
    if (!data) { replace(banner, list.length ? null : h('div', { class: 'alert info' }, '合成（compose）を含むパイプラインがありません。', h('a', { href: '#/pipelines' }, 'パイプライン'), ' で作ってください。')); return; }
    replace(banner,
      data.desired_state !== 'running' ? h('div', { class: 'alert warn' }, 'このパイプラインは止まっています。操作するには開始してください。') : null,
      data.last_error ? h('div', { class: 'alert error' }, '最後の誤り: ', data.last_error) : null);
  };

  const renderAll = () => {
    renderHead();
    renderBanner();
    renderMonitors();
    renderScenes();
    renderSources();
    renderMixer();
    paintPreview();
    const c = compose();
    if (c && c.transition) {
      if (c.transition.kind) kindSel.value = c.transition.kind;
      if (c.transition.duration_ms) durInput.value = String(c.transition.duration_ms);
    }
  };

  // ---------------------------------------------------------------- 状態の読み込み
  const poll = async () => {
    if (polling || dead || !pid || document.visibilityState !== 'visible') return;
    polling = true;
    try {
      const r = await api.get(pid);
      if (dead || r.data.id !== pid) return;
      const genChanged = !data || data.generation !== r.data.generation;
      data = r.data;
      etag = r.etag;
      const rt = runtimeOf(data);
      if (rt.status) {
        if (rt.status.session_us !== statusUs) { statusUs = rt.status.session_us; statusAt = performance.now(); }
        status = rt.status;
      }
      Object.assign(meters, rt.meters);
      const cs = composeStatus();
      if (cs && fresh()) known = { program: cs.program, preview: cs.preview };
      if (pending && cs && fresh() && cs.program === pending.program && (pending.preview === undefined || cs.preview === pending.preview)) pending = null;
      if (genChanged) { renderHead(); renderBanner(); renderMixer(); }
      renderScenes();
      renderSources();
      updateMeters();
      paintPreview();
      if (!autoStarted && cs && cs.frames > 0 && players.length) {
        autoStarted = true;
        players.forEach((p) => p.start());
      }
    } catch (e) {
      if (e.status === 404) { toast('このパイプラインは削除されました', 'warn'); pid = null; data = null; renderAll(); }
    } finally {
      polling = false;
    }
  };

  const choose = async (id) => {
    pid = id || null;
    data = null;
    status = null;
    meters = {};
    pending = null;
    known = null;
    statusAt = 0;
    statusUs = null;
    monitorsFor = null;
    if (pid) try { localStorage.setItem(LAST, pid); } catch { /* 何もしない */ }
    if (pid) {
      try {
        const r = await api.get(pid);
        data = r.data;
        etag = r.etag;
        const rt = runtimeOf(data);
        status = rt.status;
        meters = rt.meters;
      } catch (e) { reportError(e, '読み込み'); }
    }
    const comps = composeNodes(graph());
    composeId = comps[0] ? comps[0].id : null;
    renderAll();
  };

  const onKey = (e) => {
    if (e.target.closest && e.target.closest('input, textarea, select, dialog, video')) return;
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    const c = compose();
    if (!c) return;
    if (/^[1-9]$/.test(e.key)) {
      const sc = Object.keys(c.scenes || {})[Number(e.key) - 1];
      if (sc) { setPreview(sc); e.preventDefault(); }
    } else if (e.key === 'c' || e.key === 'C') { cut(); e.preventDefault(); } else if (e.key === 'f' || e.key === 'F') { fade(); e.preventDefault(); } else if (e.key === 't' || e.key === 'T' || (e.key === 'Enter' && !e.target.closest('button, a'))) { takeSel(); e.preventDefault(); }
  };
  document.addEventListener('keydown', onKey);

  root.append(
    head,
    banner,
    h('section', { class: 'panel' }, monitors, tallyText),
    h('section', { class: 'panel', 'aria-labelledby': 'p-tr' }, h('h2', { id: 'p-tr' }, 'トランジション'), trans,
      h('p', { class: 'small muted' }, 'キー: 1〜9 = プレビューへ、C = CUT、T / Enter = TAKE、F = FADE。シーンのダブルクリックでプログラムへカット。T バーは端まで動かすと、その時間のトランジションで TAKE します。')),
    h('section', { class: 'panel', 'aria-labelledby': 'p-sc' }, h('h2', { id: 'p-sc' }, 'シーン'), scenesBox),
    h('section', { class: 'panel', 'aria-labelledby': 'p-so' }, h('h2', { id: 'p-so' }, '入力（タリー）'), sourcesBox),
    h('section', { class: 'panel', 'aria-labelledby': 'p-mx' }, h('h2', { id: 'p-mx' }, '音声ミキサー'), mixer,
      h('p', { class: 'small muted' }, 'フェーダーはダブルクリックで 0 dB。メーターはノードの実測（ピーク、dBFS）。「サーバー」はノードが返した今の値です。')));

  try {
    const { data: d } = await api.list();
    list = d.items.filter((p) => composeNodes(p.graph).length);
  } catch (e) {
    if (e instanceof NotAvailableError) replace(banner, h('div', { class: 'alert warn offline-banner' }, h('strong', null, 'このノードでは使えません'), ' パイプラインの API がありません。'));
    else reportError(e, 'パイプラインの一覧');
  }
  const want = new URLSearchParams(location.hash.split('?')[1] || '').get('pipeline') || localStorage.getItem(LAST);
  const first = list.find((p) => p.id === want) || list.find((p) => p.observed_state === 'running') || list[0];
  await choose(first ? first.id : null);
  pollTimer = setInterval(poll, 200);

  return () => {
    dead = true;
    clearInterval(pollTimer);
    document.removeEventListener('keydown', onKey);
    stopPlayers();
  };
}
