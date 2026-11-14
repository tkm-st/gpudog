#!/usr/bin/env python3
"""Tests for third-party-licenses.py on made-up metadata; files stay under tmp/.

Dockerfile.release runs this before packaging, on the image's Python 3.7.
"""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("third_party_licenses", str(ROOT / "scripts/third-party-licenses.py"))
LICENSES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LICENSES)

NORMAL = [{"kind": None, "target": None}]
DEV = [{"kind": "dev", "target": None}]
BUILD = [{"kind": "build", "target": None}]


class ThirdPartyLicensesTests(unittest.TestCase):
    def setUp(self):
        (ROOT / "tmp").mkdir(exist_ok=True)
        self.scratch = tempfile.TemporaryDirectory(dir=str(ROOT / "tmp"))
        self.root = Path(self.scratch.name)
        self.packages = []
        self.nodes = []

    def tearDown(self):
        self.scratch.cleanup()

    def crate(self, name, deps=(), license="MIT", files=("LICENSE",), **extra):
        directory = self.root / name
        directory.mkdir()
        for file_name in files:
            (directory / file_name).write_bytes("{} terms\r\nline two\r\n".format(name).encode("utf-8"))
        package = {"id": name, "name": name, "version": "1.0.0", "license": license,
                   "license_file": None, "repository": "https://example.invalid/" + name,
                   "manifest_path": str(directory / "Cargo.toml")}
        package.update(extra)
        self.packages.append(package)
        self.nodes.append({"id": name, "deps": [{"pkg": dep, "dep_kinds": kinds} for dep, kinds in deps]})

    def metadata(self):
        return {"packages": self.packages, "workspace_members": ["gpudog", "gpudogd", "gpudog-core"],
                "resolve": {"nodes": self.nodes}}

    def graph(self):
        self.crate("gpudog", [("gpudog-core", NORMAL), ("tester", DEV)])
        self.crate("gpudogd", [("gpudog-core", NORMAL), ("builder", BUILD)])
        self.crate("gpudog-core", [("zeta", NORMAL), ("alpha", NORMAL + DEV)])
        self.crate("zeta", [("alpha", NORMAL)], license="MIT/Apache-2.0", files=("LICENSE-MIT", "LICENSE-APACHE"))
        self.crate("alpha", license="(MIT OR Apache-2.0) AND Unicode-3.0", files=("COPYING", "NOTICE"))
        self.crate("tester")
        self.crate("builder")

    def test_lists_normal_dependencies_in_order_without_workspace_dev_or_build_crates(self):
        self.graph()
        names = [package["name"] for package in LICENSES.third_party_crates(self.metadata())]
        self.assertEqual(names, ["alpha", "zeta"])

    def test_copies_every_license_file_with_lf_line_endings(self):
        self.graph()
        text = LICENSES.render(self.metadata())
        self.assertTrue(text.startswith("GPUDog third-party licenses\n"))
        self.assertIn("alpha 1.0.0\nLicense: (MIT OR Apache-2.0) AND Unicode-3.0\n"
                      "Repository: https://example.invalid/alpha\n\n--- COPYING ---\n\nalpha terms\nline two\n", text)
        self.assertIn("--- NOTICE ---", text)
        self.assertIn("--- LICENSE-APACHE ---", text)
        self.assertNotIn("\r", text)
        self.assertLess(text.index("alpha 1.0.0"), text.index("zeta 1.0.0"))

    def test_a_crate_without_a_license_file_stops_it(self):
        self.graph()
        (self.root / "zeta" / "LICENSE-MIT").unlink()
        (self.root / "zeta" / "LICENSE-APACHE").unlink()
        with self.assertRaises(SystemExit) as stopped:
            LICENSES.render(self.metadata())
        self.assertIn("zeta 1.0.0 ships no license file", str(stopped.exception))

    def test_a_license_outside_the_allowlist_stops_it(self):
        self.graph()
        self.packages[3]["license"] = "MPL-2.0 OR MIT"
        with self.assertRaises(SystemExit) as stopped:
            LICENSES.render(self.metadata())
        self.assertIn("MPL-2.0", str(stopped.exception))

    def test_a_license_directory_stops_it(self):
        self.graph()
        (self.root / "alpha" / "LICENSES").mkdir()
        with self.assertRaises(SystemExit) as stopped:
            LICENSES.render(self.metadata())
        self.assertIn("LICENSES/", str(stopped.exception))

    def test_a_declared_license_file_without_an_expression_is_used(self):
        self.graph()
        self.packages[4].update(license=None, license_file="TERMS.txt", repository=None)
        (self.root / "alpha" / "TERMS.txt").write_text("custom\n")
        text = LICENSES.render(self.metadata())
        self.assertIn("alpha 1.0.0\nLicense: see the file below\n\n--- COPYING ---", text)
        self.assertIn("--- TERMS.txt ---\n\ncustom", text)

    def test_a_failing_cargo_says_how_to_fetch_the_sources(self):
        original = LICENSES.subprocess.check_output
        def fail(command, **_):
            raise LICENSES.subprocess.CalledProcessError(101, command)
        def missing(command, **_):
            raise FileNotFoundError(command[0])
        try:
            LICENSES.subprocess.check_output = fail
            with self.assertRaises(SystemExit) as stopped:
                LICENSES.load_metadata(None)
            self.assertIn("cargo fetch --locked --target x86_64-unknown-linux-gnu", str(stopped.exception))
            LICENSES.subprocess.check_output = missing
            with self.assertRaises(SystemExit) as stopped:
                LICENSES.load_metadata(None)
            self.assertIn("cargo is not on PATH", str(stopped.exception))
        finally:
            LICENSES.subprocess.check_output = original

    def test_reads_metadata_from_a_file(self):
        self.graph()
        path = self.root / "metadata.json"
        path.write_text(json.dumps(self.metadata()))
        self.assertEqual(len(LICENSES.third_party_crates(LICENSES.load_metadata(str(path)))), 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
