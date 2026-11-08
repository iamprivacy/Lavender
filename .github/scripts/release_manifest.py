#!/usr/bin/env python3
"""Release manifest files for Lavender's in-app update (see release.yml).

  sums DIR TAG                         print SHA256SUMS for every file in DIR
  updater-json DIR TAG REPO VERSION    print latest.json for the Tauri updater
  notes BODY_FILE SUMS_FILE PUBKEY     print the release notes with the Verify
                                       footer (PUBKEY may be empty)
  changelog CHANGELOG_FILE TAG         print the release body for TAG: TAG's
                                       section of the changelog, which opens
                                       with the star-and-donate banner (added
                                       here when a section lacks it; nothing
                                       when the changelog has no such section)

SHA256SUMS starts with "# lavender-version: TAG", then one coreutils line
("<hex>  <name>") per asset, sorted by name. The app trusts that version line
only after the minisign signature in SHA256SUMS.sig matches.

latest.json announces VERSION, the app version the packages were built with
(e.g. 0.1.0), in the shape the Tauri updater reads. Which release is newer is
decided by the app from the signed SHA256SUMS, not from this file. latest.json
is written into the asset folder before SHA256SUMS, so it is hashed into the
signed manifest too: the app refuses a release whose latest.json is missing
from the manifest or differs from it, and checks that the package the updater
picked is the one this signed copy names for the running system.
"""

import base64
import datetime
import hashlib
import json
import os
import re
import sys

VERSION_PREFIX = "# lavender-version: "
VERIFY_HEADING = "### 🔐 Verify"

# Every release body opens with this banner. Each CHANGELOG.md section carries
# a copy right under its heading (src/changelog-format.test.js checks it
# against this block); a section without one gets it here, so it cannot be
# forgotten when the notes are written.
BANNER = """<p align="center">
  <strong>💜 If you enjoy Lavender, a star on this repo goes a long way, and if you'd like to help me afford to keep developing it, consider donating.</strong>
</p>

<p align="center">
  <a href="https://www.buymeacoffee.com/iamprivacy"><img src="https://img.shields.io/badge/-Buy%20me%20a%20coffee-ffdd00?style=flat&logo=buy-me-a-coffee&logoColor=black" alt="Buy Me a Coffee"></a>
</p>
"""
# The two files that cannot be in SHA256SUMS: itself and its signature.
# latest.json is hashed like any other asset (see the module doc).
MANIFEST_FILES = {"SHA256SUMS", "SHA256SUMS.sig"}

# Updater targets ("{os}-{arch}[-{installer}]") and the asset that serves each.
# The bare "{os}-{arch}" entry is the fallback for builds that do not know
# their bundle type.
TARGETS = [
    ("darwin-aarch64", r"(aarch64|arm64).*\.app\.tar\.gz$"),
    ("darwin-aarch64-app", r"(aarch64|arm64).*\.app\.tar\.gz$"),
    ("darwin-x86_64", r"(x64|x86_64).*\.app\.tar\.gz$"),
    ("darwin-x86_64-app", r"(x64|x86_64).*\.app\.tar\.gz$"),
    ("linux-x86_64", r"_amd64\.AppImage$"),
    ("linux-x86_64-appimage", r"_amd64\.AppImage$"),
    ("linux-x86_64-deb", r"_amd64\.deb$"),
    ("linux-x86_64-rpm", r"\.x86_64\.rpm$"),
    ("windows-x86_64", r"_x64-setup\.exe$"),
    ("windows-x86_64-nsis", r"_x64-setup\.exe$"),
    ("windows-x86_64-msi", r"_x64_[A-Za-z-]+\.msi$"),
]


def asset_names(directory):
    return sorted(
        name
        for name in os.listdir(directory)
        if os.path.isfile(os.path.join(directory, name)) and name not in MANIFEST_FILES
    )


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cmd_sums(directory, tag):
    lines = [VERSION_PREFIX + tag]
    for name in asset_names(directory):
        lines.append(f"{sha256(os.path.join(directory, name))}  {name}")
    sys.stdout.write("\n".join(lines) + "\n")


def cmd_updater_json(directory, tag, repo, version):
    names = asset_names(directory)
    platforms = {}
    for target, pattern in TARGETS:
        matches = [n for n in names if re.search(pattern, n) and not n.endswith(".sig")]
        if len(matches) != 1:
            print(f"warning: {target}: expected one package, found {matches}", file=sys.stderr)
            continue
        package = matches[0]
        sig_path = os.path.join(directory, package + ".sig")
        if not os.path.isfile(sig_path):
            print(f"warning: {target}: {package} has no .sig file", file=sys.stderr)
            continue
        with open(sig_path, encoding="utf-8") as handle:
            signature = handle.read().strip()
        platforms[target] = {
            "url": f"https://github.com/{repo}/releases/download/{tag}/{package}",
            "signature": signature,
        }
    if not platforms:
        sys.exit("error: no signed updater package found")
    now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
    manifest = {
        "version": version,
        "notes": f"Lavender {tag}",
        "pub_date": now.isoformat().replace("+00:00", "Z"),
        "platforms": platforms,
    }
    json.dump(manifest, sys.stdout, indent=2)
    sys.stdout.write("\n")


def minisign_key_line(pubkey_b64):
    """The bare key line that `minisign -P` takes, from Tauri's base64 key."""
    text = base64.b64decode(pubkey_b64).decode("utf-8")
    return text.strip().splitlines()[-1].strip()


def cmd_notes(body_file, sums_file, pubkey_b64):
    with open(body_file, encoding="utf-8") as handle:
        body = handle.read()
    # A re-run replaces the footer instead of adding a second one.
    body = body.split(VERIFY_HEADING, 1)[0].rstrip()
    with open(sums_file, encoding="utf-8") as handle:
        rows = [
            line.split(None, 1)
            for line in handle.read().splitlines()
            if line.strip() and not line.startswith("#")
        ]
    out = [body, "", VERIFY_HEADING, "", "| Asset | SHA-256 |", "| --- | --- |"]
    out += [f"| `{name.strip()}` | `{digest}` |" for digest, name in rows]
    out.append("")
    if pubkey_b64.strip():
        key = minisign_key_line(pubkey_b64.strip())
        out += [
            "`SHA256SUMS` lists these hashes, and `SHA256SUMS.sig` is a minisign signature "
            "over it, made with the Lavender release key. The app installs an update only "
            "after this signature and the hash of the download both match. The public key, "
            "also compiled into every build:",
            "",
            "```",
            key,
            "```",
            "",
            "To check a download yourself (`base64 -D` on older macOS):",
            "",
            "```bash",
            "base64 -d SHA256SUMS.sig > SHA256SUMS.minisig",
            f"minisign -Vm SHA256SUMS -x SHA256SUMS.minisig -P {key}",
            "shasum -a 256 --ignore-missing -c SHA256SUMS",
            "```",
        ]
    else:
        out += [
            "`SHA256SUMS` lists these hashes. To check a download:",
            "",
            "```bash",
            "shasum -a 256 --ignore-missing -c SHA256SUMS",
            "```",
        ]
    sys.stdout.write("\n".join(out) + "\n")


def changelog_section(text, tag):
    """TAG's section of a changelog: the lines under the "## " heading that
    carries TAG as a word (an emoji accent may precede it), up to the next
    "## " heading, without leading or trailing blank lines. "" when absent."""
    lines = []
    on = False
    for line in text.splitlines():
        if line.startswith("## "):
            on = tag in line[3:].split()
            continue
        if on:
            lines.append(line)
    return "\n".join(lines).strip()


def cmd_changelog(changelog_file, tag):
    with open(changelog_file, encoding="utf-8") as handle:
        section = changelog_section(handle.read(), tag)
    if section and not section.startswith(BANNER.strip()):
        section = BANNER + "\n" + section
    if section:
        sys.stdout.write(section + "\n")


def main(argv):
    if len(argv) >= 2 and argv[1] == "changelog" and len(argv) == 4:
        cmd_changelog(argv[2], argv[3])
    elif len(argv) >= 2 and argv[1] == "sums" and len(argv) == 4:
        cmd_sums(argv[2], argv[3])
    elif len(argv) >= 2 and argv[1] == "updater-json" and len(argv) == 6:
        cmd_updater_json(argv[2], argv[3], argv[4], argv[5])
    elif len(argv) >= 2 and argv[1] == "notes" and len(argv) == 5:
        cmd_notes(argv[2], argv[3], argv[4])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
