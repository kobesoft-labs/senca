// 合成（compose）のシーンの編集（ダイアログ）。左にシーンの一覧、右に配置の絵（レイヤーをドラッグで動かし、右下の角で大きさを変える）と、
// レイヤーごとの数値の欄。位置と大きさはキャンバスの画素。決定するまで部品の設定は変えない。

import { h, replace, select } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { modal } from '../lib/ui.js';
import { isNodeName } from './model.js';

/** 入力ごとの色（絵でレイヤーを見分ける）。 */
export const INPUT_HUES = [212, 152, 32, 280, 340, 188, 96, 256];
export const hueOf = (names, name) => INPUT_HUES[Math.max(0, names.indexOf(name)) % INPUT_HUES.length];

/**
 * @param {object} n 合成の部品（編集用のモデル）
 * @returns {Promise<{scenes: object, program: string, preview: string}|null>}
 */
export async function editScenes(n) {
  const scenes = JSON.parse(JSON.stringify(n.p.scenes || {}));
  const inputs = n.ins.map((i) => i.name);
  const cw = Number(n.p['canvas.width']) || 1280;
  const ch = Number(n.p['canvas.height']) || 720;
  let cur = Object.keys(scenes)[0] || null;
  let program = n.p.program;
  let preview = n.p.preview;
  let layerSel = 0;

  const list = h('div', { class: 'sc-list', role: 'listbox', 'aria-label': t('pipelines.sc.list') });
  const stage = h('div', { class: 'sc-stage', role: 'img' });
  stage.style.aspectRatio = `${cw} / ${ch}`;
  const head = h('div', { class: 'sc-head' });
  const layersBox = h('div', { class: 'sc-layers' });
  const presets = h('div', { class: 'sc-presets', role: 'group', 'aria-label': t('pipelines.sc.presets') });

  const sorted = (sc) => (sc.layers || []).map((l, i) => ({ l, i })).sort((a, b) => (a.l.z || 0) - (b.l.z || 0) || a.i - b.i);
  const labelOf = (l) => (l.source ? l.source : l.image ? t('pipelines.layerImage', { name: l.image }) : t('pipelines.layerColor'));

  const drawList = () => {
    replace(list, Object.keys(scenes).map((sn) => h('button', {
      type: 'button', role: 'option', class: ['sc-item', sn === cur && 'active'], 'aria-selected': String(sn === cur),
      onClick: () => { cur = sn; layerSel = 0; drawAll(); },
    }, h('span', { class: 'sc-item-name' }, sn),
    sn === program ? h('span', { class: 'tag pgm' }, 'PGM') : null,
    sn === preview ? h('span', { class: 'tag pvw' }, 'PVW') : null)),
    h('button', { type: 'button', class: 'btn sm sc-add', onClick: () => {
      let k = Object.keys(scenes).length + 1;
      while (scenes[`scene${k}`]) k += 1;
      scenes[`scene${k}`] = { layers: inputs[0] ? [{ source: inputs[0], x: 0, y: 0, width: cw, height: ch }] : [] };
      cur = `scene${k}`;
      layerSel = 0;
      drawAll();
    } }, icon('plus'), t('pipelines.addScene')));
  };

  const drawHead = () => {
    if (!cur) { replace(head); return; }
    const nm = h('input', { type: 'text', value: cur, maxlength: 64, 'aria-label': t('pipelines.sceneName'), class: 'sc-name' });
    nm.addEventListener('change', () => {
      const v = nm.value;
      if (!isNodeName(v) || (v !== cur && scenes[v])) { nm.value = cur; return; }
      const ordered = Object.fromEntries(Object.entries(scenes).map(([k, val]) => (k === cur ? [v, val] : [k, val])));
      for (const k of Object.keys(scenes)) delete scenes[k];
      Object.assign(scenes, ordered);
      if (program === cur) program = v;
      if (preview === cur) preview = v;
      cur = v;
      drawAll();
    });
    replace(head, nm, h('button', { type: 'button', class: 'btn danger sm', disabled: Object.keys(scenes).length <= 1, onClick: () => {
      delete scenes[cur];
      cur = Object.keys(scenes)[0] || null;
      drawAll();
    } }, icon('trash'), t('pipelines.deleteScene')));
  };

  const drawStage = () => {
    const sc = scenes[cur];
    if (!sc) { replace(stage); return; }
    stage.setAttribute('aria-label', t('pipelines.sc.stageAria', { scene: cur }));
    replace(stage, sorted(sc).map(({ l, i }) => {
      if (l.visible === false) return null;
      const box = h('div', { class: ['sc-layer', i === layerSel && 'sel', l.color && 'color'], 'data-layer': String(i) },
        h('span', { class: 'sc-layer-label' }, labelOf(l)),
        h('span', { class: 'sc-handle', 'aria-hidden': 'true' }));
      Object.assign(box.style, { left: `${(l.x / cw) * 100}%`, top: `${(l.y / ch) * 100}%`, width: `${(l.width / cw) * 100}%`, height: `${(l.height / ch) * 100}%`, opacity: String(l.opacity ?? 1) });
      if (l.color) box.style.background = l.color;
      else box.style.setProperty('--h', String(hueOf(inputs, l.source)));
      box.addEventListener('pointerdown', (ev) => {
        ev.preventDefault();
        layerSel = i;
        const resize = ev.target.classList.contains('sc-handle');
        const r = stage.getBoundingClientRect();
        const sx = cw / r.width;
        const sy = ch / r.height;
        const o = { x: l.x, y: l.y, w: l.width, h: l.height, cx: ev.clientX, cy: ev.clientY };
        const snap8 = (v) => Math.round(v / 8) * 8;
        const move = (e2) => {
          const dx = (e2.clientX - o.cx) * sx;
          const dy = (e2.clientY - o.cy) * sy;
          if (resize) {
            l.width = Math.max(16, snap8(o.w + dx));
            l.height = e2.shiftKey ? Math.max(16, snap8(o.h + dy)) : Math.max(16, Math.round((l.width * o.h) / o.w));
          } else {
            l.x = snap8(o.x + dx);
            l.y = snap8(o.y + dy);
          }
          drawStage();
        };
        const up = () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); drawLayers(); drawStage(); };
        window.addEventListener('pointermove', move);
        window.addEventListener('pointerup', up);
        drawLayers();
        drawStage();
      });
      return box;
    }));
  };

  const place = (kind) => {
    const l = scenes[cur] && scenes[cur].layers[layerSel];
    if (!l) return;
    const pw = Math.round(cw * 0.275);
    const ph = Math.round((pw * ch) / cw);
    const m = Math.round(cw * 0.025);
    const spots = {
      full: [0, 0, cw, ch],
      corner: [cw - pw - m, ch - ph - m, pw, ph],
      cornerLeft: [m, ch - ph - m, pw, ph],
      left: [0, Math.round(ch / 4), Math.round(cw / 2), Math.round(ch / 2)],
      right: [Math.round(cw / 2), Math.round(ch / 4), Math.round(cw / 2), Math.round(ch / 2)],
    };
    [l.x, l.y, l.width, l.height] = spots[kind];
    if (kind !== 'full') l.z = Math.max(1, l.z || 0);
    drawLayers();
    drawStage();
  };

  const drawLayers = () => {
    const sc = scenes[cur];
    if (!sc) { replace(layersBox, h('p', { class: 'muted' }, t('pipelines.sc.none'))); replace(presets); return; }
    replace(presets, ...[['full', 'fit'], ['corner', 'layout-grid'], ['cornerLeft', 'layout-grid'], ['left', 'layout-grid'], ['right', 'layout-grid']].map(([k, ic]) =>
      h('button', { type: 'button', class: 'btn sm', disabled: !sc.layers[layerSel], onClick: () => place(k) }, icon(ic), t(`pipelines.sc.place.${k}`))));
    replace(layersBox, (sc.layers || []).map((l, li) => {
      const content = select([...inputs.map((x) => [`source:${x}`, t('pipelines.layerInput', { name: x })]), ['color', t('pipelines.layerColor')], ...(l.image ? [[`image:${l.image}`, t('pipelines.layerImage', { name: l.image })]] : [])],
        l.source ? `source:${l.source}` : l.image ? `image:${l.image}` : 'color', { 'aria-label': t('pipelines.layerContent', { scene: cur, n: li + 1 }) });
      const color = h('input', { type: 'color', value: l.color || '#333333', 'aria-label': t('pipelines.layerColor'), hidden: !!(l.source || l.image) });
      content.addEventListener('change', () => {
        delete l.source; delete l.image; delete l.color;
        if (content.value.startsWith('source:')) l.source = content.value.slice(7);
        else if (content.value.startsWith('image:')) l.image = content.value.slice(6);
        else l.color = color.value;
        drawLayers();
        drawStage();
      });
      color.addEventListener('input', () => { if (!l.source && !l.image) { l.color = color.value; drawStage(); } });
      const nm = (key, lbl, def, step = 1) => {
        const x = h('input', { type: 'number', value: l[key] ?? def, step, 'aria-label': t('pipelines.layerField', { scene: cur, n: li + 1, field: lbl }) });
        x.addEventListener('change', () => { l[key] = Number(x.value); drawStage(); });
        return h('label', { class: 'pl-mini-field' }, h('span', null, lbl), x);
      };
      const vis = h('input', { type: 'checkbox', checked: l.visible !== false });
      vis.addEventListener('change', () => { l.visible = vis.checked; drawStage(); });
      const fit = select([['contain', t('pipelines.sc.fit.contain')], ['cover', t('pipelines.sc.fit.cover')], ['stretch', t('pipelines.sc.fit.stretch')]], l.fit || 'contain', { 'aria-label': t('pipelines.fit') });
      fit.addEventListener('change', () => { l.fit = fit.value; });
      const row = h('div', { class: ['sc-row', li === layerSel && 'sel'], onFocusin: () => { if (layerSel !== li) { layerSel = li; drawStage(); for (const r of layersBox.children) r.classList.toggle('sel', r === row); } } },
        h('div', { class: 'sc-row-head' },
          h('span', { class: 'sc-swatch' }),
          h('strong', null, t('pipelines.sc.layerN', { n: li + 1 })), content, color,
          h('label', { class: 'check small' }, vis, t('pipelines.visible')),
          h('button', { type: 'button', class: 'btn ghost icon-only sm', 'aria-label': t('pipelines.removeLayer', { n: li + 1 }), onClick: () => { sc.layers.splice(li, 1); layerSel = 0; drawLayers(); drawStage(); } }, icon('close'))),
        h('div', { class: 'sc-row-fields' },
          nm('x', 'x', 0), nm('y', 'y', 0), nm('width', t('pipelines.width'), cw), nm('height', t('pipelines.height'), ch), nm('z', 'z', 0), nm('opacity', t('pipelines.opacity'), 1, 0.05),
          h('label', { class: 'pl-mini-field' }, h('span', null, t('pipelines.fit')), fit)));
      const sw = row.querySelector('.sc-swatch');
      if (l.color) sw.style.background = l.color; else sw.style.setProperty('--h', String(hueOf(inputs, l.source)));
      return row;
    }), h('button', { type: 'button', class: 'btn sm', onClick: () => {
      sc.layers = sc.layers || [];
      sc.layers.push(inputs[0] ? { source: inputs[0], x: 0, y: 0, width: cw, height: ch, z: sc.layers.length } : { color: '#333333', x: 0, y: 0, width: cw, height: ch });
      layerSel = sc.layers.length - 1;
      drawLayers();
      drawStage();
    } }, icon('plus'), t('pipelines.addLayer')));
  };

  const drawAll = () => { drawList(); drawHead(); drawStage(); drawLayers(); };
  drawAll();

  const ok = await modal(t('pipelines.scenesTitle', { node: n.name }), () => [
    h('p', { class: 'small muted' }, t('pipelines.scenesLead', { w: cw, h: ch })),
    h('div', { class: 'sc-editor' }, list, h('div', { class: 'sc-main' }, head, stage, presets, layersBox)),
  ], {
    wide: true, icon: 'layout-grid', testid: 'scenes-dialog',
    actions: (close) => [h('button', { type: 'button', class: 'btn', onClick: () => close(false) }, t('common.cancel')), h('button', { type: 'button', class: 'btn primary', onClick: () => close(true) }, t('pipelines.apply'))],
  });
  return ok ? { scenes, program, preview } : null;
}

/**
 * シーンの配置の小さな絵（スイッチャーのタイル・ひな形のカード）。canvas に描く。
 * images: 入力の名前 → 画（ImageBitmap など、あれば）
 */
export function drawSceneThumb(ctx2d, scene, { width = 1280, height = 720, background = '#000000', inputs = [], images = {}, missing = new Set() } = {}) {
  const W = ctx2d.canvas.width;
  const H = ctx2d.canvas.height;
  const sx = W / width;
  const sy = H / height;
  ctx2d.globalAlpha = 1;
  ctx2d.fillStyle = background;
  ctx2d.fillRect(0, 0, W, H);
  if (!scene) return;
  const layers = [...(scene.layers || [])].map((l, i) => ({ ...l, i })).sort((a, b) => (a.z || 0) - (b.z || 0) || a.i - b.i);
  for (const l of layers) {
    if (l.visible === false) continue;
    const x = l.x * sx;
    const y = l.y * sy;
    const w = l.width * sx;
    const hh = l.height * sy;
    ctx2d.globalAlpha = l.opacity ?? 1;
    if (l.color) {
      ctx2d.fillStyle = l.color;
      ctx2d.fillRect(x, y, w, hh);
    } else {
      const img = l.source && images[l.source];
      if (img && !missing.has(l.source)) {
        ctx2d.drawImage(img, x, y, w, hh);
      } else {
        const hue = hueOf(inputs, l.source);
        const g = ctx2d.createLinearGradient(x, y, x + w, y + hh);
        g.addColorStop(0, `hsl(${hue} 45% 34%)`);
        g.addColorStop(1, `hsl(${hue} 50% 22%)`);
        ctx2d.fillStyle = missing.has(l.source) ? '#3a1a1c' : g;
        ctx2d.fillRect(x, y, w, hh);
      }
      if (l.border && l.border.width) {
        ctx2d.strokeStyle = l.border.color || '#ffffff';
        ctx2d.lineWidth = Math.max(1, l.border.width * sx);
        ctx2d.strokeRect(x, y, w, hh);
      }
      if (!img || missing.has(l.source)) {
        ctx2d.fillStyle = 'rgba(255,255,255,.88)';
        ctx2d.font = `600 ${Math.max(9, Math.round(Math.min(w, hh) * 0.16))}px system-ui, sans-serif`;
        ctx2d.textBaseline = 'middle';
        ctx2d.textAlign = 'center';
        ctx2d.fillText(l.source || l.image || '', x + w / 2, y + hh / 2, w - 8);
      }
    }
    ctx2d.globalAlpha = 1;
  }
}
