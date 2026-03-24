Release a new version of toki. macOS binaries are built locally, Linux binaries are built via GitHub Actions.

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
- Create GitHub release on **toki repo** with macOS archives:
  ```bash
  gh release create v{VERSION} toki-{VERSION}-*-apple-darwin.tar.gz --repo korjwl1/toki --title "v{VERSION}" --generate-notes
  ```

### 6. Wait for GitHub Actions

- Check the Actions run: `gh run list --repo korjwl1/toki --limit 3`
- Wait for completion: `gh run watch --repo korjwl1/toki`
- Once done, Linux archives will be uploaded to the toki release automatically

### 7. Update Homebrew tap

After both macOS (local) and Linux (Actions) archives are uploaded, compute sha256 and update the tap formula.

Download Linux archives from the toki release:

```bash
gh release download v{VERSION} --repo korjwl1/toki --pattern "toki-{VERSION}-*-linux-*"
```

Compute sha256 for all 4 archives:

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

Formula URLs point to `korjwl1/toki` releases (public repo).

The formula template:

```ruby
class Toki < Formula
  desc "AI CLI tool token usage tracker"
  homepage "https://github.com/korjwl1/toki"
  version "{VERSION}"
  license "FSL-1.1-Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/korjwl1/toki/releases/download/v{VERSION}/toki-{VERSION}-aarch64-apple-darwin.tar.gz"
      sha256 "{SHA_AARCH64_DARWIN}"
    end
    on_intel do
      url "https://github.com/korjwl1/toki/releases/download/v{VERSION}/toki-{VERSION}-x86_64-apple-darwin.tar.gz"
      sha256 "{SHA_X86_64_DARWIN}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/korjwl1/toki/releases/download/v{VERSION}/toki-{VERSION}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "{SHA_AARCH64_LINUX}"
    end
    on_intel do
      url "https://github.com/korjwl1/toki/releases/download/v{VERSION}/toki-{VERSION}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{SHA_X86_64_LINUX}"
    end
  end

  def install
    bin.install "toki"
  end

  def post_install
    # Restart daemon if it was running (picks up new binary)
    pidfile = File.expand_path("~/.config/toki/daemon.pid")
    if File.exist?(pidfile)
      pid = File.read(pidfile).strip.to_i
      if pid > 0
        begin
          Process.kill(0, pid) # check if alive
          ohai "Restarting toki daemon to use the new version..."
          system bin/"toki", "daemon", "restart"
        rescue Errno::ESRCH
          # daemon not running, stale pidfile — nothing to do
        end
      end
    end
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
