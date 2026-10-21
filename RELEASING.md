# Releasing lince

Releases are built by `.github/workflows/release.yml` from a pushed `vX.Y.Z` tag. The workflow runs the tests, builds `target/release/lince` on Ubuntu 22.04 (an older glibc, so the binary runs on any current distribution) and publishes the bare binary, a tarball and the SHA-256 of each. Never publish a binary built anywhere else.

## Steps

1. **Start from an up-to-date `main`** whose changes have been merged and pass CI.

2. **Bump the version** in the four files the previous release touched, and nothing else:
   - `Cargo.toml` (`version`), then `cargo build --release` to update `Cargo.lock`;
   - `PROTOCOL.md` (the `"version"` in the status example);
   - `README.md` (the two `lince-vX.Y.Z-x86_64-linux` names in the install snippet).

   Commit as `release: lince X.Y.Z`.

3. **Run the CI checks locally**, as `.github/workflows/ci.yml` does:

   ```sh
   cargo clippy --release --locked -- -D warnings
   cargo test --release --locked
   cargo build --release --locked
   bash tests/cli_test.sh target/release/lince
   python3 tests/stress_updates.py target/release/lince 2000
   ```

4. **Push `main` and wait for CI on that exact commit.** Do not tag until it passes. A failure on the release commit alone usually means a test depends on the machine (file system order, timing); fix the test in its own commit, push, and wait again.

5. **Tag the commit that passed** and push the tag:

   ```sh
   git tag -a vX.Y.Z -m "lince X.Y.Z" <commit>
   git push origin vX.Y.Z
   ```

6. **Check the published release.** Download the bare binary and verify it independently before anyone pins it:

   ```sh
   gh release download vX.Y.Z --pattern 'lince-vX.Y.Z-x86_64-linux*'
   sha256sum -c lince-vX.Y.Z-x86_64-linux.sha256
   gh api repos/jfg96/lince/releases/tags/vX.Y.Z --jq '.assets[] | select(.name=="lince-vX.Y.Z-x86_64-linux") | "\(.size) \(.digest)"'
   chmod +x lince-vX.Y.Z-x86_64-linux && ./lince-vX.Y.Z-x86_64-linux --version
   ```

7. **Write the release notes.** The workflow publishes the release without text. Add a one-line summary, the user-visible changes, measurements if the release is about performance (with how they were taken), whether the protocol and snapshot formats changed, and a `Validation:` line. Edit them with `gh release edit vX.Y.Z --notes-file notes.md`.

## Omarchy Find

[Omarchy Find](https://github.com/jfg96/omarchy-find) runs the release pinned in its `lince-release.env`, verified by size and SHA-256. After a lince release, open a pull request there that updates all five fields from the values checked in step 6 (`LINCE_SOURCE_SHA` is the commit the tag points to) and bumps the plugin version in `manifest.json`, then tag and publish that release with notes.

To try a release on one machine before the pin changes, put the verified binary at `bin/lince` inside the installed plugin (ignored by Git; it takes precedence over the pin) and stop the running daemon; Omarchy Find relaunches it. Remove that file once the plugin carries the new pin, or it keeps overriding later pins.
