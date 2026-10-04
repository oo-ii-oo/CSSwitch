#!/usr/bin/env python3
"""Check the actual Intel app/DMG payload; never launch Science or a provider."""

import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def run(*args, read_stderr=False):
    result = subprocess.run(args, check=True, capture_output=True)
    return result.stderr if read_stderr else result.stdout


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_app(app, root=ROOT):
    config = json.loads((root / "desktop/src-tauri/tauri.conf.json").read_text())
    if app.name != "CSSwitch.app" or app.is_symlink() or not app.is_dir():
        raise ValueError("Expected one regular CSSwitch.app bundle")
    contents = app / "Contents"
    info = plistlib.loads((contents / "Info.plist").read_bytes())
    expected = {
        "CFBundleIdentifier": config["identifier"],
        "CFBundleShortVersionString": config["version"],
        "LSMinimumSystemVersion": "13.0",
    }
    for key, value in expected.items():
        if info.get(key) != value:
            raise ValueError(f"Bundle {key} must be {value!r}")
    if info.get("CFBundleExecutable") != "desktop":
        raise ValueError("Unexpected Desktop executable")
    binaries = {}
    for name in ("desktop", "csswitch-gateway"):
        binary = contents / "MacOS" / name
        if binary.is_symlink() or not binary.is_file():
            raise ValueError(f"Missing regular executable: {name}")
        if binary.stat().st_mode & 0o111 != 0o111:
            raise ValueError(f"Executable permissions required for all users: {name}")
        arches = run("lipo", "-archs", str(binary)).decode().split()
        if arches != ["x86_64"]:
            raise ValueError(f"{name} must be Intel x86_64; found {arches}")
        binaries[name] = {"architectures": arches, "sha256": digest(binary)}
    resources = contents / "Resources"
    if (resources / "proxy").exists():
        raise ValueError("Unexpected legacy proxy runtime")
    for source, destination in config["bundle"]["resources"].items():
        original = root / "desktop/src-tauri" / source
        bundled = resources / destination
        if bundled.is_symlink() or not bundled.is_file() or digest(bundled) != digest(original):
            raise ValueError(f"Bundled resource does not match source: {destination}")
    run("codesign", "--verify", "--deep", "--strict", str(app))
    signing_details = run(
        "codesign", "--display", "--verbose=4", str(app), read_stderr=True
    ).decode().splitlines()
    if "Signature=adhoc" in signing_details:
        signature_type = "ad-hoc"
    elif any(line.startswith("Authority=") for line in signing_details):
        signature_type = "certificate"
    else:
        raise ValueError("Cannot establish the actual bundle signature type")
    return {
        "version": config["version"],
        "target": "x86_64-apple-darwin",
        "minimum_macos": "13.0",
        "binaries": binaries,
        "resources_match_source": True,
        "signature_integrity": "verified",
        "signature_type": signature_type,
        "notarization": "not-established",
        "science_runtime": "not-tested",
        "live_provider": "not-tested",
    }


def verify_image_root(mount, root=ROOT):
    metadata = {".DS_Store", ".VolumeIcon.icns"}
    names = {entry.name for entry in mount.iterdir()} - metadata
    if names != {"CSSwitch.app", "Applications"}:
        raise ValueError(f"Unexpected DMG root payload: {sorted(names)}")
    applications = mount / "Applications"
    if not applications.is_symlink() or str(applications.readlink()) != "/Applications":
        raise ValueError("DMG Applications link must point to /Applications")
    return verify_app(mount / "CSSwitch.app", root)


def verify_dmg(dmg):
    with tempfile.TemporaryDirectory(prefix="csswitch-intel-verify-") as temporary:
        mount = Path(temporary) / "mount"
        mount.mkdir()
        run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", str(mount), str(dmg))
        try:
            report = verify_image_root(mount)
        finally:
            run("hdiutil", "detach", str(mount))
    report["dmg_sha256"] = digest(dmg)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--app", type=Path)
    source.add_argument("--dmg", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    report = verify_app(args.app) if args.app else verify_dmg(args.dmg)
    output = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.report:
        args.report.write_text(output, encoding="utf-8")
    print(output, end="")


if __name__ == "__main__":
    main()
