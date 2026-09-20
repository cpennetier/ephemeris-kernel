"""Fast fixtures for editable native-output routing; no compiled engine required."""
import importlib.util
import os
from pathlib import Path
import sysconfig
import sys
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("routing", Path(__file__).resolve().parents[1] / "scripts/route-native-extension.py")
routing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(routing)
owned_fixtures = []


class RoutingTests(unittest.TestCase):
    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="ephemeris-native-route-")
        self.base = Path(self.fixture.name)
        owned_fixtures.append(self.base)
        self.repo = self.base / "clone with spaces"
        self.package = self.repo / "python/ephemeris"
        self.package.mkdir(parents=True)
        self.source = self.package / ("_engine" + sysconfig.get_config_var("EXT_SUFFIX"))
        self.source.write_bytes(b"fresh generated extension")
        self.source.chmod(0o755)
        self.target = self.base / "external cargo target"
        self.destination = self.target / "python-extensions" / self.source.name
        self.addCleanup(self.fixture.cleanup)

    def run_route(self, target=None):
        return routing.route_extension(self.repo, str(self.target) if target is None else target)

    def assert_no_scratch(self):
        self.assertFalse(list(self.package.glob(".engine-link-*")))
        self.assertFalse(list(self.destination.parent.glob(".engine-*")))

    def test_unset_and_empty_are_unchanged(self):
        for target in [None, ""]:
            self.assertIsNone(routing.route_extension(self.repo, target))
        self.assertFalse(self.source.is_symlink())
        self.assertFalse(self.target.exists())

    def test_unset_does_not_require_an_extension(self):
        self.source.unlink()
        self.assertIsNone(routing.route_extension(self.repo, None))

    def test_external_bytes_mode_and_unrelated_files(self):
        other = self.package / "_engine.other-abi.so"
        other.write_bytes(b"unrelated ABI")
        self.assertEqual(self.run_route(), self.destination.resolve())
        self.assertTrue(self.source.is_symlink())
        self.assertEqual(self.source.read_bytes(), b"fresh generated extension")
        self.assertEqual(self.destination.stat().st_mode & 0o777, 0o755)
        self.assertEqual(other.read_bytes(), b"unrelated ABI")
        self.assert_no_scratch()

    def test_rebuild_replaces_leaf_then_routes_new_bytes(self):
        self.run_route()
        self.source.unlink()  # Reproduce maturin's replacement of the source leaf.
        self.source.write_bytes(b"second editable build")
        self.run_route()
        self.assertTrue(self.source.is_symlink())
        self.assertEqual(self.destination.read_bytes(), b"second editable build")
        self.assert_no_scratch()

    def test_idempotent_link(self):
        self.run_route()
        before = self.destination.stat().st_ino
        self.run_route()
        self.assertEqual(before, self.destination.stat().st_ino)

    def test_relative_cargo_target(self):
        result = self.run_route("build outputs")
        self.assertEqual(result, (self.repo / "build outputs/python-extensions" / self.source.name).resolve())
        self.assertEqual(self.source.read_bytes(), b"fresh generated extension")

    def test_missing_active_abi_refuses_without_output(self):
        self.source.unlink()
        with self.assertRaisesRegex(RuntimeError, "missing editable extension"):
            self.run_route()
        self.assertFalse(self.target.exists())

    def test_destination_directory_refuses_without_source_change(self):
        self.destination.mkdir(parents=True)
        with self.assertRaisesRegex(RuntimeError, "not a regular file"):
            self.run_route()
        self.assertFalse(self.source.is_symlink())

    def test_destination_symlink_is_not_followed(self):
        self.destination.parent.mkdir(parents=True)
        victim = self.base / "unrelated"
        victim.write_bytes(b"untouched")
        self.destination.symlink_to(victim)
        with self.assertRaisesRegex(RuntimeError, "not a regular file"):
            self.run_route()
        self.assertEqual(victim.read_bytes(), b"untouched")
        self.assertFalse(self.source.is_symlink())

    def test_unexpected_source_symlink_refuses(self):
        victim = self.base / "other extension"
        victim.write_bytes(b"untouched")
        self.source.unlink()
        self.source.symlink_to(victim)
        with self.assertRaisesRegex(RuntimeError, "unexpected native extension"):
            self.run_route()
        self.assertEqual(victim.read_bytes(), b"untouched")
        self.assertFalse(self.target.exists())

    def test_output_inside_package_refuses(self):
        with self.assertRaisesRegex(RuntimeError, "inside the Python package"):
            self.run_route(str(self.package))
        self.assertFalse(self.source.is_symlink())

    def test_symlink_failure_preserves_both_old_files(self):
        self.destination.parent.mkdir(parents=True)
        self.destination.write_bytes(b"old external extension")
        with mock.patch.object(Path, "symlink_to", side_effect=OSError("no symlinks")):
            with self.assertRaises(OSError):
                self.run_route()
        self.assertEqual(self.destination.read_bytes(), b"old external extension")
        self.assertEqual(self.source.read_bytes(), b"fresh generated extension")
        self.assertFalse(self.source.is_symlink())
        self.assert_no_scratch()

    def test_final_replace_failure_retains_importable_source(self):
        original_replace = os.replace
        def replace(source, destination):
            if Path(destination) == self.source.resolve():
                raise OSError("source replacement refused")
            return original_replace(source, destination)
        with mock.patch.object(routing.os, "replace", side_effect=replace):
            with self.assertRaises(OSError):
                self.run_route()
        self.assertEqual(self.source.read_bytes(), b"fresh generated extension")
        self.assertFalse(self.source.is_symlink())
        self.assertEqual(self.destination.read_bytes(), self.source.read_bytes())
        self.assert_no_scratch()


if __name__ == "__main__":
    program = unittest.main(verbosity=2, exit=False)
    assert all(not p.exists() for p in owned_fixtures), "an owned fixture remains"
    print(f"Cleanup verified: all {len(owned_fixtures)} owned temporary fixture roots removed.")
    sys.exit(not program.result.wasSuccessful())
