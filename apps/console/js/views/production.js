// Web のスイッチャー（REQ-008 R8-11）: 動いているパイプラインの合成（compose）と音声ミキサー（audio_mix）を操作する。
//
//   ┌ 見出し: パイプラインと合成の部品を選ぶ・グラフを編集・安全装置・キーの説明
//   ├ マルチビュー: プレビュー（緑）｜トランジション（CUT・AUTO・種類・時間・T バー）｜プログラム（赤）
//   ├ シーン: 大きなタイル（配置の絵＋入力の静止画、番号キー、タリー）
//   └ 音声ミキサー（メーター・フェーダー・ミュート・ソロ）｜入力のタリー
//
// - プログラム・プレビュー・トランジション中・信号なしは GET /pipelines/{id} の status（約 5 Hz で読む）が正。
//   操作は POST /pipelines/{id}/control（set_preview・switch_program・take・set_audio・set_master）。
// - CUT = take（cut）、AUTO = take（選んだトランジションと時間）。T バーは途中の位置を送れないので、端から端まで動かしたら、
//   動かすのにかかった時間のトランジションで take する（ハードウェアの T バーと同じく、次は反対の端から戻す）。
// - 安全装置（このブラウザに残す）: CUT・AUTO は 2 回押して確定、シーンのダブルクリックと Shift＋数字の直接カットは効かない。
// - モニター: 出力の stream（sink）を WHEP（WebRTC）で再生する。プレビューの出力が無ければ、シーンの配置を描く。
//
// キーボード: 1〜9 = そのシーンをプレビューへ、Shift＋1〜9 = そのシーンをすぐプログラムへ、C = CUT、A か T = AUTO（入力欄にいないとき）。

import { h, replace, select } from '../lib/dom.js';
import { toast, reportError } from '../lib/ui.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { button, iconButton, pageHeader, alert, emptyState, helpHint, badge, segmented, toggle, attachPopover, skeleton } from '../ui/index.js';
import { isTyping } from '../shortcuts.js';
import { pipelines as api, ops, runtimeOf, NotAvailableError } from '../api/pipelines.js';
import { streams, listAll } from '../api/resources.js';
import { composeNodes, sinkOrigins, composeInputStreams } from '../pipeline/model.js';
import { drawSceneThumb } from '../pipeline/scenes.js';
import { observedBadge } from '../pipeline/status.js';
import { playerWidget } from '../player/widget.js';
import { rememberRecent } from '../lib/recent.js';

const LAST = 'senca.console.production.pipeline';
const SAFETY = 'senca.console.switcher.safety';
const TRANS = 'senca.console.switcher.transition';
const ls = {
  get: (k, d) => { try { return JSON.parse(localStorage.getItem(k) || 'null') ?? d; } catch { return d; } },
  set: (k, v) => { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* 残らないだけ */ } },
};
const dbText = (v) => `${v > 0 ? '+' : ''}${Number(v).toFixed(Number.isInteger(v) ? 0 : 1)} dB`;
const DURATIONS = [250, 500, 1000, 2000];

export async function render(root, params, ctx) {
  root.classList.add('full', 'sw-root');
  let list = [];
  let streamList = [];
  let pid = null;
  let data = null; // GET /pipelines/{id}
  let etag = null;
  let composeId = null;
  let status = null; // event = status
  let meters = {}; // node → event = meters
  let pending = null; // {program, preview, until}
  let polling = false;
  let pollTimer = null;
  let thumbTimer = null;
  let dead = false;
  let monitorsFor = null;
  let players = [];
  let autoStarted = false;
  let safety = !!ls.get(SAFETY, false);
  const trans = { kind: 'fade', duration: 500, ...ls.get(TRANS, {}) };
  let tbarAt = 0; // T バーのいる端（0 か 100）
  let tbarStart = 0;
  let armed = null; // {what, until}
  let thumbs = {}; // 入力の名前 → ImageBitmap
  let statusAt = 0;
  let statusUs = null;
  let known = null; // {program, preview}

  const graph = () => (data && data.graph) || { nodes: [] };
  const compose = () => composeNodes(graph()).find((n) => n.id === composeId) || null;
  const composeStatus = () => (status && (status.compose || []).find((c) => c.node === composeId)) || null;
  const fresh = () => performance.now() - statusAt < 1500;
  const running = () => !!data && data.desired_state === 'running' && data.observed_state === 'running';
  const base = () => {
    const cs = composeStatus();
    if (cs && (fresh() || !known)) return { program: cs.program, preview: cs.preview };
    if (known) return known;
    const c = compose() || {};
    return { program: c.program, preview: c.preview };
  };
  const program = () => (pending && performance.now() < pending.until && pending.program ? pending.program : base().program);
  const preview = () => (pending && performance.now() < pending.until && pending.preview !== undefined ? pending.preview : base().preview);
  const sceneNames = () => Object.keys((compose() || {}).scenes || {});
  const streamName = (id) => (streamList.find((x) => x.id === id) || {}).name || (id ? id.slice(0, 8) : '—');

  // ---------------------------------------------------------------- 要素
  const head = h('div');
  const banner = h('div', { class: 'sw-banners' });
  const live = h('div', { class: 'visually-hidden', role: 'status', 'aria-live': 'assertive' });
  const pvwMon = h('div', { class: 'monitor preview', 'data-testid': 'monitor-preview' });
  const pgmMon = h('div', { class: 'monitor program', 'data-testid': 'monitor-program' });
  const pvwLabel = h('div', { class: 'mon-label pvw' });
  const pgmLabel = h('div', { class: 'mon-label pgm' });
  const scenesBox = h('div', { class: 'sw-scenes', role: 'group', 'aria-label': t('production.scenes'), 'data-testid': 'scenes' });
  const sourcesBox = h('ul', { class: 'sw-sources', 'data-testid': 'tally' });
  const mixerBox = h('div', { class: 'sw-mixer', role: 'group', 'aria-label': t('production.mixer') });
  const stateLine = h('div', { class: 'sw-state', 'data-tally': '' });
  const progress = h('div', { class: 'sw-progress', 'aria-hidden': 'true' }, h('div', { class: 'bar' }));

  // ---------------------------------------------------------------- 操作
  const control = async (op, what) => {
    if (!pid) return null;
    try {
      const r = await api.control(pid, op);
      return r.data;
    } catch (e) {
      if (e.status === 409) toast(t('production.notRunning'), 'warn');
      else reportError(e, what);
      return null;
    }
  };

  const say = (msg) => { live.textContent = ''; setTimeout(() => { live.textContent = msg; }, 20); };

  const setPreview = async (scene) => {
    if (!compose() || !compose().scenes[scene] || !running()) return;
    const pg = program();
    pending = { preview: scene, program: pg, until: performance.now() + 1500 };
    paintTally();
    if (await control(ops.setPreview(composeId, scene), t('production.preview'))) {
      known = { program: pg, preview: scene };
      say(t('production.previewIs', { scene }));
    } else pending = null;
    paintTally();
  };

  const animate = (ms) => {
    const bar = progress.firstChild;
    progress.classList.remove('run');
    bar.style.transitionDuration = '0ms';
    bar.style.width = '0%';
    if (!ms) return;
    void bar.offsetWidth;
    progress.classList.add('run');
    bar.style.transitionDuration = `${ms}ms`;
    bar.style.width = '100%';
    setTimeout(() => { progress.classList.remove('run'); bar.style.transitionDuration = '0ms'; bar.style.width = '0%'; }, ms + 300);
  };

  const take = async (tr, label) => {
    if (!running()) { toast(t('production.notRunning'), 'warn'); return; }
    const pv = preview();
    const pg = program();
    if (!pv) { toast(t('production.noPreview'), 'warn'); return; }
    pending = { program: pv, preview: pg, until: performance.now() + (tr ? tr.duration_ms : 0) + 1500 };
    paintTally();
    animate(tr ? tr.duration_ms : 0);
    if (await control(ops.take(composeId, tr), label)) {
      known = { program: pv, preview: pg };
      say(t('production.tookAnnounce', { what: label, scene: pv }));
    } else { pending = null; animate(0); }
    paintTally();
  };

  const cutTo = async (scene) => {
    if (!running()) return;
    const pv = preview();
    pending = { program: scene, preview: pv, until: performance.now() + 1500 };
    paintTally();
    if (await control(ops.switchProgram(composeId, scene), t('production.toProgram'))) {
      known = { program: scene, preview: pv };
      say(t('production.tookAnnounce', { what: 'CUT', scene }));
    } else pending = null;
    paintTally();
  };

  const autoTr = () => (trans.kind === 'cut' ? { kind: 'cut', duration_ms: 0 } : { kind: trans.kind, duration_ms: Math.max(0, Math.min(60000, Number(trans.duration) || 0)) });

  /** 安全装置: 1 回目で構え、2.5 秒以内の 2 回目で実行する。 */
  const guarded = (what, fn) => () => {
    if (!running()) { toast(t('production.notRunning'), 'warn'); return; }
    if (!safety) { fn(); return; }
    if (armed && armed.what === what && performance.now() < armed.until) {
      armed = null;
      paintTransition();
      fn();
      return;
    }
    armed = { what, until: performance.now() + 2500 };
    say(t('production.armed', { what: what.toUpperCase() }));
    paintTransition();
    setTimeout(() => { if (armed && armed.what === what && performance.now() >= armed.until) { armed = null; paintTransition(); } }, 2600);
  };
  const doCut = guarded('cut', () => take({ kind: 'cut', duration_ms: 0 }, 'CUT'));
  const doAuto = guarded('auto', () => take(autoTr(), 'AUTO'));

  // ---------------------------------------------------------------- トランジション
  const cutBtn = h('button', { type: 'button', class: 'sw-big sw-cut', 'data-testid': 'sw-cut', onClick: () => doCut() });
  const autoBtn = h('button', { type: 'button', class: 'sw-big sw-auto', 'data-testid': 'sw-auto', onClick: () => doAuto() });
  const kindSeg = segmented({
    options: [['cut', t('pipelines.transition.cut')], ['fade', t('pipelines.transition.fade')], ['wipe_left', t('production.wipeL')], ['wipe_right', t('production.wipeR')]],
    value: trans.kind, label: t('production.kind'), size: 'sm', testid: 'sw-kind',
    onChange: (v) => { trans.kind = v; ls.set(TRANS, trans); paintTransition(); },
  });
  const durInput = h('input', { type: 'number', min: 0, max: 60000, step: 50, value: trans.duration, class: 'sw-dur-input', 'aria-label': t('production.durationAria'), 'data-testid': 'sw-duration' });
  durInput.addEventListener('change', () => { trans.duration = Math.max(0, Math.min(60000, Number(durInput.value) || 0)); durInput.value = String(trans.duration); ls.set(TRANS, trans); paintTransition(); });
  const durChips = h('div', { class: 'sw-dur-chips', role: 'group', 'aria-label': t('production.duration') });
  const tbar = h('input', { type: 'range', min: 0, max: 100, value: 0, class: 'sw-tbar', 'aria-label': t('production.tbar'), 'aria-valuetext': t('production.tbarStart'), 'data-testid': 'sw-tbar' });
  tbar.addEventListener('pointerdown', () => { tbarStart = performance.now(); });
  tbar.addEventListener('input', () => {
    if (!tbarStart) tbarStart = performance.now();
    const v = Number(tbar.value);
    const pct = tbarAt === 0 ? v : 100 - v;
    tbar.setAttribute('aria-valuetext', t('production.tbarPct', { n: pct }));
    progress.firstChild.style.transitionDuration = '0ms';
    progress.firstChild.style.width = `${pct}%`;
  });
  tbar.addEventListener('change', () => {
    const v = Number(tbar.value);
    const target = tbarAt === 0 ? 100 : 0;
    if (Math.abs(v - target) <= 5) {
      const dur = Math.max(100, Math.min(5000, Math.round(performance.now() - (tbarStart || performance.now()))));
      tbarAt = target;
      tbar.value = String(target);
      take({ kind: trans.kind === 'cut' ? 'fade' : trans.kind, duration_ms: dur }, t('production.tbar'));
    } else {
      tbar.value = String(tbarAt);
      progress.firstChild.style.width = '0%';
    }
    tbar.setAttribute('aria-valuetext', t('production.tbarStart'));
    tbarStart = 0;
  });
  const safetyToggle = toggle({ label: t('production.safety'), description: t('production.safetyHint'), checked: safety, testid: 'sw-safety', onChange: (v) => { safety = v; ls.set(SAFETY, v); armed = null; paintTransition(); renderScenes(); } });

  const transCol = h('div', { class: 'sw-trans', role: 'group', 'aria-label': t('production.transition') },
    h('div', { class: 'sw-big-row' }, cutBtn, autoBtn),
    progress,
    h('div', { class: 'sw-trans-opts' },
      h('div', { class: 'sw-opt' }, h('span', { class: 'sw-opt-label' }, t('production.kind')), kindSeg),
      h('div', { class: 'sw-opt' }, h('span', { class: 'sw-opt-label' }, t('production.duration')), h('div', { class: 'sw-dur' }, durChips, h('label', { class: 'sw-dur-field' }, durInput, h('span', { class: 'muted small' }, 'ms'))))),
    h('div', { class: 'sw-tbar-wrap' }, h('span', { class: 'sw-opt-label' }, t('production.tbar')), tbar));

  const paintTransition = () => {
    const off = !running();
    const armCut = armed && armed.what === 'cut';
    const armAuto = armed && armed.what === 'auto';
    replace(cutBtn, h('span', { class: 'sw-big-main' }, armCut ? t('production.confirmCut') : 'CUT'), h('kbd', null, 'C'));
    replace(autoBtn, h('span', { class: 'sw-big-main' }, armAuto ? t('production.confirmAuto') : 'AUTO'),
      h('span', { class: 'sw-big-sub' }, trans.kind === 'cut' ? t('pipelines.transition.cut') : `${t(`production.short.${trans.kind}`)} · ${trans.duration} ms`), h('kbd', null, 'A'));
    cutBtn.classList.toggle('armed', !!armCut);
    autoBtn.classList.toggle('armed', !!armAuto);
    cutBtn.disabled = off;
    autoBtn.disabled = off;
    tbar.disabled = off;
    cutBtn.setAttribute('aria-label', armCut ? t('production.confirmCutAria') : t('production.cutAria'));
    autoBtn.setAttribute('aria-label', armAuto ? t('production.confirmAutoAria') : t('production.autoAria', { kind: t(`pipelines.transition.${trans.kind}`), ms: trans.duration }));
    durInput.disabled = trans.kind === 'cut';
    replace(durChips, DURATIONS.map((ms) => h('button', {
      type: 'button', class: 'sw-chip', 'aria-pressed': String(Number(trans.duration) === ms), disabled: trans.kind === 'cut',
      onClick: () => { trans.duration = ms; durInput.value = String(ms); ls.set(TRANS, trans); paintTransition(); },
    }, ms >= 1000 ? `${ms / 1000} s` : `${ms} ms`)));
  };

  // ---------------------------------------------------------------- シーン
  const thumbCanvases = new Map();
  const renderScenes = () => {
    const c = compose();
    thumbCanvases.clear();
    if (!c) { replace(scenesBox); return; }
    replace(scenesBox, Object.keys(c.scenes || {}).map((name, i) => {
      const cv = h('canvas', { width: 320, height: 180, class: 'sw-thumb', 'aria-hidden': 'true' });
      thumbCanvases.set(name, cv);
      const b = h('button', { type: 'button', class: 'sw-scene', 'data-scene': name, onClick: () => setPreview(name) },
        h('span', { class: 'sw-thumb-wrap' }, cv, i < 9 ? h('kbd', { class: 'sw-key' }, String(i + 1)) : null, h('span', { class: 'sw-tally-tag' })),
        h('span', { class: 'sw-scene-name' }, name));
      b.addEventListener('dblclick', () => { if (!safety) cutTo(name); });
      return b;
    }));
    paintThumbs();
    paintTally();
  };

  const paintThumbs = () => {
    const c = compose();
    if (!c) return;
    const cs = composeStatus();
    const opt = { width: (c.canvas && c.canvas.width) || 1280, height: (c.canvas && c.canvas.height) || 720, background: (c.canvas && c.canvas.background) || '#000000', inputs: Object.keys(c.inputs || {}), images: thumbs, missing: new Set((cs && cs.missing) || []) };
    for (const [name, cv] of thumbCanvases) drawSceneThumb(cv.getContext('2d'), c.scenes[name], opt);
    if (pvwCanvas.isConnected) drawSceneThumb(pvwCanvas.getContext('2d'), c.scenes[preview()], opt);
  };

  const paintTally = () => {
    const pg = program();
    const pv = preview();
    const cs = composeStatus();
    for (const b of scenesBox.querySelectorAll('[data-scene]')) {
      const name = b.dataset.scene;
      const onPg = name === pg;
      const onPv = name === pv;
      b.classList.toggle('program', onPg);
      b.classList.toggle('preview', onPv && !onPg);
      b.setAttribute('aria-pressed', String(onPv));
      b.setAttribute('aria-label', `${name}${onPg ? t('production.onProgram') : ''}${onPv ? t('production.onPreview') : ''}`);
      const tag = b.querySelector('.sw-tally-tag');
      tag.textContent = onPg ? 'PGM' : onPv ? 'PVW' : '';
      b.disabled = !running();
    }
    replace(pvwLabel, h('span', { class: 'mon-tag' }, 'PVW'), h('span', { class: 'mon-scene', 'data-preview': pv || '' }, pv || '—'));
    replace(pgmLabel, h('span', { class: 'mon-tag' }, 'PGM'), h('span', { class: 'mon-scene', 'data-program': pg || '' }, pg || '—'),
      running() ? h('span', { class: 'badge live sm' }, t('production.onAir')) : null);
    replace(stateLine,
      running() && fresh() && cs ? h('span', { class: 'sw-fresh ok' }, h('span', { class: 'dot ok' }), t('production.nodeState')) : running() ? h('span', { class: 'sw-fresh' }, h('span', { class: 'dot warn' }), t('production.fromReply')) : null,
      cs && cs.in_transition ? badge(t('production.inTransition'), 'info', { dot: true }) : null,
      cs && cs.missing && cs.missing.length ? badge(t('production.noSignal', { list: cs.missing.join(', ') }), 'danger', { icon: 'wifi-off' }) : null,
      cs ? h('span', { class: 'muted small num' }, t('production.frames', { n: cs.frames })) : null);
    paintThumbs();
  };

  // ---------------------------------------------------------------- 入力のタリー
  const renderSources = () => {
    const srcs = (status && status.sources) || [];
    const gsrc = graph().nodes.filter((n) => n.type === 'source');
    if (!gsrc.length) { replace(sourcesBox, h('li', { class: 'muted small' }, t('production.noSources'))); return; }
    replace(sourcesBox, gsrc.map((n) => {
      const x = srcs.find((s) => s.id === n.id) || {};
      const state = x.state || (running() ? 'missing' : 'stopped');
      return h('li', { class: ['sw-source', x.on_program && 'program', x.on_preview && !x.on_program && 'preview'], 'data-source': n.id },
        h('span', { class: 'sw-src-tally', 'aria-hidden': 'true' }),
        h('div', { class: 'sw-src-main' },
          h('strong', null, n.id),
          h('span', { class: 'muted small' }, streamName(n.stream))),
        h('div', { class: 'sw-src-side' },
          x.on_program ? badge('PGM', 'live') : x.on_preview ? badge('PVW', 'ok') : null,
          h('span', { class: ['sw-src-state', state] }, h('span', { class: ['dot', state === 'live' ? 'ok' : state === 'stalled' ? 'warn' : state === 'stopped' ? '' : 'danger'] }),
            state === 'stopped' ? t('status.stopped') : t(`pipelines.live.${['live', 'stalled'].includes(state) ? state : 'missing'}`)),
          x.fps ? h('span', { class: 'muted small num' }, `${x.fps.toFixed(1)} fps`) : null));
    }));
  };

  // ---------------------------------------------------------------- 音声
  const strips = new Map(); // `${node}/${input}` → {setMeter}
  const strip = (mixNode, inp, isMaster) => {
    const key = `${mixNode}/${isMaster ? '@master' : inp.name}`;
    const title = isMaster ? t('production.master') : inp.name;
    const bars = [h('div', { class: 'lvl' }), h('div', { class: 'lvl' })];
    const peaks = [h('div', { class: 'peak' }), h('div', { class: 'peak' })];
    const meter = h('div', { class: 'sw-meter', role: 'meter', 'aria-label': t('production.levelOf', { name: isMaster ? t('production.masterOf', { node: mixNode }) : title }), 'aria-valuemin': '-60', 'aria-valuemax': '0', 'aria-valuenow': '-60' },
      h('div', { class: 'ch' }, bars[0], peaks[0]), h('div', { class: 'ch' }, bars[1], peaks[1]));
    const scale = h('div', { class: 'sw-scale', 'aria-hidden': 'true' }, [0, -6, -12, -24, -48].map((db) => { const s = h('span', null, String(db)); s.style.bottom = `${((db + 60) / 60) * 100}%`; return s; }));
    let gain = isMaster ? (inp.master_gain_db || 0) : (inp.gain_db || 0);
    const db = h('output', { class: 'sw-db num' }, dbText(gain));
    const server = h('span', { class: 'sw-server', 'data-server-gain': '' });
    const fader = h('input', { type: 'range', class: 'sw-fader', min: -60, max: 12, step: 0.5, value: gain, 'aria-label': t('production.volumeOf', { name: isMaster ? t('production.masterOf', { node: mixNode }) : title }), 'aria-valuetext': dbText(gain), 'data-testid': `fader-${mixNode}-${isMaster ? 'master' : inp.name}` });
    let lastSent = 0;
    let sendTimer = null;
    const send = (v) => control(isMaster ? ops.setMaster(mixNode, v) : ops.setAudio(mixNode, inp.name, { gain_db: v }), t('production.volume'));
    const onFader = () => {
      gain = Number(fader.value);
      db.textContent = dbText(gain);
      fader.setAttribute('aria-valuetext', dbText(gain));
      // 動かしている間は 60 ms ごと、止めたら最後の値を送る（反応よく、送りすぎない）
      clearTimeout(sendTimer);
      const now = performance.now();
      if (now - lastSent > 60) { lastSent = now; send(gain); } else sendTimer = setTimeout(() => { lastSent = performance.now(); send(gain); }, 70);
    };
    fader.addEventListener('input', onFader);
    fader.addEventListener('dblclick', () => { fader.value = '0'; onFader(); });
    fader.addEventListener('keydown', (e) => {
      if (e.key === 'PageUp' || e.key === 'PageDown') { e.preventDefault(); fader.value = String(Math.max(-60, Math.min(12, Number(fader.value) + (e.key === 'PageUp' ? 6 : -6)))); onFader(); }
      if (e.key === '0' || e.key === 'Home') { e.preventDefault(); fader.value = '0'; onFader(); }
    });
    let muted = !!inp.mute;
    let solo = !!inp.solo;
    const muteBtn = isMaster ? null : h('button', { type: 'button', class: 'sw-ms mute', 'aria-pressed': String(muted), 'aria-label': t('production.muteOf', { name: title }) }, icon('volume-x'), h('span', null, 'M'));
    const soloBtn = isMaster ? null : h('button', { type: 'button', class: 'sw-ms solo', 'aria-pressed': String(solo), 'aria-label': t('production.soloOf', { name: title }) }, icon('headphones'), h('span', null, 'S'));
    const paint = () => {
      if (muteBtn) muteBtn.setAttribute('aria-pressed', String(muted));
      if (soloBtn) soloBtn.setAttribute('aria-pressed', String(solo));
      el.classList.toggle('is-muted', muted);
    };
    if (muteBtn) muteBtn.addEventListener('click', async () => { muted = !muted; paint(); if (!(await control(ops.setAudio(mixNode, inp.name, { mute: muted }), t('production.mute')))) { muted = !muted; paint(); } });
    if (soloBtn) soloBtn.addEventListener('click', async () => { solo = !solo; paint(); if (!(await control(ops.setAudio(mixNode, inp.name, { solo }), t('production.solo')))) { solo = !solo; paint(); } });
    const peakHold = [{ v: 0, t: 0 }, { v: 0, t: 0 }];
    const el = h('div', { class: ['sw-strip', isMaster && 'master'], 'data-strip': key },
      h('div', { class: 'sw-strip-name', title }, isMaster ? icon('volume', { size: 'sm' }) : null, title),
      h('div', { class: 'sw-strip-body' }, scale, meter, h('div', { class: 'sw-fader-wrap' }, fader)),
      db, server,
      isMaster ? h('div', { class: 'sw-ms-row muted small' }, mixNode) : h('div', { class: 'sw-ms-row' }, muteBtn, soloBtn));
    paint();
    strips.set(key, {
      setMeter: (reading) => {
        const pk = reading.peak_db && reading.peak_db.length ? reading.peak_db : [-120];
        const chs = [pk[0], pk.length > 1 ? pk[1] : pk[0]];
        const now = performance.now();
        chs.forEach((v, i) => {
          const pct = Math.max(0, Math.min(100, ((v + 60) / 60) * 100));
          bars[i].style.transform = `scaleY(${pct / 100})`;
          bars[i].classList.toggle('hot', v > -6);
          bars[i].classList.toggle('clip', v > -1);
          if (pct >= peakHold[i].v || now - peakHold[i].t > 1500) peakHold[i] = { v: pct, t: now };
          peaks[i].style.bottom = `${peakHold[i].v}%`;
        });
        meter.setAttribute('aria-valuenow', String(Math.round(Math.max(-60, Math.max(...chs)))));
        if (typeof reading.gain_db === 'number') {
          server.textContent = t('production.server', { db: dbText(reading.gain_db) });
          server.dataset.serverGain = String(reading.gain_db);
          if (!isMaster && typeof reading.mute === 'boolean' && reading.mute !== muted && !document.activeElement.closest('[data-strip]')) { muted = reading.mute; paint(); }
        }
      },
    });
    return el;
  };

  const renderMixer = () => {
    strips.clear();
    const mixes = graph().nodes.filter((n) => n.type === 'audio_mix');
    if (!mixes.length) { replace(mixerBox, h('p', { class: 'muted small sw-none' }, icon('info', { size: 'sm' }), t('production.noMixer'))); return; }
    replace(mixerBox, ...mixes.map((m) => h('div', { class: 'sw-mix-group' },
      ...Object.entries(m.inputs || {}).map(([name, v]) => strip(m.id, { name, ...v }, false)),
      strip(m.id, m, true))));
  };

  const updateMeters = () => {
    for (const [node, ev] of Object.entries(meters)) {
      for (const r of ev.inputs || []) { const st = strips.get(`${node}/${r.name}`); if (st) st.setMeter(r); }
      const ms = strips.get(`${node}/@master`);
      if (ms && ev.master) ms.setMeter(ev.master);
    }
  };

  // ---------------------------------------------------------------- モニター
  const pvwCanvas = h('canvas', { width: 640, height: 360, class: 'mon-canvas', role: 'img', 'aria-label': t('production.previewLayout') });
  const stopPlayers = () => { players.forEach((p) => p.stop()); players = []; };
  const renderMonitors = () => {
    const key = `${pid}/${composeId}`;
    if (monitorsFor === key) return;
    monitorsFor = key;
    stopPlayers();
    autoStarted = false;
    const origins = sinkOrigins(graph());
    const pgm = origins.find((o) => o.compose === composeId && o.from === 'program');
    const pvw = origins.find((o) => o.compose === composeId && o.from === 'preview');
    const mon = (o, kind) => {
      const w = playerWidget({ getPlayback: async () => (await streams.playbackUrl(o.stream)).data.playback, live: true, label: kind === 'program' ? t('production.programMonitor') : t('production.previewMonitor'), mode: 'auto' });
      players.push(w);
      return [w.el, h('span', { class: 'mon-stream' }, icon('live', { size: 'sm' }), streamName(o.stream))];
    };
    replace(pvwMon, pvwLabel, ...(pvw ? mon(pvw, 'preview') : [pvwCanvas, h('span', { class: 'mon-stream' }, t('production.previewLayoutLabel'))]));
    replace(pgmMon, pgmLabel, ...(pgm ? mon(pgm, 'program') : [h('div', { class: 'mon-empty' }, icon('live'), h('p', null, t('production.noSink')))]));
    pvwMon.classList.toggle('layout', !pvw);
  };

  // ---------------------------------------------------------------- 入力の静止画（シーンの絵に使う）
  const loadThumbs = async () => {
    const c = compose();
    if (!c || dead || document.visibilityState !== 'visible') return;
    const map = composeInputStreams(graph(), composeId);
    const next = { ...thumbs };
    await Promise.all(Object.entries(map).map(async ([name, sid]) => {
      const st = streamList.find((x) => x.id === sid);
      if (st && st.observed_state !== 'live') { delete next[name]; return; }
      try {
        const { data: blob } = await streams.snapshot(sid);
        if (typeof createImageBitmap === 'function') next[name] = await createImageBitmap(blob);
      } catch { /* 絵が無ければ配置だけ */ }
    }));
    if (dead) return;
    thumbs = next;
    paintThumbs();
  };

  // ---------------------------------------------------------------- 見出しとお知らせ
  const renderHead = () => {
    const pickSel = select(list.map((p) => [p.id, p.name]), pid || '', { 'aria-label': t('production.pickPipeline'), id: 'sw-pick', 'data-testid': 'sw-pick', class: 'sw-pick' });
    pickSel.addEventListener('change', () => choose(pickSel.value));
    const comps = composeNodes(graph());
    const compSel = comps.length > 1 ? select(comps.map((c) => [c.id, c.id]), composeId, { 'aria-label': t('production.composeNode') }) : null;
    if (compSel) compSel.addEventListener('change', () => { composeId = compSel.value; monitorsFor = null; renderAll(); loadThumbs(); });
    const keys = iconButton('keyboard', t('production.keysTitle'), {});
    attachPopover(keys, () => h('div', { class: 'pl-keys' }, h('h3', null, t('production.keysTitle')), h('dl', null,
      [['1–9', t('production.k.preview')], ['Shift + 1–9', t('production.k.direct')], ['C', t('production.k.cut')], ['A / T', t('production.k.auto')], [t('production.k.dblclick'), t('production.k.direct')], ['PgUp / PgDn', t('production.k.fader')]]
        .flatMap(([k, v]) => [h('dt', null, h('kbd', null, k)), h('dd', null, v)])), h('p', { class: 'muted small' }, t('production.k.safetyNote'))), { label: t('production.keysTitle') });
    replace(head, pageHeader({
      title: t('nav.production'),
      help: helpHint({ title: t('nav.production'), body: t('production.help'), guide: '07-pipelines.md' }),
      badges: data ? [observedBadge(data.observed_state)] : null,
      actions: [
        list.length ? h('label', { class: 'visually-hidden', for: 'sw-pick' }, t('production.pickPipeline')) : null,
        list.length ? pickSel : null, compSel,
        pid ? h('a', { class: 'btn', href: `#/pipelines/${encodeURIComponent(pid)}`, 'data-testid': 'sw-edit' }, icon('pipelines'), h('span', { class: 'btn-label' }, t('production.editGraph'))) : null,
        list.length ? keys : null,
      ].filter(Boolean),
    }));
  };

  const startPipeline = async () => {
    try {
      const cur = await api.get(pid);
      const r = await api.setDesired(pid, 'running', cur.etag || `"${cur.data.generation}"`);
      data = r.data;
      etag = r.etag;
      toast(t('pipelines.ed.starting'), 'ok');
      renderAll();
    } catch (e) { reportError(e, t('pipelines.start'), { retry: startPipeline }); }
  };

  const renderBanner = () => {
    if (!data) { replace(banner); return; }
    const parts = [];
    if (data.desired_state !== 'running') {
      parts.push(alert('warn', t('production.stoppedBody'), { title: t('production.stoppedTitle'), actions: [button(t('production.startPipeline'), { variant: 'primary', icon: 'play', onClick: startPipeline, testid: 'sw-start' })] }));
    } else if (data.observed_state !== 'running') {
      parts.push(alert('info', data.observed_state === 'error' ? t('production.errorBody') : t('production.startingBody'), { title: data.observed_state === 'error' ? t('production.errorTitle') : t('production.startingTitle') }));
    }
    if (data.last_error) parts.push(alert('error', data.last_error, { title: t('pipelines.lastError'), small: true, actions: [h('a', { class: 'btn sm', href: `#/pipelines/${encodeURIComponent(pid)}` }, t('production.openEditor'))] }));
    replace(banner, ...parts);
  };

  const renderAll = () => {
    renderHead();
    renderBanner();
    renderMonitors();
    renderScenes();
    renderSources();
    renderMixer();
    paintTransition();
    paintTally();
    workspace.classList.toggle('is-off', !running());
  };

  // ---------------------------------------------------------------- 状態の読み込み
  const poll = async () => {
    if (polling || dead || !pid || document.visibilityState !== 'visible') return;
    polling = true;
    try {
      const r = await api.get(pid);
      if (dead || r.data.id !== pid) return;
      const genChanged = !data || data.generation !== r.data.generation;
      const stateChanged = !data || data.observed_state !== r.data.observed_state || data.desired_state !== r.data.desired_state;
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
      if (genChanged) { monitorsFor = null; renderAll(); } else if (stateChanged) { renderHead(); renderBanner(); paintTransition(); workspace.classList.toggle('is-off', !running()); }
      paintTally();
      renderSources();
      updateMeters();
      if (!autoStarted && cs && cs.frames > 0 && players.length) {
        autoStarted = true;
        players.forEach((p) => p.start());
      }
    } catch (e) {
      if (e.status === 404) { toast(t('pipelines.gone'), 'warn'); pid = null; data = null; await loadList(); renderEmptyOrAll(); }
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
    thumbs = {};
    if (pid) ls.set(LAST, pid);
    if (pid) {
      try {
        const r = await api.get(pid);
        data = r.data;
        etag = r.etag;
        const rt = runtimeOf(data);
        status = rt.status;
        meters = rt.meters;
        rememberRecent({ kind: 'pipeline', id: pid, label: `${t('nav.production')}: ${data.name}`, href: `#/production?pipeline=${encodeURIComponent(pid)}` });
      } catch (e) { reportError(e, t('common.refresh')); }
    }
    const comps = composeNodes(graph());
    composeId = comps[0] ? comps[0].id : null;
    if (history.replaceState && pid) history.replaceState(null, '', `#/production?pipeline=${encodeURIComponent(pid)}`);
    renderAll();
    loadThumbs();
  };

  const onKey = (e) => {
    if (dead || isTyping(e.target) || document.querySelector('dialog[open]') || e.defaultPrevented) return;
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.target.closest && e.target.closest('video, .sw-fader, .sw-tbar')) return;
    const c = compose();
    if (!c) return;
    const digit = /^Digit([1-9])$/.exec(e.code || '');
    if (digit) {
      const sc = sceneNames()[Number(digit[1]) - 1];
      if (!sc) return;
      e.preventDefault();
      if (e.shiftKey) { if (!safety) cutTo(sc); else toast(t('production.safetyBlocked'), 'info'); } else setPreview(sc);
    } else if (e.key === 'c' || e.key === 'C') { e.preventDefault(); doCut(); } else if (['a', 'A', 't', 'T'].includes(e.key)) { e.preventDefault(); doAuto(); }
  };
  document.addEventListener('keydown', onKey);

  // ---------------------------------------------------------------- 組み立て
  const workspace = h('div', { class: 'sw-workspace' },
    h('section', { class: 'sw-mv', 'aria-label': t('production.monitors') }, pvwMon, transCol, pgmMon),
    h('div', { class: 'sw-statusbar' }, stateLine, safetyToggle.el),
    h('section', { class: 'panel sw-panel', 'aria-labelledby': 'sw-scenes-h' },
      h('div', { class: 'sw-panel-head' }, h('h2', { id: 'sw-scenes-h' }, icon('layout-grid'), t('production.scenes')), h('span', { class: 'muted small' }, t('production.scenesHint'))),
      scenesBox),
    h('div', { class: 'sw-bottom' },
      h('section', { class: 'panel sw-panel', 'aria-labelledby': 'sw-mixer-h' },
        h('div', { class: 'sw-panel-head' }, h('h2', { id: 'sw-mixer-h' }, icon('sliders'), t('production.mixer')), h('span', { class: 'muted small' }, t('production.mixerHelp'))),
        mixerBox),
      h('section', { class: 'panel sw-panel', 'aria-labelledby': 'sw-src-h' },
        h('div', { class: 'sw-panel-head' }, h('h2', { id: 'sw-src-h' }, icon('camera'), t('production.sources'))),
        sourcesBox)));

  const body = h('div', { class: 'sw-body' }, skeleton({ kind: 'block' }));
  root.append(head, banner, body, live);

  const loadList = async () => {
    try {
      const { data: d } = await api.list();
      list = d.items.filter((p) => composeNodes(p.graph).length);
      return true;
    } catch (e) {
      if (e instanceof NotAvailableError) replace(banner, alert('warn', t('production.offlineBody'), { title: t('pipelines.offlineTitle') }));
      else reportError(e, t('pipelines.list'), { retry: () => ctx.navigate(location.hash) });
      return false;
    }
  };

  const renderEmptyOrAll = () => {
    if (!list.length) {
      renderHead();
      replace(body, h('section', { class: 'panel sw-first-run', 'data-testid': 'switcher-empty' },
        emptyState({
          icon: 'switcher', title: t('production.emptyTitle'), body: t('production.emptyBody'),
          actions: [
            h('a', { class: 'btn primary', href: '#/pipelines?new=1&template=switcher', 'data-testid': 'switcher-create' }, icon('sparkles'), t('production.createSwitcher')),
            h('a', { class: 'btn', href: '#/pipelines' }, icon('pipelines'), t('production.goPipelines')),
          ],
        }),
        h('div', { class: 'sw-first-explain' },
          h('div', { class: 'sw-first-mock', 'aria-hidden': 'true' }, h('div', { class: 'm pvw' }, 'PVW'), h('div', { class: 'm ctl' }, 'AUTO'), h('div', { class: 'm pgm' }, 'PGM')),
          h('ol', null, h('li', null, t('production.first1')), h('li', null, t('production.first2')), h('li', null, t('production.first3'))))));
      return;
    }
    replace(body, workspace);
  };

  try { streamList = await listAll(streams.list); } catch { /* 名前が出ないだけ */ }
  await loadList();
  const q = new URLSearchParams(location.hash.split('?')[1] || '');
  const want = q.get('pipeline') || ls.get(LAST, null);
  const first = list.find((p) => p.id === want) || list.find((p) => p.observed_state === 'running') || list[0];
  renderEmptyOrAll();
  if (first) await choose(first.id);
  pollTimer = setInterval(poll, 200);
  thumbTimer = setInterval(() => { loadThumbs(); listAll(streams.list).then((x) => { streamList = x; }).catch(() => {}); }, 10000);

  return () => {
    dead = true;
    root.classList.remove('sw-root');
    clearInterval(pollTimer);
    clearInterval(thumbTimer);
    document.removeEventListener('keydown', onKey);
    stopPlayers();
  };
}
