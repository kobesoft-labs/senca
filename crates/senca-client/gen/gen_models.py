#!/usr/bin/env python3
"""docs/api/openapi.yaml の components.schemas から src/models.rs（serde の型）を作る。

使い方: python3 crates/senca-client/gen/gen_models.py [openapi.yaml] [models.rs]
（引数を省くとリポジトリの既定の場所）。仕様を変えたら作り直して差分を確かめる。出力は決定的（同じ入力から同じ出力）。

対応の規則:
- object（properties あり）→ struct。応答の前方互換のため未知のフィールドは拒否しない（deny_unknown_fields を付けない）。
  必須で null にならない → `T`、必須で null になりうる → `Option<T>`（null を送る）、必須でない → `Option<T>`（省けば送らない）。
  名前が `Patch` で終わる型の null になりうるフィールドは `Option<Option<T>>`（省く・null で消す・値で置き換えるの 3 通り）。
- 文字列の enum → enum。知らない値は `Unknown`（`#[serde(other)]`、サーバーが値を足しても読める）。
- 中に書いた object・enum は「親の名前 + フィールドの名前」の型にする（同じ形は 1 つにまとめる）。
- oneOf（null との組を除く）・const・型の無いもの → `serde_json::Value`。
- additionalProperties だけの object → `BTreeMap<String, T>`。
"""
import json
import os
import re
import sys

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, '..', '..', '..'))
SPEC = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, 'docs', 'api', 'openapi.yaml')
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(HERE, '..', 'src', 'models.rs')

RUST_KEYWORDS = {
    'as', 'break', 'const', 'continue', 'crate', 'else', 'enum', 'extern', 'false', 'fn', 'for', 'if', 'impl', 'in',
    'let', 'loop', 'match', 'mod', 'move', 'mut', 'pub', 'ref', 'return', 'static', 'struct', 'trait', 'true',
    'type', 'unsafe', 'use', 'where', 'while', 'async', 'await', 'dyn', 'abstract', 'become', 'box', 'do', 'final',
    'macro', 'override', 'priv', 'typeof', 'unsized', 'virtual', 'yield', 'try', 'gen',
}
# r# にできない識別子
NO_RAW = {'self', 'Self', 'super', 'crate', 'extern'}

spec = yaml.safe_load(open(SPEC, encoding='utf-8'))
schemas = spec['components']['schemas']


def pascal(s):
    parts = re.split(r'[^0-9A-Za-z]+', s)
    out = ''.join(p[:1].upper() + p[1:] for p in parts if p)
    if not out or out[0].isdigit():
        out = 'V' + out
    return out


def snake_field(name):
    s = re.sub(r'[^0-9A-Za-z_]', '_', name)
    s = re.sub(r'([a-z0-9])([A-Z])', r'\1_\2', s).lower()
    if not s or s[0].isdigit():
        s = 'f_' + s
    if s in NO_RAW:
        return 'is_' + s if s == 'self' else s + '_'
    if s in RUST_KEYWORDS:
        return 'r#' + s
    return s


def doc_lines(text, indent):
    if not text:
        return []
    out = []
    for line in str(text).strip().splitlines():
        line = line.rstrip()
        out.append(f'{indent}///{" " + line if line else ""}')
    return out


def ref_name(r):
    return r.split('/')[-1]


def canon(schema):
    """形の比較用（説明は除く）。"""
    def strip(x):
        if isinstance(x, dict):
            return {k: strip(v) for k, v in sorted(x.items()) if k not in ('description', 'example', 'examples', 'default')}
        if isinstance(x, list):
            return [strip(v) for v in x]
        return x
    return json.dumps(strip(schema), sort_keys=True, ensure_ascii=False)


class Gen:
    def __init__(self):
        self.items = []          # (name, code)
        self.names = set(schemas.keys())
        self.by_shape = {}       # canon → 名前（中に書いた型）
        self.default_ok = {}     # 名前 → Default を derive できるか
        self.copy_ok = {}
        self.pending = []

    # ------------------------------------------------------------ 型の解決
    def nullable_split(self, s):
        """(本体, null になりうるか)。"""
        if not isinstance(s, dict):
            return s, False
        if 'oneOf' in s or 'anyOf' in s:
            alts = s.get('oneOf') or s.get('anyOf')
            non_null = [a for a in alts if not (isinstance(a, dict) and a.get('type') == 'null')]
            if len(non_null) == 1 and len(non_null) != len(alts):
                inner = dict(non_null[0])
                if 'description' in s and 'description' not in inner:
                    inner['description'] = s['description']
                return inner, True
        t = s.get('type')
        if isinstance(t, list) and 'null' in t:
            rest = [x for x in t if x != 'null']
            inner = dict(s)
            inner['type'] = rest[0] if len(rest) == 1 else rest
            return inner, True
        if s.get('nullable') is True:
            inner = dict(s)
            del inner['nullable']
            return inner, True
        return s, False

    def rust_type(self, s, ctx):
        """スキーマ → Rust の型（null の扱いは呼ぶ側）。ctx は中に書いた型の名前の元。"""
        if not isinstance(s, dict) or not s:
            return 'Value'
        if '$ref' in s:
            n = ref_name(s['$ref'])
            return pascal(n)
        if 'oneOf' in s or 'anyOf' in s or 'allOf' in s or 'const' in s:
            if 'allOf' in s and len(s['allOf']) == 1:
                return self.rust_type(s['allOf'][0], ctx)
            return 'Value'
        t = s.get('type')
        if isinstance(t, list):
            return 'Value'
        if t == 'string':
            if 'enum' in s:
                return self.inline(s, ctx, kind='enum')
            return 'String'
        if t == 'integer':
            mn = s.get('minimum')
            return 'u64' if mn is not None and mn >= 0 else 'i64'
        if t == 'number':
            return 'f64'
        if t == 'boolean':
            return 'bool'
        if t == 'array':
            return f'Vec<{self.rust_type(s.get("items", {}), ctx + "Item")}>'
        if t == 'object' or 'properties' in s:
            if s.get('properties'):
                return self.inline(s, ctx, kind='struct')
            ap = s.get('additionalProperties')
            if isinstance(ap, dict) and ap:
                inner, nul = self.nullable_split(ap)
                it = self.rust_type(inner, ctx + 'Value')
                return f'BTreeMap<String, {"Option<" + it + ">" if nul else it}>'
            return 'Value'
        return 'Value'

    def inline(self, s, ctx, kind):
        key = canon(s)
        if key in self.by_shape:
            return self.by_shape[key]
        name = ctx
        n = 2
        while name in self.names:
            name = f'{ctx}{n}'
            n += 1
        self.names.add(name)
        self.by_shape[key] = name
        self.pending.append((name, s, kind))
        return name

    # ------------------------------------------------------------ 出力
    def emit_schema(self, name, s):
        rname = pascal(name)
        inner, nullable = self.nullable_split(s)
        if 'properties' in inner and inner.get('properties'):
            self.emit_struct(rname, inner, patch=name.endswith('Patch'))
        elif inner.get('type') == 'string' and 'enum' in inner:
            self.emit_enum(rname, inner)
        else:
            t = self.rust_type(inner, rname) if not ('properties' in inner) else 'Value'
            if nullable:
                t = f'Option<{t}>'
            code = doc_lines(s.get('description'), '') + [f'pub type {rname} = {t};']
            self.items.append((rname, '\n'.join(code)))
            self.default_ok[rname] = True

    def emit_enum(self, rname, s):
        values = [v for v in s['enum'] if v is not None]
        lines = doc_lines(s.get('description'), '')
        lines.append('#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]')
        lines.append(f'pub enum {rname} {{')
        seen = set()
        for v in values:
            vn = pascal(str(v).lower()) if str(v).isupper() else pascal(str(v))
            while vn in seen or vn == 'Unknown':
                vn += '_'
            seen.add(vn)
            lines.append(f'    #[serde(rename = {json.dumps(str(v), ensure_ascii=False)})]')
            lines.append(f'    {vn},')
        lines.append('    /// この版の SDK が知らない値（サーバーが値を足したとき）。')
        lines.append('    #[serde(other)]')
        lines.append('    Unknown,')
        lines.append('}')
        lines.append('')
        lines.append(f'impl {rname} {{')
        lines.append('    /// JSON での値（`Unknown` は `None`）。')
        lines.append('    pub fn as_str(self) -> Option<&\'static str> {')
        lines.append('        match self {')
        seen2 = []
        for v, vn in zip(values, self._variant_names(values)):
            lines.append(f'            {rname}::{vn} => Some({json.dumps(str(v), ensure_ascii=False)}),')
            seen2.append(vn)
        lines.append(f'            {rname}::Unknown => None,')
        lines.append('        }')
        lines.append('    }')
        lines.append('}')
        self.items.append((rname, '\n'.join(lines)))
        self.default_ok[rname] = False
        self.copy_ok[rname] = True

    def _variant_names(self, values):
        seen = set()
        out = []
        for v in values:
            vn = pascal(str(v).lower()) if str(v).isupper() else pascal(str(v))
            while vn in seen or vn == 'Unknown':
                vn += '_'
            seen.add(vn)
            out.append(vn)
        return out

    def emit_struct(self, rname, s, patch):
        req = set(s.get('required', []))
        fields = []
        for pname in sorted(s['properties'].keys()):
            ps = s['properties'][pname]
            inner, nullable = self.nullable_split(ps)
            t = self.rust_type(inner, rname + pascal(pname))
            fname = snake_field(pname)
            attrs = []
            raw = fname[2:] if fname.startswith('r#') else fname
            if raw != pname:
                attrs.append(f'rename = {json.dumps(pname)}')
            if patch and nullable:
                ty = f'Option<Option<{t}>>'
                attrs += ['default', 'skip_serializing_if = "Option::is_none"', 'with = "crate::double_option"']
            elif pname in req and not nullable:
                ty = t
            elif pname in req and nullable:
                ty = f'Option<{t}>'
                attrs.append('default')
            else:
                ty = f'Option<{t}>'
                attrs += ['default', 'skip_serializing_if = "Option::is_none"']
            desc = ps.get('description') if isinstance(ps, dict) else None
            fields.append((fname, ty, attrs, desc, t))
        lines = doc_lines(s.get('description'), '')
        lines.append('#[derive(Debug, Clone, PartialEq, Serialize, Deserialize__DEFAULT__)]')
        lines.append(f'pub struct {rname} {{')
        for fname, ty, attrs, desc, _ in fields:
            lines += doc_lines(desc, '    ')
            if attrs:
                lines.append(f'    #[serde({", ".join(attrs)})]')
            lines.append(f'    pub {fname}: {ty},')
        lines.append('}')
        self.items.append((rname, '\n'.join(lines)))
        self.struct_fields = getattr(self, 'struct_fields', {})
        self.struct_fields[rname] = [ty for _, ty, _, _, _ in fields]

    def run(self):
        for name in schemas:
            self.emit_schema(name, schemas[name])
            while self.pending:
                n, s, kind = self.pending.pop(0)
                if kind == 'enum':
                    self.emit_enum(n, s)
                else:
                    self.emit_struct(n, s, patch=False)
        # Default を derive できるか（すべてのフィールドが Default を持つ）
        sf = getattr(self, 'struct_fields', {})

        def ok_type(ty, stack):
            ty = ty.strip()
            if ty.startswith('Option<') or ty.startswith('Vec<') or ty.startswith('BTreeMap<'):
                return True
            if ty in ('String', 'u64', 'i64', 'f64', 'bool', 'Value'):
                return True
            if ty in sf:
                if ty in stack:
                    return False
                return all(ok_type(t, stack | {ty}) for t in sf[ty])
            return self.default_ok.get(ty, False)

        out = []
        for n, code in self.items:
            if '__DEFAULT__' in code:
                d = ', Default' if ok_type(n, set()) else ''
                code = code.replace('__DEFAULT__', d)
            out.append(code)
        return out


g = Gen()
items = g.run()
header = f'''//! 管理 API のモデル（`docs/api/openapi.yaml` の `components.schemas`）。
//!
//! 生成物: 手で直さない。`python3 crates/senca-client/gen/gen_models.py` で作り直す。
//! 規則は生成器の先頭の説明を参照（未知のフィールドは読み飛ばす、知らない enum の値は `Unknown`、
//! `*Patch` の null になりうるフィールドは `Option<Option<T>>`）。
#![allow(clippy::large_enum_variant, clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

use serde::{{Deserialize, Serialize}};
#[allow(unused_imports)]
use serde_json::Value;
#[allow(unused_imports)]
use std::collections::BTreeMap;
'''
body = header + '\n' + '\n\n'.join(items) + '\n'
os.makedirs(os.path.dirname(OUT), exist_ok=True)
with open(OUT, 'w', encoding='utf-8') as f:
    f.write(body)
print(f'{OUT}: {len(items)} items from {len(schemas)} schemas')
