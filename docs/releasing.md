# Releasing

Releases are cut by pushing a `v*` tag. `.github/workflows/release.yml` builds the binaries, publishes a GitHub Release and updates the Homebrew formula in [mihaicrisan04/homebrew-tap](https://github.com/mihaicrisan04/homebrew-tap).

## One-time setup

1. Create `github.com/mihaicrisan04/plugbay` and push the repo.
2. Create a fine-grained personal access token with access to `mihaicrisan04/homebrew-tap` only, permission **Contents: read and write**.
3. Add it to the plugbay repo as the Actions secret `HOMEBREW_TAP_TOKEN` (Settings > Secrets and variables > Actions, or `gh secret set HOMEBREW_TAP_TOKEN -R mihaicrisan04/plugbay`).
4. Optionally enable private vulnerability reporting (Settings > Security), which `SECURITY.md` points to.

The release itself uses the built-in `GITHUB_TOKEN`; only the tap update needs the extra secret.

## Cutting a release

1. Make sure `main` is green in CI and `README.md` exists (it ships in every tarball; the build fails without it).
2. Bump `version` in `Cargo.toml`, then run `cargo build` so `Cargo.lock` picks it up.
3. Commit (`chore: release v0.2.0`), then tag and push:

   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```

4. Watch the run with `gh run watch -R mihaicrisan04/plugbay`.

A tag with a hyphen (`v0.2.0-rc.1`) is published as a GitHub prerelease and does not touch the tap.

## What the workflow does

1. **check-version** fails the run if the tag does not match the `Cargo.toml` version.
2. **build** compiles `--release --locked` for each target and packs `plugbay-v{version}-{target}.tar.gz` (containing `plugbay`, `README.md`, `LICENSE`) plus a `.sha256` file:

   | Target | Runner |
   | --- | --- |
   | `aarch64-apple-darwin` | `macos-latest` |
   | `x86_64-apple-darwin` | `macos-latest` (cross-compiled) |
   | `x86_64-unknown-linux-gnu` | `ubuntu-22.04` |
   | `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` |

   Linux builds run on the oldest Ubuntu runners so the binaries work with glibc 2.35 and newer.
3. **release** creates the GitHub Release with the tarballs, the `.sha256` files, a combined `SHA256SUMS` and generated notes.
4. **homebrew-tap** renders `packaging/homebrew/plugbay.rb` with `packaging/homebrew/render.sh` and pushes it to `Formula/plugbay.rb` in the tap as `plugbay: update to v{version}`.

If the tap step fails (missing or expired token), fix the secret and re-run only that job from the Actions page.

## Testing the formula locally

`packaging/homebrew/plugbay.rb` is a template with `@VERSION@` and `@SHA256_...@` placeholders, so render it first. To test before tagging, package a local build and point the formula at it with a `file://` URL:

```sh
version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
dist=/tmp/plugbay-dist && rm -rf "$dist" && mkdir -p "$dist/stage"
cargo build --release --locked
cp target/release/plugbay README.md LICENSE "$dist/stage/"
tar -czf "$dist/plugbay-v$version-aarch64-apple-darwin.tar.gz" -C "$dist/stage" .
(cd "$dist" && shasum -a 256 *.tar.gz > SHA256SUMS)
# render.sh needs a line per target; fake the ones you did not build
for t in x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  printf '%064d  plugbay-v%s-%s.tar.gz\n' 0 "$version" "$t" >> "$dist/SHA256SUMS"
done
packaging/homebrew/render.sh "$version" "$dist/SHA256SUMS" "file://$dist" > "$dist/plugbay.rb"
```

Homebrew only installs formulae from a tap, so use a throwaway local one:

```sh
brew tap-new mihaicrisan04/plugbaytest --no-git
cp "$dist/plugbay.rb" "$(brew --repository)/Library/Taps/mihaicrisan04/homebrew-plugbaytest/Formula/"
brew style mihaicrisan04/plugbaytest/plugbay
brew audit --strict --formula mihaicrisan04/plugbaytest/plugbay
brew install mihaicrisan04/plugbaytest/plugbay
brew test mihaicrisan04/plugbaytest/plugbay
brew uninstall plugbay && brew untap mihaicrisan04/plugbaytest
```

After a real release, render against the published checksums instead (`gh release download v0.2.0 -p SHA256SUMS`, no base URL) and install from the real tap: `brew install mihaicrisan04/tap/plugbay`.

## Platform notes

### macOS keychain prompts after upgrades

The release binaries are not signed with a Developer ID (the linker only ad-hoc signs them). macOS ties "Always Allow" keychain access to the code signature, and an ad-hoc signature changes with every build, so after `brew upgrade` macOS asks again whether plugbay may read its keychain.

plugbay keeps every secret it stores in one keychain item (`plugbay-vault`, see `src/vault.rs`), so that is **one** prompt per upgrade, not one per login. Users upgrading from a build that stored one item per login get one prompt per old item, once, as they are copied into the vault.

**Before launch:** sign with a Developer ID Application certificate in the release workflow (`codesign --options runtime --timestamp`) and notarize with `xcrun notarytool`. The identity then stays stable across upgrades and the prompt disappears. Needs an Apple Developer account, plus the certificate and an App Store Connect API key (or app-specific password) as Actions secrets.

### Linux

Logins and tokens are stored through the Secret Service API (pure Rust over D-Bus, no system libraries needed at build or install time). At runtime a keyring daemon such as GNOME Keyring or KWallet must be running in the session; headless machines usually have none. The formula caveats say this on Linux.
