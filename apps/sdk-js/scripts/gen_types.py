#!/usr/bin/env python3
"""docs/api/openapi.yaml から TypeScript の型（src/rest/openapi.gen.d.ts）と操作の表（src/rest/operations.gen.js）を作る。

    python3 apps/sdk-js/scripts/gen_types.py [openapi.yaml] [出力ディレクトリ]

- `components.schemas` を 1 つずつ `export type` にする（説明は JSDoc に）。
- `paths` の操作（`/api/v1` の下だけ。署名付きの再生パス・稼働確認は除く）を operationId → {method, path} の表にする。
  SDK の試験が「すべての操作に SDK のメソッドがある」ことをこの表で確かめる（仕様が増えたら試験が落ちる）。
- 出力は決定的（同じ入力から同じ出力）。手で直さない。
"""
import json
import os
import sys

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, '..', '..', '..'))
SPEC = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, 'docs', 'api', 'openapi.yaml')
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(HERE, '..', 'src', 'rest')
METHODS = ('get', 'post', 'put', 'patch', 'delete')
# /api/v1 の外（servers が `/`）の操作は管理 API の client の対象にしない
OUTSIDE_PREFIXES = ('/s/', '/healthz', '/readyz')


def ident(name):
    return name.replace('-', '_').replace('.', '_')


def key(k):
    return k if k.replace('_', 'a').isalnum() and not k[0].isdigit() else json.dumps(k)


def doc(text, indent):
    if not text:
        return ''
    lines = str(text).strip().replace('*/', '*\\/').split('\n')
    pad = ' ' * indent
    if len(lines) == 1:
        return f'{pad}/** {lines[0]} */\n'
    return pad + '/**\n' + ''.join(f'{pad} * {l}'.rstrip() + '\n' for l in lines) + pad + ' */\n'


def ts(s, indent=0):
    """JSON Schema → TypeScript の型の式。"""
    if s is None or s is True or s == {}:
        return 'unknown'
    if s is False:
        return 'never'
    if '$ref' in s:
        t = s['$ref'].split('/')[-1]
        return t + (' | null' if s.get('nullable') else '')
    if 'const' in s:
        return json.dumps(s['const'], ensure_ascii=False)
    for comb, sep in (('oneOf', ' | '), ('anyOf', ' | '), ('allOf', ' & ')):
        if comb in s:
            parts = [ts(x, indent) for x in s[comb]]
            base = {k: v for k, v in s.items() if k not in (comb, 'description', 'title', 'default', 'example')}
            if base.get('properties') or base.get('type') == 'object' and comb == 'allOf':
                parts.append(ts(base, indent))
            uniq = []
            for p in parts:
                if p not in uniq:
                    uniq.append(p)
            out = sep.join(f'({p})' if (' | ' in p and sep == ' & ') else p for p in uniq)
            return out
    if 'enum' in s:
        t = ' | '.join(json.dumps(v, ensure_ascii=False) for v in s['enum'])
        return t
    typ = s.get('type')
    if isinstance(typ, list):
        parts = []
        for t1 in typ:
            sub = dict(s)
            sub['type'] = t1
            parts.append(ts(sub, indent) if t1 != 'null' else 'null')
        return ' | '.join(dict.fromkeys(parts))
    nullable = ' | null' if s.get('nullable') else ''
    if typ in ('integer', 'number'):
        return 'number' + nullable
    if typ == 'string':
        return 'string' + nullable
    if typ == 'boolean':
        return 'boolean' + nullable
    if typ == 'null':
        return 'null'
    if typ == 'array':
        it = ts(s.get('items'), indent)
        if ' ' in it:
            return f'Array<{it}>' + nullable
        return f'{it}[]' + nullable
    if typ == 'object' or 'properties' in s or 'additionalProperties' in s:
        return obj(s, indent) + nullable
    return 'unknown'


def obj(s, indent):
    props = s.get('properties') or {}
    req = set(s.get('required') or [])
    ap = s.get('additionalProperties', None)
    pad = ' ' * (indent + 2)
    if not props:
        if isinstance(ap, dict):
            return f'{{ [key: string]: {ts(ap, indent)} }}'
        if ap is False:
            return 'Record<string, never>'
        return 'Record<string, unknown>'
    body = ''
    for name in sorted(props):
        p = props[name]
        body += doc(p.get('description') if isinstance(p, dict) else None, indent + 2)
        opt = '' if name in req else '?'
        body += f'{pad}{key(name)}{opt}: {ts(p, indent + 2)};\n'
    if isinstance(ap, dict):
        body += f'{pad}[key: string]: unknown;\n'
    return '{\n' + body + ' ' * indent + '}'


def main():
    spec = yaml.safe_load(open(SPEC, encoding='utf-8'))
    schemas = spec.get('components', {}).get('schemas', {})
    out = []
    out.append('// 生成物: apps/sdk-js/scripts/gen_types.py が docs/api/openapi.yaml から作る。手で直さない。\n')
    out.append(f'// 管理 API {spec.get("info", {}).get("version", "")} の components.schemas（{len(schemas)} 個）。\n\n')
    for name in sorted(schemas):
        s = schemas[name]
        out.append(doc(s.get('description') if isinstance(s, dict) else None, 0))
        out.append(f'export type {name} = {ts(s, 0)};\n\n')
    out.append('export {};\n')
    with open(os.path.join(OUT, 'openapi.gen.d.ts'), 'w', encoding='utf-8') as f:
        f.write(''.join(out))

    ops = []
    for path, item in spec.get('paths', {}).items():
        if path.startswith(OUTSIDE_PREFIXES):
            continue
        for m in METHODS:
            op = item.get(m)
            if not isinstance(op, dict) or 'operationId' not in op:
                continue
            ops.append((op['operationId'], m.upper(), path, op.get('x-senca-scope')))
    ops.sort()
    js = ['// 生成物: apps/sdk-js/scripts/gen_types.py が docs/api/openapi.yaml から作る。手で直さない。\n',
          '// 管理 API の操作（operationId → method・path・要る権限）。SDK の網羅の試験と `client.request()` の案内に使う。\n\n',
          '/** @type {Readonly<Record<string, {method: string, path: string, scope: string | null}>>} */\n',
          'export const OPERATIONS = Object.freeze({\n']
    for oid, m, p, scope in ops:
        js.append(f'  {oid}: {{ method: {json.dumps(m)}, path: {json.dumps(p)}, scope: {json.dumps(scope)} }},\n')
    js.append('});\n')
    with open(os.path.join(OUT, 'operations.gen.js'), 'w', encoding='utf-8') as f:
        f.write(''.join(js))
    print(f'{len(schemas)} schemas, {len(ops)} operations')


if __name__ == '__main__':
    main()
