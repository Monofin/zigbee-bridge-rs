#!/usr/bin/env python3
"""Index ZHA quirk identities from committed Python source, without importing quirks.

Usage: python3 zha.py CHECKOUT > zha.jsonl  (use Python >= upstream syntax).
Each row is one declared identity pair or one explicitly unresolved declaration.
Endpoint signatures, predicates and converter code still need individual review.
Upstream: https://github.com/zigpy/zha-device-handlers (Apache-2.0, LICENSE.md).
"""
from __future__ import annotations

import argparse
import ast
from dataclasses import dataclass, field
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile

import zha_fingerprints


def assignments(body):
    for stmt in body:
        if isinstance(stmt, ast.Assign):
            for target in stmt.targets:
                if isinstance(target, ast.Name):
                    yield target.id, stmt.value
        elif isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name) and stmt.value:
            yield stmt.target.id, stmt.value


def ident(expr):
    if isinstance(expr, ast.Name):
        return expr.id
    if isinstance(expr, ast.Attribute):
        return expr.attr
    if isinstance(expr, ast.Constant) and isinstance(expr.value, str):
        return expr.value
    return None


@dataclass
class Module:
    path: str
    name: str
    body: list
    bindings: dict = field(default_factory=dict)
    imports: dict = field(default_factory=dict)

    @classmethod
    def parse(cls, path, text):
        stem = Path(path).with_suffix("")
        package = ".".join(stem.parent.parts)
        name = package if stem.name == "__init__" else ".".join(stem.parts)
        body = ast.parse(text, filename=path).body
        obj = cls(path, name, body, dict(assignments(body)))
        for stmt in body:
            if isinstance(stmt, ast.ImportFrom):
                target = importlib.util.resolve_name("." * stmt.level + (stmt.module or ""), package)
                for alias in stmt.names:
                    obj.imports[alias.asname or alias.name] = (target, alias.name)
        return obj


def load(checkout):
    def git(*args):
        return subprocess.check_output(["git", "-C", str(checkout), *args])

    revision = git("rev-parse", "--verify", "HEAD^{commit}").decode().strip()
    # Git objects tie every parsed byte to the cited commit despite working-tree edits.
    source = git("archive", "--format=tar", revision, "zhaquirks")
    modules = {}
    with tarfile.open(fileobj=io.BytesIO(source)) as files:
        for member in files:
            if member.isfile() and member.name.endswith(".py"):
                mod = Module.parse(member.name, files.extractfile(member).read())
                modules[mod.name] = mod
    return revision, modules


UNKNOWN = object()


def identity_value(expr, mod, modules, seen=frozenset()):
    if expr is None:
        return None
    if isinstance(expr, ast.Constant) and (expr.value is None or isinstance(expr.value, str)):
        return expr.value
    if isinstance(expr, ast.JoinedStr):
        parts = []
        for part in expr.values:
            if isinstance(part, ast.FormattedValue):
                if part.conversion != -1 or part.format_spec is not None:
                    return UNKNOWN
                part = part.value
            value = identity_value(part, mod, modules, seen)
            if not isinstance(value, str):
                return UNKNOWN
            parts.append(value)
        return "".join(parts)
    if not isinstance(expr, ast.Name) or (mod.name, expr.id) in seen:
        return UNKNOWN
    key = (mod.name, expr.id)
    if expr.id in mod.bindings:
        return identity_value(mod.bindings[expr.id], mod, modules, seen | {key})
    origin, name = mod.imports.get(expr.id, (None, None))
    other = modules.get(origin)
    if other is not None:
        return identity_value(ast.Name(id=name), other, modules, seen | {key})
    return UNKNOWN


def legacy_pairs(mod, signature, modules):
    if not isinstance(signature, ast.Dict) or None in signature.keys:
        return [UNKNOWN]
    fields = {ident(key): value for key, value in zip(signature.keys, signature.values)}
    pairs = fields.get("MODELS_INFO", fields.get("models_info"))
    if isinstance(pairs, (ast.Tuple, ast.List)) and pairs.elts:
        result = []
        for pair in pairs.elts:
            if not isinstance(pair, (ast.Tuple, ast.List)) or len(pair.elts) != 2:
                result.append(UNKNOWN)
            else:
                result.append(tuple(identity_value(v, mod, modules) for v in pair.elts))
        return result
    if pairs is not None and not isinstance(pairs, (ast.Tuple, ast.List)):
        return [UNKNOWN]
    return [tuple(identity_value(fields.get(k, fields.get(k.lower())), mod, modules)
                  for k in ("MANUFACTURER", "MODEL"))]


def arg(call, index, name):
    for kw in call.keywords:
        if kw.arg == name:
            return kw.value
    return call.args[index] if index < len(call.args) else None


def builder_pairs(expr, mod, modules, seen=frozenset()):
    """Read the fluent expression spine only. Do not interpret callbacks or helpers."""
    steps = []
    while isinstance(expr, ast.Call) and isinstance(expr.func, ast.Attribute):
        steps.append((expr.func.attr, expr))
        expr = expr.func.value
    def pair(call):
        if any(isinstance(a, ast.Starred) for a in call.args) or any(k.arg is None for k in call.keywords):
            return UNKNOWN
        return tuple(identity_value(arg(call, i, n), mod, modules)
                     for i, n in enumerate(("manufacturer", "model")))

    if isinstance(expr, ast.Name) and expr.id in mod.bindings and expr.id not in seen:
        pairs, builder = builder_pairs(mod.bindings[expr.id], mod, modules, seen | {expr.id})
    elif isinstance(expr, ast.Call) and ident(expr.func) in {"QuirkBuilder", "TuyaQuirkBuilder", "InovelliQuirkBuilder"}:
        pairs = [pair(expr)] if expr.args or expr.keywords else []
        builder = ident(expr.func)
    else:
        return [UNKNOWN], ident(expr.func) if isinstance(expr, ast.Call) else None
    for method, call in reversed(steps):
        if method == "clone":
            omit = arg(call, 0, "omit_man_model_data")
            if omit is None or (isinstance(omit, ast.Constant) and omit.value is True):
                pairs = []
            elif not isinstance(omit, ast.Constant) or omit.value is not False:
                pairs = [UNKNOWN]
        elif method in ("applies_to", "also_applies_to"):
            pairs.append(pair(call))
    return pairs, builder


def identity_preserved(node):
    """Accept endpoint edits and plain dict unpacking. Other signature uses need review."""
    for stmt in node.body:
        if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            continue
        children = list(ast.walk(stmt))
        safe = {n.value for n in children if isinstance(n, ast.Subscript)
                and ident(n.slice) in ("ENDPOINTS", "endpoints")}
        for literal in (n for n in children if isinstance(n, ast.Dict)):
            safe.update(v for k, v in zip(literal.keys, literal.values) if k is None)
        if any(isinstance(n, ast.Name) and n.id == "signature" and isinstance(n.ctx, ast.Load)
               and n not in safe for n in children):
            return False
    return True


def declarations(modules):
    classes = {(m.name, n.name): n for m in modules.values() for n in m.body if isinstance(n, ast.ClassDef)}

    def parent_pairs(mod, reference, seen, models_info_only):
        if not (isinstance(reference, ast.Attribute) and reference.attr == "signature"
                and isinstance(reference.value, ast.Name)):
            return [UNKNOWN]
        name = reference.value.id
        key = mod.imports.get(name, (mod.name, name))
        if key not in classes or key in seen:
            return [UNKNOWN]
        return class_pairs(modules[key[0]], classes[key], seen | {key}, models_info_only) or [UNKNOWN]

    def class_pairs(mod, node, seen, models_info_only=False):
        direct = dict(assignments(node.body))
        if "signature" not in direct:
            for base in node.bases:
                if isinstance(base, ast.Name):
                    key = mod.imports.get(base.id, (mod.name, base.id))
                    if key in classes and key not in seen:
                        pairs = class_pairs(modules[key[0]], classes[key], seen | {key}, models_info_only)
                        if pairs is not None:
                            return pairs
            return None

        expr = direct["signature"]
        if isinstance(expr, ast.Constant) and expr.value is None:
            return None
        if not identity_preserved(node):
            return [UNKNOWN]
        if isinstance(expr, ast.Call) and isinstance(expr.func, ast.Name):
            if (mod.imports.get(expr.func.id) == ("copy", "deepcopy")
                    and len(expr.args) == 1 and not expr.keywords):
                return parent_pairs(mod, expr.args[0], seen, models_info_only)
        if isinstance(expr, ast.Dict):
            for key, selector in zip(expr.keys, expr.values):
                if ident(key) not in ("MODELS_INFO", "models_info"):
                    continue
                if (isinstance(selector, ast.Call) and isinstance(selector.func, ast.Attribute)
                        and selector.func.attr == "copy" and not selector.args and not selector.keywords):
                    selector = selector.func.value
                if isinstance(selector, ast.Subscript) and ident(selector.slice) in ("MODELS_INFO", "models_info"):
                    return parent_pairs(mod, selector.value, seen, True)
                break
            else:
                if models_info_only:
                    return [UNKNOWN]
        return legacy_pairs(mod, expr, modules)

    for mod in sorted(modules.values(), key=lambda m: m.path):
        for stmt in mod.body:
            if isinstance(stmt, ast.ClassDef):
                pairs = class_pairs(mod, stmt, {(mod.name, stmt.name)})
                if pairs is not None:
                    yield mod.path, stmt.lineno, stmt.name, "v1", pairs
                continue
            if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            for call in ast.walk(stmt):
                if not (isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute)
                        and call.func.attr == "add_to_registry"):
                    continue
                pairs, builder = builder_pairs(call, mod, modules)
                # Registrations inside a loop or conditional cannot be certified as
                # unconditional static declarations, even with literal arguments.
                if not isinstance(stmt, (ast.Expr, ast.Assign, ast.AnnAssign)):
                    pairs = [UNKNOWN]
                symbol = f"{builder or 'registration'}@{call.lineno}"
                yield mod.path, call.lineno, symbol, "v2", pairs or [UNKNOWN]


def rows(revision, modules):
    by_path = {m.path: m for m in modules.values()}
    for path, line, symbol, kind, pairs in declarations(modules):
        for pair in dict.fromkeys(pairs):
            resolved = pair is not UNKNOWN and UNKNOWN not in pair and any(v is not None for v in pair)
            maker, model = pair if resolved else (None, None)
            row = {
                "source": "zha", "revision": revision, "path": path, "line": line, "symbol": symbol,
                "manufacturers": [maker] if maker is not None else [],
                "models": [model] if model is not None else [],
                "kind": kind if resolved else f"{kind}-unresolved",
            }
            if not resolved:
                if symbol.startswith("GreenPowerQuirkBuilder@"):
                    row["reason"] = "green_power_commissioning_identity"
                elif pair == (None, None):
                    row["reason"] = "no_manufacturer_model_selector"
                else:
                    row["reason"] = "dynamic_or_inherited_selector"
            if revision == zha_fingerprints.REVISION:
                filters = zha_fingerprints.fingerprints(by_path[path], row)
                if filters:
                    row["fingerprints"] = filters
            yield row


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    args = parser.parse_args()
    try:
        source = load(args.checkout)
    except SyntaxError as exc:
        parser.exit(1, f"{exc.filename}:{exc.lineno}: cannot parse this upstream revision: "
                      f"{exc.msg}. Use a Python version supporting its syntax (3.12+ for the current pin).\n")
    if source[0] != zha_fingerprints.REVISION:
        import sys
        print("ZHA fingerprint extraction needs review for this revision", file=sys.stderr)
    for row in rows(*source):
        print(json.dumps(row, ensure_ascii=True, separators=(",", ":")))
