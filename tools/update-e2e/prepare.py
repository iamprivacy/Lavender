#!/usr/bin/env python3
"""Builds the fake releases the local server hands to the test app, the same
way release.yml does (release_manifest.py, then `tauri signer sign`).

Reads $WORK/new/Lavender.app.tar.gz(.sig) and writes $WORK/releases/.
Environment: WORK, REPO (repo root), KEY (private release key file).
"""

import json
import os
import shutil
import subprocess

WORK = os.environ["WORK"]
REPO = os.environ["REPO"]
KEY = os.environ["KEY"]
# The app version in src-tauri/Cargo.toml (run.sh reads it); release N is
# VERSION-local.N.
VERSION = os.environ["VERSION"]
SCRIPT = os.path.join(REPO, ".github/scripts/release_manifest.py")
SERVER = "http://127.0.0.1:8765"
PKG = "Lavender_aarch64.app.tar.gz"


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def sign(path):
    env = {k: v for k, v in os.environ.items() if not k.startswith("TAURI_SIGNING")}
    subprocess.run(
        ["npx", "tauri", "signer", "sign", "-f", KEY, "-p", "", path],
        check=True, capture_output=True, text=True, env=env, cwd=REPO,
    )


def build(name, tag, manifest_tag=None, tamper=None, manifest=True):
    d = os.path.join(WORK, "releases", name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    shutil.copy(os.path.join(WORK, "new", "Lavender.app.tar.gz"), os.path.join(d, PKG))
    shutil.copy(os.path.join(WORK, "new", "Lavender.app.tar.gz.sig"), os.path.join(d, PKG + ".sig"))
    with open(os.path.join(d, f"Lavender_{VERSION}_aarch64.dmg"), "w") as f:
        f.write("not a real dmg\n")
    if manifest:
        # latest.json first, so the sums cover it as release.yml's do.
        latest = run("python3", SCRIPT, "updater-json", d, tag, "iamprivacy/Lavender", VERSION)
        latest = latest.replace(
            f"https://github.com/iamprivacy/Lavender/releases/download/{tag}/",
            f"{SERVER}/dl/{name}/",
        )
        with open(os.path.join(d, "latest.json"), "w") as f:
            f.write(latest)
        sums = run("python3", SCRIPT, "sums", d, manifest_tag or tag)
        if tamper == "badhash":
            sums = "".join(
                ("0" * 64 + "  " + PKG + "\n") if line.endswith("  " + PKG) else line + "\n"
                for line in sums.splitlines()
            )
        with open(os.path.join(d, "SHA256SUMS"), "w") as f:
            f.write(sums)
        sign(os.path.join(d, "SHA256SUMS"))
        if tamper == "badsig":
            with open(os.path.join(d, "SHA256SUMS"), "a") as f:
                f.write("0" * 64 + "  extra-file.zip\n")
    api = {
        "tag_name": tag,
        "html_url": f"https://github.com/iamprivacy/Lavender/releases/tag/{tag}",
        "assets": [
            {"name": n, "browser_download_url": f"{SERVER}/dl/{name}/{n}"}
            for n in sorted(os.listdir(d))
        ],
    }
    with open(os.path.join(WORK, "releases", name + ".api.json"), "w") as f:
        json.dump(api, f, indent=2)


build("normal", f"v{VERSION}-local.2")
build("badsig", f"v{VERSION}-local.2", tamper="badsig")
build("badhash", f"v{VERSION}-local.2", tamper="badhash")
build("mismatch", f"v{VERSION}-local.3", manifest_tag=f"v{VERSION}-local.2")
build("older", f"v{VERSION}-local.1")
build("nomanifest", f"v{VERSION}-local.2", manifest=False)
print("releases ready")
