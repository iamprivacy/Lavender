# Lavender for Homebrew

The Homebrew tap for [Lavender](https://github.com/iamprivacy/Lavender), a
synced lyrics downloader for your music library that can work from a local copy
of the LRCLIB database.

## Install

```shell
brew tap iamprivacy/lavender
brew install --cask iamprivacy/lavender/lavender
```

Naming the cask in full tells Homebrew to trust this tap; with only
`lavender`, Homebrew refuses a tap it does not trust yet.

The cask installs the Apple Silicon or Intel build to match your Mac.

Lavender isn't code signed yet, so the first time you open it macOS may block
it: click **Open Anyway** in System Settings > Privacy & Security.

## Update

```shell
brew upgrade --cask lavender
```

Lavender can also update itself from **About**. After such an update Homebrew
still lists the version it installed, so the next `brew upgrade` downloads the
new version once more; that is harmless.

## Uninstall

```shell
brew uninstall --cask lavender
```

Add `--zap` to also remove your Lavender library and settings.

This tap is updated automatically when a Lavender release is published; please
report problems in the [Lavender repo](https://github.com/iamprivacy/Lavender/issues).
