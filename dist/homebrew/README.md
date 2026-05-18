# Homebrew distribution

## Recommended layout: a separate tap

For Homebrew, the canonical layout is a separate repo named
`homebrew-usbipd-darwin` under the same GitHub org as this project,
containing a single `Formula/usbipd-darwin.rb` file. Users install with:

```sh
brew tap lima-vm/usbipd-darwin
brew install usbipd-darwin
```

That tap should be created once (empty repo, MIT-licensed, README pointing
back here). The formula file lives there, not in this repo, so that
Homebrew's `brew bump-formula-pr` automation keeps working.

The file at [usbipd-darwin.rb](usbipd-darwin.rb) in this directory is the
**reference formula**: it is what should be copied into the tap repo for
the first release, and it tracks the canonical shape going forward.

## Release flow

For each `vX.Y.Z` tag:

1. The `release` GitHub Actions workflow uploads two tarballs to the
   GitHub Release:
   - `usbipd-darwin-X.Y.Z-aarch64-apple-darwin.tar.gz`
   - `usbipd-darwin-X.Y.Z-x86_64-apple-darwin.tar.gz`
   plus a `.sha256` for each.
2. Update the formula in the tap repo:
   - `version`
   - `url`s (both arches)
   - `sha256`s (both arches, copied from the `.sha256` files)
3. `brew install --build-from-source ./Formula/usbipd-darwin.rb` locally
   to smoke-test, then `git commit -m "usbipd-darwin X.Y.Z"` and push.

A future improvement: a `homebrew-bump-formula-pr` step in the release
workflow that opens a PR against the tap automatically. Skipped for the
first release because the tap doesn't exist yet.

## Caveats

- **Sudo requirement.** Homebrew normally installs into a user-writable
  prefix; the daemon still needs `sudo` to run. The formula prints a
  caveat pointing the user at `usbipd sudoers` for a NOPASSWD rule.
- **No launchd plist by default.** The formula installs the sample plist
  under `#{share}/usbipd-darwin/launchd/` rather than wiring up
  `brew services` automatically — running a privileged daemon should be
  an explicit user choice.
- **Notarization.** Bottles built by the Homebrew CI farm would lose the
  Apple notarization ticket (different binary). Until/unless we ship a
  proper bottle, users get the notarized binary from the GitHub Release
  via `url`+`sha256` source install (`brew install` will download and
  verify the tarball directly).
