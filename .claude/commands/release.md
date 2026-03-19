Release a new version of toki via local cross-compilation and Homebrew tap update.

## Instructions

When the user runs `/release`, follow these steps:

### 1. Determine version

- Ask the user for the version to release (e.g., `0.2.0`)
- Or if the user already provided it as an argument (e.g., `/release 0.2.0`), use that
- Validate it follows semver format (MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-prerelease)

### 2. Pre-flight checks

- Run `cargo test` to make sure tests pass
- Check `git status` to ensure the working tree is clean (no uncommitted changes)
- Check that the tag `v{VERSION}` doesn't already exist: `git tag -l v{VERSION}`
- Verify `cross` and `docker` are available

If any check fails, report the issue and stop.

### 3. Update version

- Update `version` in `Cargo.toml` to the new version
- Run `cargo check` to update `Cargo.lock`
- Commit with message: `chore: bump version to {VERSION}`

### 4. Build all targets

Build all 4 targets using `cargo build` for native macOS and `cross build` for Linux:

```bash
# macOS (native, no Docker needed)
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

# Linux (via cross, uses Docker)
cross build --release --target x86_64-unknown-linux-gnu
cross build --release --target aarch64-unknown-linux-gnu
```

Then package each into tar.gz:

```bash
for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  tar -czf "toki-{VERSION}-${target}.tar.gz" -C "target/${target}/release" toki
done
```

### 5. Create tag, push, and upload release

- Create an annotated tag: `git tag -a v{VERSION} -m "Release v{VERSION}"`
- Show the user what will be pushed and ask for confirmation
- Push the commit and tag: `git push origin main && git push origin v{VERSION}`
- Create GitHub release with the archives:
  ```bash
  gh release create v{VERSION} toki-{VERSION}-*.tar.gz --title "v{VERSION}" --generate-notes
  ```

### 6. Update Homebrew tap

Compute sha256 for each archive and update the tap formula:

```bash
# Compute sha256
SHA_AARCH64_DARWIN=$(shasum -a 256 toki-{VERSION}-aarch64-apple-darwin.tar.gz | cut -d' ' -f1)
SHA_X86_64_DARWIN=$(shasum -a 256 toki-{VERSION}-x86_64-apple-darwin.tar.gz | cut -d' ' -f1)
SHA_AARCH64_LINUX=$(shasum -a 256 toki-{VERSION}-aarch64-unknown-linux-gnu.tar.gz | cut -d' ' -f1)
SHA_X86_64_LINUX=$(shasum -a 256 toki-{VERSION}-x86_64-unknown-linux-gnu.tar.gz | cut -d' ' -f1)
```

Then clone the tap repo, update `Formula/toki.rb` with the new version and sha256 values, commit, and push:

```bash
cd /tmp && rm -rf homebrew-tap && git clone https://github.com/korjwl1/homebrew-tap.git && cd homebrew-tap
```

Write the updated formula with correct version, URLs, and sha256 hashes, then:

```bash
git add Formula/toki.rb && git commit -m "Update toki to {VERSION}" && git push
```

### 7. Cleanup and confirm

- Remove the local tar.gz files
- Tell the user the release is complete
- Remind that users can install via:
  ```
  brew tap korjwl1/tap
  brew install toki
  ```
