Release a new version of toki and trigger the Homebrew release pipeline.

## Instructions

When the user runs `/release`, follow these steps:

### 1. Determine version

- Ask the user for the version to release (e.g., `0.2.0`)
- Or if the user already provided it as an argument (e.g., `/release 0.2.0`), use that
- Validate it follows semver format (MAJOR.MINOR.PATCH)

### 2. Pre-flight checks

- Run `cargo build --release` to make sure the code compiles
- Run `cargo test` to make sure tests pass
- Check `git status` to ensure the working tree is clean (no uncommitted changes)
- Check that the tag `v{VERSION}` doesn't already exist: `git tag -l v{VERSION}`

If any check fails, report the issue and stop.

### 3. Update version

- Update `version` in `Cargo.toml` to the new version
- Run `cargo check` to update `Cargo.lock`
- Commit with message: `chore: bump version to {VERSION}`

### 4. Create tag and push

- Create an annotated tag: `git tag -a v{VERSION} -m "Release v{VERSION}"`
- Show the user what will be pushed and ask for confirmation
- Push the commit and tag: `git push origin main && git push origin v{VERSION}`

### 5. Confirm

- Tell the user the release pipeline has been triggered
- Provide the GitHub Actions URL: `https://github.com/korjwl1/toki/actions`
- Remind that after the workflow completes, users can install via:
  ```
  brew tap korjwl1/tap
  brew install toki
  ```
