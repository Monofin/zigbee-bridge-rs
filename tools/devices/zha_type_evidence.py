"""Recover literal v1 signature descriptors without loading or executing upstream code."""
import ast
import hashlib
import re
import subprocess
import zipfile

import zha


class Signatures:
    def __init__(self, checkout, wheel, revision):
        actual, self.modules = zha.load(checkout)
        if actual != revision:
            raise ValueError("ZHA checkout differs from source lock")
        lock = subprocess.check_output(["git", "-C", str(checkout), "show", revision + ":uv.lock"]).decode()
        wheel_hash = hashlib.sha256(wheel.read_bytes()).hexdigest()
        if not re.search(r'url = "[^"]*/zigpy-[^"/]+\.whl", hash = "sha256:' + wheel_hash + '"', lock):
            raise ValueError("zigpy wheel does not match this ZHA commit's uv.lock")
        with zipfile.ZipFile(wheel) as files:
            for path in files.namelist():
                if path.startswith(("zigpy/profiles/", "zigpy/zcl/clusters/", "zigpy/zgp/")) and path.endswith(".py"):
                    module = zha.Module.parse(path, files.read(path))
                    self.modules[module.name] = module
        self.module_aliases = {(mod.name, alias.asname): alias.name
                               for mod in self.modules.values() for stmt in mod.body
                               if isinstance(stmt, ast.Import) for alias in stmt.names if alias.asname}
        self.classes = {(mod.name, x.name): x for mod in self.modules.values()
                        for x in mod.body if isinstance(x, ast.ClassDef)}

    def profile_type_ids(self):
        """Profile-qualified numeric enum values recorded by the pinned zigpy release."""
        identifiers = {}
        for name, mod in self.modules.items():
            cls = self.classes.get((name, "DeviceType"))
            if not name.startswith("zigpy.profiles.") or cls is None:
                continue
            profile = self.number(mod, mod.bindings["PROFILE_ID"])
            for symbol, expr in zha.assignments(cls.body):
                device = self.number(mod, expr)
                if profile is None or device is None:
                    raise ValueError(f"unresolved profile enum {name}.{symbol}")
                identifiers[profile, device] = name + ".DeviceType." + symbol
        return identifiers

    def path(self, mod, node):
        if isinstance(node, ast.Attribute):
            base = self.path(mod, node.value)
            return (*base, node.attr) if base else ()
        if isinstance(node, ast.Name):
            module = self.module_aliases.get((mod.name, node.id))
            if module:
                return (module,)
            if node.id in mod.imports:
                target, alias = mod.imports[node.id]
                if target + "." + alias in self.modules:
                    return (target + "." + alias,)
                return (target, alias)
            return (mod.name, node.id)
        return ()

    def number(self, mod, expr, seen=frozenset()):
        if isinstance(expr, ast.Constant) and type(expr.value) is int:
            return expr.value if 0 <= expr.value <= 0xffff else None
        return self.resolve(self.path(mod, expr), seen)

    def resolve(self, path, seen):
        if not path or path in seen or path[0] not in self.modules:
            return None
        seen = seen | {path}
        mod = self.modules[path[0]]
        if len(path) == 2 and path[1] in mod.bindings:
            return self.number(mod, mod.bindings[path[1]], seen)
        if len(path) == 3 and path[:2] in self.classes:
            cls = self.classes[path[:2]]
            attr = dict(zha.assignments(cls.body)).get(path[2])
            if attr is not None:
                return self.number(mod, attr, seen)
            # Multiple bases can override the inherited value.
            if len(cls.bases) == 1:
                return self.resolve((*self.path(mod, cls.bases[0]), path[2]), seen)
        if len(path) >= 2 and path[1] in mod.imports:
            target, name = mod.imports[path[1]]
            return self.resolve((target, name, *path[2:]), seen)
        return None

    def signature(self, key, seen=frozenset()):
        if key not in self.classes or key in seen:
            return None
        cls = self.classes[key]
        mod = self.modules[key[0]]
        signature = dict(zha.assignments(cls.body)).get("signature")
        for stmt in cls.body:
            if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                continue
            if any(isinstance(n, ast.Name) and n.id == "signature" and isinstance(n.ctx, ast.Load)
                   for n in ast.walk(stmt)):
                return None  # mutated or constructed: no stale snapshot of the base
        if signature is None and len(cls.bases) == 1:
            return self.signature(self.path(mod, cls.bases[0]), seen | {key})
        if isinstance(signature, ast.Dict):
            return mod, signature
        return None

    def builder_signature(self, mod, row):
        registration = next((n for stmt in mod.body for n in ast.walk(stmt)
                             if isinstance(n, ast.Call) and n.lineno == row["line"]
                             and isinstance(n.func, ast.Attribute) and n.func.attr == "add_to_registry"), None)
        node = registration
        while isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if node.func.attr == "filter":
                predicate = node.args[0] if node.args else None
                if (isinstance(predicate, ast.Call) and zha.ident(predicate.func) == "signature_matches"
                        and len(predicate.args) == 1):
                    ref = predicate.args[0]
                    if isinstance(ref, ast.Attribute) and ref.attr == "signature":
                        return self.signature(self.path(mod, ref.value)), True
                    if isinstance(ref, ast.Name) and isinstance(mod.bindings.get(ref.id), ast.Dict):
                        return (mod, mod.bindings[ref.id]), True
                return None, True
            node = node.func.value
        return None, False

    def endpoints(self, row):
        modname = row["path"].removesuffix(".py").replace("/", ".").removesuffix(".__init__")
        if row["kind"].startswith("v2"):
            sig, filtered = self.builder_signature(self.modules[modname], row)
            if not filtered:
                return None, []
        else:
            sig = self.signature((modname, row["symbol"]))
        if not sig:
            return None, ["signature is inherited nontrivially, computed, or mutated"]
        mod, signature = sig
        def fields(expr):
            if not isinstance(expr, ast.Dict) or None in expr.keys:
                return {}
            return {(zha.ident(k) or "").lower(): v for k, v in zip(expr.keys, expr.values)}
        eps = fields(signature).get("endpoints")
        if not isinstance(eps, ast.Dict):
            return None, ["no literal endpoint map"]
        out, warnings = [], []
        for ep, desc in zip(eps.keys, eps.values):
            point = {"ID": self.number(mod, ep)}
            if point["ID"] is None:
                warnings.append("unresolved endpoint identifier")
            attrs = fields(desc)
            if not attrs:
                warnings.append("endpoint has no literal fields")
            for key, target in [("profile_id","profileID"), ("device_type","deviceID"),
                                ("input_clusters","inputClusters"), ("output_clusters","outputClusters")]:
                if key not in attrs:
                    continue
                expr = attrs[key]
                if key.endswith("clusters") and isinstance(expr, (ast.List, ast.Tuple)):
                    values = [self.number(mod, x) for x in expr.elts]
                    if None not in values:
                        point[target] = values
                    else:
                        # A partial list is a distinct observation from a complete list.
                        point[target + "Known"] = [v for v in values if v is not None]
                        warnings.append(f"{key}: " + ", ".join(ast.unparse(x) for x, v in zip(expr.elts, values) if v is None))
                else:
                    number = self.number(mod, expr)
                    if number is not None and not key.endswith("clusters"):
                        point[target] = number
                    else:
                        warnings.append(f"{key}: {ast.unparse(expr)}")
            out.append(point)
        return out, sorted(set(warnings))
