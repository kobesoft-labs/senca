#!/usr/bin/env python3
"""CycloneDX 1.5（JSON）の SBOM を作る（仕様 9.5・AC-25・REQ-007 AC-R7-03）。標準ライブラリだけを使う。

    sbom.py --metadata META.json --lock Cargo.lock [--dpkg DPKG.tsv] [--root senca-node --root senca-codec-worker]
            [--version 0.1.0] [--commit HASH] [--image senca/node:TAG] [--epoch SOURCE_DATE_EPOCH] -o sbom.json

- Rust の部品: `cargo metadata --format-version 1 --locked --filter-platform <triple>` の出力から、
  配布する実行ファイル（--root）の通常の依存（dev は除く）を辿る。build だけの依存（build.rs・proc-macro）は
  `scope: excluded` で載せる（配布物には入らないがビルドに使う）。
  ライセンスは Cargo.toml の `license`（SPDX 式）、ハッシュは Cargo.lock の checksum（crates.io の .crate の SHA-256）。
- OS の部品（--dpkg）: Docker イメージの中の `dpkg-query` の出力（tools/release/sbom.sh が作る）。
  Debian の copyright から拾った License 欄は参考（SPDX とは限らない）なので `licenses[].license.name` に入れる。

出力は同じ入力なら同じバイト列になる（並び順を固定し、時刻は --epoch から作る）。
"""

import argparse
import datetime
import hashlib
import json
import sys
import tomllib
import uuid


def load_lock(path):
    with open(path, "rb") as f:
        lock = tomllib.load(f)
    out = {}
    for p in lock.get("package", []):
        out[(p["name"], p["version"])] = p
    return out


def purl_cargo(name, version):
    return f"pkg:cargo/{name}@{version}"


def license_entry(expr):
    if not expr:
        return []
    # Cargo の古い書き方（"MIT/Apache-2.0"）は OR として SPDX 式に直す
    e = expr.replace("/", " OR ") if "/" in expr and " " not in expr else expr
    return [{"expression": e}]


def walk(meta, roots):
    """roots から辿った部品。値は 'required'（実行時）か 'excluded'（build だけ）。"""
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    by_name = {}
    for pid, p in pkgs.items():
        if pid in meta["workspace_members"]:
            by_name[p["name"]] = pid
    scope = {}
    deps_of = {}

    def visit(pid, sc):
        prev = scope.get(pid)
        if prev == "required" or prev == sc:
            return
        scope[pid] = sc if prev is None else ("required" if "required" in (prev, sc) else sc)
        node = nodes[pid]
        for d in node["deps"]:
            kinds = {k.get("kind") for k in d["dep_kinds"]}
            deps_of.setdefault(pid, set()).add(d["pkg"])
            if None in kinds or "normal" in kinds:
                # proc-macro は実行ファイルに入らない
                target_kinds = {k for t in pkgs[d["pkg"]]["targets"] for k in t["kind"]}
                if "proc-macro" in target_kinds:
                    visit(d["pkg"], "excluded")
                else:
                    visit(d["pkg"], sc)
            elif "build" in kinds:
                visit(d["pkg"], "excluded")
            # dev は辿らない

    for r in roots:
        if r not in by_name:
            sys.exit(f"sbom: ワークスペースに {r} がない")
        visit(by_name[r], "required")
    return pkgs, scope, deps_of, [by_name[r] for r in roots]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--metadata", required=True)
    ap.add_argument("--lock", required=True)
    ap.add_argument("--dpkg")
    ap.add_argument("--root", action="append", default=[])
    ap.add_argument("--version", default="0.0.0")
    ap.add_argument("--commit", default="")
    ap.add_argument("--image", default="")
    ap.add_argument("--epoch", type=int, default=0)
    ap.add_argument("-o", "--output", required=True)
    a = ap.parse_args()
    roots = a.root or ["senca-node", "senca-codec-worker"]

    with open(a.metadata) as f:
        meta = json.load(f)
    lock = load_lock(a.lock)
    pkgs, scope, deps_of, root_ids = walk(meta, roots)

    components = []
    ref_of = {}
    for pid in sorted(scope, key=lambda i: (pkgs[i]["name"], pkgs[i]["version"])):
        p = pkgs[pid]
        ref = purl_cargo(p["name"], p["version"])
        ref_of[pid] = ref
        c = {
            "type": "application" if p["name"] in roots else "library",
            "bom-ref": ref,
            "name": p["name"],
            "version": p["version"],
            "scope": scope[pid],
            "purl": ref,
            "licenses": license_entry(p.get("license")),
        }
        if not c["licenses"] and p.get("license_file"):
            c["licenses"] = [{"license": {"name": "LicenseRef-file:" + p["license_file"].rsplit("/", 1)[-1]}}]
        lk = lock.get((p["name"], p["version"]))
        if lk and lk.get("checksum"):
            c["hashes"] = [{"alg": "SHA-256", "content": lk["checksum"]}]
        src = p.get("source") or ""
        props = [{"name": "cdx:cargo:source", "value": src or "workspace"}]
        if src.startswith("registry+"):
            c["externalReferences"] = [
                {"type": "distribution", "url": f"https://crates.io/crates/{p['name']}/{p['version']}"}
            ]
        if p.get("repository"):
            c.setdefault("externalReferences", []).append({"type": "vcs", "url": p["repository"]})
        c["properties"] = props
        components.append(c)

    dependencies = []
    for pid in sorted(scope, key=lambda i: ref_of[i]):
        ds = sorted(ref_of[d] for d in deps_of.get(pid, ()) if d in ref_of)
        dependencies.append({"ref": ref_of[pid], "dependsOn": ds})

    # OS の部品
    if a.dpkg:
        os_refs = []
        with open(a.dpkg) as f:
            for line in f:
                parts = line.rstrip("\n").split("\t")
                if len(parts) < 4 or not parts[0]:
                    continue
                name, ver, arch, srcpkg = parts[:4]
                lic = [x for x in (parts[4].split("|") if len(parts) > 4 and parts[4] else []) if x]
                ref = f"pkg:deb/ubuntu/{name}@{ver}?arch={arch}"
                os_refs.append(ref)
                c = {
                    "type": "library",
                    "bom-ref": ref,
                    "name": name,
                    "version": ver,
                    "scope": "required",
                    "purl": ref,
                    "licenses": [{"license": {"name": l}} for l in sorted(set(lic))[:16]],
                    "properties": [
                        {"name": "cdx:deb:source", "value": srcpkg or name},
                        {"name": "senca:layer", "value": "os"},
                    ],
                }
                components.append(c)
        if a.image:
            img_ref = f"oci:{a.image}"
            components.append({
                "type": "container",
                "bom-ref": img_ref,
                "name": a.image,
                "version": a.version,
                "scope": "required",
            })
            dependencies.append({"ref": img_ref, "dependsOn": sorted(os_refs + [ref_of[r] for r in root_ids])})

    ts = datetime.datetime.fromtimestamp(a.epoch, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    # serialNumber は内容から決める（同じ入力 → 同じ SBOM）
    digest = hashlib.sha256(json.dumps(components, sort_keys=True).encode()).hexdigest()
    serial = "urn:uuid:" + str(uuid.UUID(digest[:32]))
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": serial,
        "version": 1,
        "metadata": {
            "timestamp": ts,
            "tools": {"components": [{"type": "application", "name": "senca tools/release/sbom.py", "version": "1"}]},
            "component": {
                "type": "application",
                "bom-ref": "senca",
                "name": "senca",
                "version": a.version,
                "licenses": [{"license": {"id": "Apache-2.0"}}],
                "properties": [{"name": "senca:commit", "value": a.commit}] if a.commit else [],
            },
        },
        "components": components,
        "dependencies": [{"ref": "senca", "dependsOn": sorted(ref_of[r] for r in root_ids)}] + dependencies,
    }
    with open(a.output, "w") as f:
        json.dump(bom, f, indent=2, sort_keys=False, ensure_ascii=False)
        f.write("\n")
    n_req = sum(1 for c in components if c.get("scope") == "required")
    print(f"sbom: {len(components)} 部品（実行時 {n_req}）→ {a.output}")


if __name__ == "__main__":
    main()
