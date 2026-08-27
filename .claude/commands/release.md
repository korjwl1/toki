Release a new version of toki. macOS binaries are built locally, Linux binaries are built via GitHub Actions.

## Instructions

When the user runs `/release`, follow these steps:

### 1. Determine version

- Ask the user for the version to release (e.g., `2.3.0`)
- Or if the user already provided it as an argument (e.g., `/release 2.3.0`), use that
- Validate it follows semver format (MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-prerelease)

### 2. Pre-flight checks

- Run `cargo test` to make sure tests pass
- Run `cargo clippy --all-targets --all-features -- -D warnings`. The current
  development branch has a warning baseline, so this strict release gate is
  expected to block until those warnings are resolved; do not mistake plain
  Clippy's exit 0 for a clean release check.
- Check `git status` to ensure the working tree is clean (no uncommitted changes)
- Check that the tag `v{VERSION}` doesn't already exist: `git tag -l v{VERSION}`
- Confirm `Cargo.toml` has no local `[patch."https://github.com/korjwl1/toki-sync-protocol.git"]` section. A tagged release/Actions checkout has no sibling `../toki_sync_protocol`, so releasing with the patch is guaranteed to fail.
- Confirm the protocol dependency is pinned to a published tag containing every wire type the client uses. For the current branch the required sequence is: tag `toki-sync-protocol` v1.1.0, repin toki and toki_sync to v1.1.0, remove both local patches, then release toki_sync before toki_monitor.

If any check fails, report the issue and stop.

### 3. Update version

- Update `version` in `Cargo.toml` to the new version
- Run `cargo check` to update `Cargo.lock`
- Commit with message: `chore: bump version to {VERSION}`

### 4. Build macOS targets (local)

```bash
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
```

Package into tar.gz:

```bash
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  tar -czf "toki-{VERSION}-${target}.tar.gz" -C "target/${target}/release" toki
done
```

### 5. Create tag, push, and upload

- Create an annotated tag: `git tag -a v{VERSION} -m "Release v{VERSION}"`
- Show the user what will be pushed and ask for confirmation
- Push the commit and tag: `git push origin main && git push origin v{VERSION}`
- Tag push triggers GitHub Actions workflow (`.github/workflows/release-linux.yml`) which builds Linux x86_64 and aarch64 with jemalloc, and uploads to the toki release
- The Linux workflow may create the GitHub release before the local command
  reaches it. Upload to an existing release, or create it if it does not exist:
  ```bash
  if gh release view v{VERSION} --repo korjwl1/toki >/dev/null 2>&1; then
    gh release upload v{VERSION} toki-{VERSION}-*-apple-darwin.tar.gz \
      --repo korjwl1/toki --clobber
  else
    gh release create v{VERSION} toki-{VERSION}-*-apple-darwin.tar.gz \
      --repo korjwl1/toki --title "v{VERSION}" --generate-notes
  fi
  ```

### 6. Wait for GitHub Actions

- Check the Actions run: `gh run list --repo korjwl1/toki --limit 3`
- Wait for completion: `gh run watch --repo korjwl1/toki`
- Once done, Linux archives will be uploaded to the toki release automatically

### 7. Verify the automated Homebrew tap PR

Publishing the GitHub release triggers `.github/workflows/bump-tap.yml`. It
waits for all four archives, computes their checksums, updates the MIT-licensed
formula, and opens a PR in `korjwl1/homebrew-tap` using `TAP_REPO_TOKEN`.

- Confirm the `Bump Homebrew formula` workflow succeeds.
- Review and merge the generated `bump-toki-{VERSION}` PR.
- If it fails, check that all four archives exist and that `TAP_REPO_TOKEN` has
  contents and pull-request write permission. Do not hand-publish a formula
  containing only a subset of platforms.

### 8. Cleanup and confirm

- Remove the local tar.gz files
- Tell the user the release is complete
- Remind that users can install via:
  ```
  brew tap korjwl1/tap
  brew install toki
  ```

### Notes

- Linux builds use jemalloc (required to prevent memory fragmentation on long-running daemons)
- macOS builds are done locally (10x cheaper than Actions macOS runners)
- Linux builds are done via GitHub Actions (native Linux runners, no cross-compilation issues with jemalloc)
- All release artifacts are hosted on `korjwl1/toki` releases (public repo)
