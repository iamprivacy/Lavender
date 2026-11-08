# In-app update, end to end (macOS)

`run.sh` checks the whole update path on a real app bundle before a release:

1. builds release 1 and release 2 of Lavender as signed debug bundles
   (`build.sh`), with their own app identifier so a real install's settings
   and library are never touched;
2. builds fake GitHub releases from release 2 with the same tools as
   `release.yml` (`prepare.py`: `release_manifest.py` and `tauri signer sign`),
   including tampered ones;
3. serves them on `127.0.0.1:8765` (`server.py`), which the test bundles ask
   instead of GitHub (`LAVENDER_UPDATE_TEST_SERVER`, compile time only);
4. drives the app over WebDriver (`wd.py`, debug builds serve it on `:4445`):
   install, restart into release 2, then the refusals (tampered manifest,
   wrong package hash, manifest of another release, no manifest, an older
   release, no release at all).

```bash
tools/update-e2e/run.sh
```

It needs the private release key (`lavender-release.key` next to the main
checkout by default, or `KEY=...`). Work files go to `/private/tmp/lavender-update-e2e`
(`WORK=...`; the script resolves symlinks, which Tauri refuses to run from). The two builds take a few minutes the first time.
