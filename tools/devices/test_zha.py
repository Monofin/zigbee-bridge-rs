import subprocess
import tempfile
import unittest
from pathlib import Path

import zha


class CatalogueTest(unittest.TestCase):
    def test_preserves_pairs_inheritance_and_literal_whitespace(self):
        vendor = zha.Module.parse("zhaquirks/vendor/__init__.py", 'NAME = " Vendor"')
        models = zha.Module.parse("zhaquirks/vendor/models.py", """
from . import NAME
class First(CustomDevice):
    signature = {MODELS_INFO: [(NAME, " Model 1"), ("Other", "Model 2")]}
class Second(First):
    pass
""")
        rows = list(zha.rows("test", {m.name: m for m in (vendor, models)}))
        self.assertEqual([(r["symbol"], r["manufacturers"], r["models"]) for r in rows], [
            (symbol, [maker], [model])
            for symbol in ("First", "Second")
            for maker, model in ((" Vendor", " Model 1"), ("Other", "Model 2"))
        ])

    def test_cross_module_signature_references_and_unreviewed_mutations(self):
        base = zha.Module.parse("zhaquirks/vendor/base.py", """
NAME = "Vendor"
class Parent(CustomDevice):
    signature = {MODELS_INFO: [(NAME, "first"), (NAME, "second")]}
""")
        child = zha.Module.parse("zhaquirks/vendor/child.py", """
from copy import deepcopy as duplicate
from .base import Parent as Imported
class A(Imported):
    signature = {MODELS_INFO: Imported.signature[MODELS_INFO].copy()}
class B(A):
    signature = duplicate(A.signature)
    signature[ENDPOINTS][1][INPUT_CLUSTERS].insert(0, 1)
class C(B):
    signature = duplicate(B.signature)
    signature[MODELS_INFO].append(("another", "unknown"))
class D(C):
    pass
""")
        rows = list(zha.rows("test", {m.name: m for m in [base, child]}))
        for name in ("Parent", "A", "B"):
            self.assertEqual([r["models"] for r in rows if r["symbol"] == name],
                             [["first"], ["second"]])
        for name in ("C", "D"):
            self.assertEqual([r["models"] for r in rows if r["symbol"] == name], [[]])

    def test_clone_resets_identity_unless_requested_and_keeps_unknowns(self):
        mod = zha.Module.parse("zhaquirks/models.py", """
base = QuirkBuilder(None, "first")
base.clone().applies_to(f" {'Vendor'}", "second").add_to_registry()
base.clone(False).applies_to(get_manufacturer(), "unknown").add_to_registry()
GreenPowerQuirkBuilder().applies_to(model_id=2).add_to_registry()
""")
        rows = list(zha.rows("test", {mod.name: mod}))
        self.assertEqual([(r["kind"], r["manufacturers"], r["models"]) for r in rows], [
            ("v2", [" Vendor"], ["second"]),
            ("v2", [], ["first"]),
            ("v2-unresolved", [], []),
            ("v2-unresolved", [], []),
        ])

    def test_uses_git_objects_and_never_executes_upstream(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True)
            git("init", "-q")
            (root / "zhaquirks").mkdir()
            source = root / "zhaquirks/fixture.py"
            source.write_text('raise RuntimeError("must never run")\nQuirkBuilder("vendor", "original").add_to_registry()\n')
            git("add", "zhaquirks")
            git("-c", "user.name=test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture")
            source.write_text('raise RuntimeError("dirty checkout")')
            row, = zha.rows(*zha.load(root))
            self.assertEqual(row["models"], ["original"])
            self.assertEqual(row["path"], "zhaquirks/fixture.py")
            self.assertEqual(len(row["revision"]), 40)


if __name__ == "__main__":
    unittest.main()
