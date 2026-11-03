// 型定義（types/*.d.ts）と API の文書（API.md）を作る。ソースは JSDoc 付きの素の ES modules（ビルド無しでそのまま動く）。
//
//   node scripts/build.mjs [--check] [tsc のパス]
//
// - tsc（TypeScript 5.x。`npm i -D typescript` か TSC=... か引数）で src/ を型検査し、.d.ts を types/ へ出す。
// - 手で書いた・生成した .d.ts（src/rest/openapi.gen.d.ts）を types/ へ写す。
// - TypeScript の API で types/index.d.ts の公開の名前を読み、API.md（クラス・メソッドの署名と説明）を作る。
// - `--check` は作ったものが今の types/・API.md と同じか確かめるだけ（違えば終了コード 1）。
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const check = args.includes('--check');
const tscArg = args.find((a) => !a.startsWith('--'));

function findTsc() {
  const cands = [tscArg, process.env.TSC, path.join(ROOT, 'node_modules/typescript/bin/tsc')];
  try { cands.push(createRequire(import.meta.url).resolve('typescript/bin/tsc')); } catch { /* 無い */ }
  for (const c of cands) if (c && fs.existsSync(c)) return c;
  throw new Error('tsc が見つからない（npm i -D typescript、または TSC=... / 引数で指定）');
}

const tsc = findTsc();
const out = check ? fs.mkdtempSync(path.join(os.tmpdir(), 'senca-sdk-types-')) : path.join(ROOT, 'types');
if (!check) fs.rmSync(out, { recursive: true, force: true });
execFileSync(process.execPath, [tsc, '-p', path.join(ROOT, 'tsconfig.json'), '--outDir', out], { stdio: 'inherit' });
for (const f of ['rest/openapi.gen.d.ts']) fs.copyFileSync(path.join(ROOT, 'src', f), path.join(out, f));

// ------------------------------------------------------------------ API.md
const ts = (await import(pathToFileURL(path.join(path.dirname(tsc), '..', 'lib', 'typescript.js')).href)).default;
const program = ts.createProgram([path.join(out, 'index.d.ts')], { strict: true, skipLibCheck: true, target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.NodeNext, moduleResolution: ts.ModuleResolutionKind.NodeNext, lib: ['lib.es2022.d.ts', 'lib.dom.d.ts'] });
const checker = program.getTypeChecker();
const entry = program.getSourceFile(path.join(out, 'index.d.ts'));
const mod = checker.getSymbolAtLocation(entry);
const exports = checker.getExportsOfModule(mod).sort((a, b) => a.name.localeCompare(b.name));
const docOf = (sym) => ts.displayPartsToString(sym.getDocumentationComment(checker)).trim();
const one = (s) => s.split('\n')[0];
const fmt = (s) => s.replace(/\s+/g, ' ');
const lines = ['# @senca/sdk API', '', '`node scripts/build.mjs` が型定義（types/）から作る。手で直さない。使い方は README.md。', ''];
const sections = { class: [], function: [], const: [], type: [] };
for (const sym0 of exports) {
  const sym = sym0.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(sym0) : sym0;
  if (sym.flags & ts.SymbolFlags.Class) sections.class.push([sym0.name, sym]);
  else if (sym.flags & ts.SymbolFlags.Function) sections.function.push([sym0.name, sym]);
  else if (sym.flags & ts.SymbolFlags.Variable) sections.const.push([sym0.name, sym]);
  else if (sym.flags & ts.SymbolFlags.TypeAlias) sections.type.push([sym0.name, sym]);
}
const isInternal = (d) => ts.getJSDocTags(d).some((t) => t.tagName.text === 'internal');
const decl = (sym) => sym.valueDeclaration ?? sym.declarations?.[0];
lines.push('## 関数・定数', '');
for (const [name, sym] of [...sections.function, ...sections.const]) {
  const type = checker.getTypeOfSymbolAtLocation(sym, decl(sym));
  lines.push(`### \`${name}\``, '', '```ts', fmt(`${name}: ${checker.typeToString(type, undefined, (ts.TypeFormatFlags.NoTruncation | ts.TypeFormatFlags.UseAliasDefinedOutsideCurrentScope))}`), '```', '');
  if (docOf(sym)) lines.push(docOf(sym), '');
  const props = type.getProperties();
  if (name === 'Senca') for (const p of props) lines.push(`- \`Senca.${p.name}\`: ${one(docOf(p))}`);
  lines.push('');
}
lines.push('## クラス', '');
for (const [name, sym] of sections.class) {
  lines.push(`### \`${name}\``, '');
  if (docOf(sym)) lines.push(docOf(sym), '');
  const inst = checker.getDeclaredTypeOfSymbol(sym);
  const statics = checker.getTypeOfSymbolAtLocation(sym, decl(sym));
  const rows = [];
  const member = (p, prefix) => {
    if (p.name.startsWith('_') || p.name === 'prototype' || p.name.startsWith('__@')) return;
    const d = decl(p);
    if (d && isInternal(d)) return;
    if (d && ts.getCombinedModifierFlags(d) & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected)) return;
    // 宣言の文字列（.d.ts のまま。型の別名を展開しない）
    const text = (p.declarations ?? []).map((x) => x.getText().replace(/;\s*$/, '').replace(/^(static|readonly)\s+/g, '').replace(/^get (\w+)\(\): /, '$1: ')).filter((x) => !/^set /.test(x)).join('<br>');
    const t = checker.getTypeOfSymbolAtLocation(p, d ?? decl(sym));
    const sig = text ? `${prefix}${text.replace(/^static\s+/, '')}` : `${prefix}${p.name}: ${checker.typeToString(t)}`;
    rows.push(`| \`${fmt(sig).replace(/\|/g, '\\|')}\` | ${one(docOf(p)).replace(/\|/g, '\\|')} |`);
  };
  for (const s of checker.getSignaturesOfType(statics, ts.SignatureKind.Construct).filter((x) => x.declaration && !isInternal(x.declaration)).slice(0, 1)) rows.push(`| \`new ${name}${fmt(checker.signatureToString(s)).replace(/^\(/, '(').replace(/\|/g, '\\|')}\` | |`);
  for (const p of statics.getProperties()) member(p, 'static ');
  for (const p of inst.getProperties()) member(p, '');
  if (rows.length) lines.push('| メンバー | 説明 |', '|---|---|', ...rows, '');
}
lines.push('## 型', '', '管理 API の資源の型（`Stream` など）は docs/api/openapi.yaml から生成した `types/rest/openapi.gen.d.ts`。', '');
const REST_MODELS = new Set(['Stream', 'StreamCreate', 'StreamPatch', 'Recording', 'Schedule', 'Token', 'Input', 'Output', 'Secret', 'Pipeline', 'Node', 'Grid', 'Problem', 'Room', 'RoomParticipant', 'RoomToken']);
for (const [name, sym] of sections.type) {
  if (REST_MODELS.has(name)) continue;
  const type = checker.getDeclaredTypeOfSymbol(sym);
  lines.push(`### \`${name}\``, '');
  if (docOf(sym)) lines.push(docOf(sym), '');
  const props = type.isUnion() || !(type.flags & ts.TypeFlags.Object) ? [] : type.getProperties();
  if (props.length && !(type.flags & ts.TypeFlags.Union)) {
    lines.push('| 項目 | 型 | 説明 |', '|---|---|---|');
    for (const p of props) {
      const d = decl(p);
      const opt = p.flags & ts.SymbolFlags.Optional ? '?' : '';
      const t = d ? checker.typeToString(checker.getTypeOfSymbolAtLocation(p, d), undefined, (ts.TypeFormatFlags.NoTruncation | ts.TypeFormatFlags.UseAliasDefinedOutsideCurrentScope)) : '';
      lines.push(`| \`${p.name}${opt}\` | \`${fmt(t).replace(/\|/g, '\\|')}\` | ${fmt(docOf(p)).replace(/\|/g, '\\|')} |`);
    }
  } else {
    const flags = (ts.TypeFormatFlags.NoTruncation | ts.TypeFormatFlags.UseAliasDefinedOutsideCurrentScope) | ts.TypeFormatFlags.InTypeAlias;
    const objs = type.isUnion() && type.types.length > 3 && type.types.every((t) => t.flags & ts.TypeFlags.Object);
    if (objs) lines.push('```ts', `type ${name} =`, ...type.types.map((t) => `  | ${fmt(checker.typeToString(t, undefined, flags))}`), '```');
    else lines.push('```ts', fmt(`type ${name} = ${checker.typeToString(type, undefined, flags)}`), '```');
  }
  lines.push('');
}
const api = lines.join('\n').replace(/\n{3,}/g, '\n\n');
const apiPath = path.join(ROOT, 'API.md');

if (check) {
  const diff = [];
  const walk = (dir) => fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(dir, e.name)) : [path.join(dir, e.name)]));
  for (const f of walk(out)) {
    const rel = path.relative(out, f);
    const cur = path.join(ROOT, 'types', rel);
    if (!fs.existsSync(cur) || fs.readFileSync(cur, 'utf8') !== fs.readFileSync(f, 'utf8')) diff.push(`types/${rel}`);
  }
  if (!fs.existsSync(apiPath) || fs.readFileSync(apiPath, 'utf8') !== api) diff.push('API.md');
  fs.rmSync(out, { recursive: true, force: true });
  if (diff.length) { console.error('古い: ' + diff.join(', ')); process.exit(1); }
  console.log('types/ と API.md は最新');
} else {
  fs.writeFileSync(apiPath, api);
  console.log(`types/ と API.md を作った（${exports.length} 個の公開の名前）`);
}
