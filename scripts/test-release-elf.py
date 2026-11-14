#!/usr/bin/env python3
"""Pure regression tests for the release ABI gate; no GPU or Docker needed."""
import importlib.util
import sys
import unittest
from unittest.mock import patch
from pathlib import Path

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("release_elf", Path(__file__).with_name("check-release-elf.py"))
elf = importlib.util.module_from_spec(spec)
spec.loader.exec_module(elf)


class VersionTests(unittest.TestCase):
    def test_minimum_and_older_versions(self):
        elf.validate_versions(["GLIBC_2.2.5", "GLIBC_2.17", "GLIBC_2.28", "GCC_3.0"])

    def test_newer_versions_fail(self):
        for version in ("GLIBC_2.29", "GLIBC_2.34", "GLIBC_2.100", "GLIBC_3.0"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                elf.validate_versions([version])

    def test_unrecognized_versions_fail_closed(self):
        for version in ("GLIBC_PRIVATE", "GLIBC_ABI_DT_RELR", "GLIBC_2x28", "GLIBC_2", "GLIBCXX_3.4.29", "UNKNOWN"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                elf.validate_versions([version])
        with self.assertRaises(ValueError):
            elf.validate_versions([])

    def test_only_needs_are_considered(self):
        text = "Version definition section\nName: GLIBC_9.0\nVersion needs section '.gnu.version_r' contains 2 entries:\nName: GLIBC_2.28 Flags: none\nName: GCC_3.0 Flags: none"
        self.assertEqual(elf.required_versions(text), ["GLIBC_2.28", "GCC_3.0"])
        with self.assertRaises(ValueError):
            elf.required_versions("Version definition section\nName: GLIBC_2.28")

    def test_architecture_interpreter_and_dependencies_are_checked(self):
        header = "Class: ELF64\nMachine: Advanced Micro Devices X86-64\n"
        program = "[Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]"
        dynamic = "(NEEDED) Shared library: [libc.so.6]\n(NEEDED) Shared library: [libgcc_s.so.1]"
        versions = "Version needs section\nName: GLIBC_2.28\nName: GCC_3.0\n"
        with patch.object(elf.subprocess, "check_output", side_effect=[header, program, dynamic, versions]):
            elf.inspect(Path("fixture"))
        for outputs in (["Class: ELF32\nMachine: Intel 80386"], [header, "wrong interpreter"],
                        [header, program, dynamic + "\n(NEEDED) Shared library: [libunexpected.so]"],
                        [header, program, dynamic, versions + "Name: GLIBC_PRIVATE"]):
            with self.subTest(outputs=outputs), patch.object(elf.subprocess, "check_output", side_effect=outputs), self.assertRaises(ValueError):
                elf.inspect(Path("fixture"))


class SnapshotTests(unittest.TestCase):
    def test_one_dashboard_waits_for_a_fresh_frame(self):
        container_spec = importlib.util.spec_from_file_location("container_checks", Path(__file__).with_name("container-check.py"))
        module = importlib.util.module_from_spec(container_spec)
        container_spec.loader.exec_module(module)
        checks = module.Checks.__new__(module.Checks)
        created = []
        checks.client = lambda args: created.append(args) or "test-container"
        logs = iter(["unknown free", "2 free, 0 waiting", "2 free, 0 waiting"])
        def docker(*args):
            text = next(logs) if args[0] == "logs" else "true"
            return module.subprocess.CompletedProcess(args, 0, stdout=text, stderr="")
        checks.docker = docker
        self.assertIn("2 free", checks.wait_snapshot("2 free, 0 waiting", timeout=1))
        self.assertEqual(len(created), 1)


if __name__ == "__main__":
    unittest.main()
