# Releasing peemux

## One-time setup

### 1. Homebrew tap token

The release CI pushes formula updates to `arcnid/homebrew-peemux`. It needs a
GitHub PAT stored as a repo secret.

1. GitHub → Settings → Developer settings → **Fine-grained personal access tokens**
2. Create a token scoped to **`arcnid/homebrew-peemux` only**, with **Contents: Read and write**
3. Set it as a secret on the main repo:
   ```bash
   gh secret set TAP_TOKEN -R arcnid/peemux
   # paste the token when prompted
   ```

### 2. crates.io login

```bash
# Get an API token from https://crates.io/settings/tokens
cargo login
```

This is stored locally in `~/.cargo/credentials.toml`. Only needed once per machine.

## Cutting a release

### Tag and push

```bash
# Bump version in Cargo.toml first, then:
git add Cargo.toml Cargo.lock
git commit -m "v0.1.0"
git tag v0.1.0
git push origin main v0.1.0
```

### What CI does

The `.github/workflows/release.yml` workflow triggers on any `v*` tag:

1. **Build** — three parallel jobs:
   - `aarch64-apple-darwin` (macOS Apple Silicon)
   - `x86_64-apple-darwin` (macOS Intel)
   - `x86_64-unknown-linux-gnu` (Linux)
   
   Each produces a `.tar.gz` + `.sha256` file.

2. **Release** — creates a GitHub Release with auto-generated notes and
   uploads all six artifacts.

3. **Update tap** — clones `arcnid/homebrew-peemux`, rewrites
   `Formula/peemux.rb` with the new version and SHA256 hashes, commits, and
   pushes. Requires the `TAP_TOKEN` secret.

### Publish to crates.io (manual)

```bash
cargo publish
```

Run after the tag push. Not automated in CI to avoid accidental publishes.

## Install paths

| Method | Command | Needs Rust? |
|---|---|---|
| Homebrew | `brew tap arcnid/peemux && brew install peemux` | No |
| crates.io | `cargo install peemux` | Yes |
| Source | `git clone ... && cargo install --path .` | Yes |

## Repos

| Repo | Purpose |
|---|---|
| `arcnid/peemux` | Source, CI, releases |
| `arcnid/homebrew-peemux` | Homebrew tap (auto-updated by CI) |
