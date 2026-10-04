import importlib.util
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("intel_package", ROOT / "scripts/verify-macos-intel.py")
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


class IntelPackageTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="csswitch-intel-package-")
        self.addCleanup(self.temporary.cleanup)
        self.mount = Path(self.temporary.name)
        self.app = self.mount / "CSSwitch.app"
        contents = self.app / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        self.binaries = contents / "MacOS"
        for name in ("desktop", "csswitch-gateway"):
            (self.binaries / name).write_bytes(b"x86_64")
            (self.binaries / name).chmod(0o755)
        config = json.loads((ROOT / "desktop/src-tauri/tauri.conf.json").read_text())
        self.info_path = contents / "Info.plist"
        self.info = {
            "CFBundleIdentifier": config["identifier"],
            "CFBundleShortVersionString": config["version"],
            "CFBundleExecutable": "desktop",
            "LSMinimumSystemVersion": "13.0",
        }
        self.write_info()
        for source, destination in config["bundle"]["resources"].items():
            output = contents / "Resources" / destination
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / "desktop/src-tauri" / source, output)
        (self.mount / "Applications").symlink_to("/Applications")
        self.signing_metadata = b"Signature=adhoc\n"
        self.mock_run = patch.object(verifier, "run", side_effect=self.fake_tool).start()
        self.addCleanup(patch.stopall)

    def write_info(self):
        self.info_path.write_bytes(plistlib.dumps(self.info))

    def fake_tool(self, *args, **kwargs):
        if args[0] == "lipo":
            return Path(args[-1]).read_bytes()
        if args[0] == "codesign":
            if "--display" in args:
                self.assertTrue(kwargs.get("read_stderr"))
                return self.signing_metadata
            return b""
        raise AssertionError(f"Unexpected command: {args}")

    def test_exact_intel_payload_passes_with_runtime_explicitly_unverified(self):
        report = verifier.verify_image_root(self.mount)
        self.assertEqual(report["target"], "x86_64-apple-darwin")
        self.assertEqual(report["science_runtime"], "not-tested")
        self.assertEqual(report["live_provider"], "not-tested")
        self.assertEqual(report["signature_type"], "ad-hoc")
        self.mock_run.assert_any_call("codesign", "--verify", "--deep", "--strict", str(self.app))

    def test_each_binary_must_be_intel(self):
        for name in ("desktop", "csswitch-gateway"):
            with self.subTest(name=name):
                binary = self.binaries / name
                binary.write_bytes(b"arm64")
                with self.assertRaisesRegex(ValueError, "must be Intel"):
                    verifier.verify_app(self.app)
                binary.write_bytes(b"x86_64")

    def test_missing_gateway_fails(self):
        (self.binaries / "csswitch-gateway").unlink()
        with self.assertRaisesRegex(ValueError, "Missing regular executable"):
            verifier.verify_app(self.app)

    def test_each_binary_requires_executable_permissions_for_all_users(self):
        for name in ("desktop", "csswitch-gateway"):
            for mode in (0o644, 0o744, 0o754):
                with self.subTest(name=name, mode=oct(mode)):
                    binary = self.binaries / name
                    binary.chmod(mode)
                    with self.assertRaisesRegex(ValueError, "Executable permissions"):
                        verifier.verify_app(self.app)
                    binary.chmod(0o755)

    def test_changed_resource_fails(self):
        (self.app / "Contents/Resources/scripts/doctor.sh").write_text("stale resource")
        with self.assertRaisesRegex(ValueError, "resource does not match"):
            verifier.verify_app(self.app)

    def test_version_or_minimum_system_mismatch_fails(self):
        for key in ("CFBundleShortVersionString", "LSMinimumSystemVersion"):
            old = self.info[key]
            self.info[key] = "0.0"
            self.write_info()
            with self.assertRaisesRegex(ValueError, key):
                verifier.verify_app(self.app)
            self.info[key] = old
        self.write_info()

    def test_old_acceptance_bundle_in_dmg_fails(self):
        (self.mount / "CSSwitch Acceptance.app").mkdir()
        with self.assertRaisesRegex(ValueError, "Unexpected DMG root"):
            verifier.verify_image_root(self.mount)

    def test_wrong_applications_link_fails(self):
        link = self.mount / "Applications"
        link.unlink()
        link.symlink_to("/tmp")
        with self.assertRaisesRegex(ValueError, "must point to /Applications"):
            verifier.verify_image_root(self.mount)

    def test_failed_signature_verification_fails(self):
        def fail_codesign(*args, **kwargs):
            if args[0] == "codesign":
                raise subprocess.CalledProcessError(1, "codesign")
            return self.fake_tool(*args, **kwargs)

        self.mock_run.side_effect = fail_codesign
        with self.assertRaises(subprocess.CalledProcessError):
            verifier.verify_app(self.app)

    def test_certificate_signature_is_observed_instead_of_inferred_from_config(self):
        self.signing_metadata = b"Authority=Developer ID Application: Fixture\n"
        report = verifier.verify_app(self.app)
        self.assertEqual(report["signature_type"], "certificate")
        self.assertNotIn("signing_identity", report)

    def test_unknown_signature_metadata_fails(self):
        self.signing_metadata = b""
        with self.assertRaisesRegex(ValueError, "Cannot establish"):
            verifier.verify_app(self.app)


if __name__ == "__main__":
    unittest.main()
