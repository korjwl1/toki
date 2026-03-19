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

### 5. Create tag, push, and upload releases

- Create an annotated tag: `git tag -a v{VERSION} -m "Release v{VERSION}"`
- Show the user what will be pushed and ask for confirmation
- Push the commit and tag: `git push origin main && git push origin v{VERSION}`
- Create GitHub release on **toki repo** (for source tracking):
  ```bash
  gh release create v{VERSION} toki-{VERSION}-*.tar.gz --repo korjwl1/toki --title "v{VERSION}" --generate-notes
  ```
- Upload the same archives to **homebrew-tap repo** release (public, for brew download):
  ```bash
  gh release create v{VERSION} toki-{VERSION}-*.tar.gz --repo korjwl1/homebrew-tap --title "toki v{VERSION}" --notes "Release artifacts for toki v{VERSION}"
  ```

### 6. Update Homebrew tap

Compute sha256 for each archive and update the tap formula.

**IMPORTANT**: Formula URLs must point to `korjwl1/homebrew-tap` releases (public), NOT `korjwl1/toki` (private).

```bash
SHA_AARCH64_DARWIN=$(shasum -a 256 toki-{VERSION}-aarch64-apple-darwin.tar.gz | cut -d' ' -f1)
SHA_X86_64_DARWIN=$(shasum -a 256 toki-{VERSION}-x86_64-apple-darwin.tar.gz | cut -d' ' -f1)
SHA_AARCH64_LINUX=$(shasum -a 256 toki-{VERSION}-aarch64-unknown-linux-gnu.tar.gz | cut -d' ' -f1)
SHA_X86_64_LINUX=$(shasum -a 256 toki-{VERSION}-x86_64-unknown-linux-gnu.tar.gz | cut -d' ' -f1)
```

Clone tap repo and write the updated Formula/toki.rb:

```bash
cd /tmp && rm -rf homebrew-tap && git clone https://github.com/korjwl1/homebrew-tap.git && cd homebrew-tap
```

The formula template (URLs point to homebrew-tap releases):

```ruby
class Toki < Formula
  desc "AI CLI tool token usage tracker"
  homepage "https://github.com/korjwl1/toki"
  version "{VERSION}"
  license "FSL-1.1-Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/korjwl1/homebrew-tap/releases/download/v{VERSION}/toki-{VERSION}-aarch64-apple-darwin.tar.gz"
      sha256 "{SHA_AARCH64_DARWIN}"
    end
    on_intel do
      url "https://github.com/korjwl1/homebrew-tap/releases/download/v{VERSION}/toki-{VERSION}-x86_64-apple-darwin.tar.gz"
      sha256 "{SHA_X86_64_DARWIN}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/korjwl1/homebrew-tap/releases/download/v{VERSION}/toki-{VERSION}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "{SHA_AARCH64_LINUX}"
    end
    on_intel do
      url "https://github.com/korjwl1/homebrew-tap/releases/download/v{VERSION}/toki-{VERSION}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{SHA_X86_64_LINUX}"
    end
  end

  def install
    bin.install "toki"
  end

  test do
    system "#{bin}/toki", "--version"
  end
end
```

Commit and push:

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

### Notes

- When toki repo becomes public, update Formula URLs to point to `korjwl1/toki` releases instead, and stop uploading to homebrew-tap releases.
