# Distributing the DoodleIQ provider agent

Single self-contained binary. `cloudflared` is **not** bundled — `doodleiq run`
downloads Cloudflare's official release into the config dir on first use
(Linux/Windows) or asks for `brew install cloudflared` (macOS).

No CI service. Releases are cut locally with `packaging/release-local.sh` and
hosted on Cloudflare R2.

## Artifacts

| File | Runs on | Size | Notes |
|---|---|---|---|
| `doodleiq-macos-universal.tar.gz` | macOS 11+, Intel & Apple Silicon | ~6.5 MB | `lipo` universal; signed + notarized (see `SIGNING.md`); needs `brew install cloudflared` |
| `doodleiq-linux-x86_64.tar.gz` | glibc Linux x86_64 | ~3.4 MB | needs `ca-certificates` |

TLS is rustls + platform verifier — no OpenSSL. (Windows and Linux arm64 are
supported by the code but not built by default; see below.)

## One-time setup

1. **Wrangler** — `npm i -g wrangler` (Cloudflare's CLI; you likely already use it
   for the tunnels/Workers), then `wrangler login`.
2. **R2 bucket**:

   ```sh
   wrangler r2 bucket create doodleiq-releases
   ```

   Then in the Cloudflare dashboard → R2 → `doodleiq-releases` → **Settings →
   Public access**: connect a custom domain (`get.doodleiq.com`) or enable the
   `r2.dev` subdomain. That public URL is `RELEASE_BASE_URL`.
3. **macOS signing** — follow `packaging/SIGNING.md`.
4. Put this in your shell profile (`~/.zshrc`):

   ```sh
   export RELEASE_BASE_URL="https://get.doodleiq.com"
   export R2_BUCKET="doodleiq-releases"
   export APPLE_CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
   export NOTARY_PROFILE="NOTARY_PROFILE"
   ```

   Wrangler handles auth from `wrangler login`. (Alternative: set `R2_TOOL=aws`
   plus `R2_ENDPOINT` + `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` from an R2
   *S3 API* token, and `brew install awscli`.)

## Cutting a release

```sh
# bump `version` in apps/provider/Cargo.toml, then from the repo root:
apps/provider/packaging/release-local.sh          # version taken from Cargo.toml
# or: apps/provider/packaging/release-local.sh v0.1.1
```

It builds macOS (universal, on this Mac) and Linux x86_64 (in a `rust:1-bookworm`
Docker container — amd64 is emulated on Apple Silicon but still ~5 min), signs +
notarizes macOS, writes `.sha256` files, renders the install scripts with
`RELEASE_BASE_URL` baked in, and uploads:

```
<bucket>/install.sh                          <bucket>/install.ps1
<bucket>/latest/doodleiq-*.tar.gz(.sha256)   (cache 5 min)
<bucket>/<version>/doodleiq-*.tar.gz(.sha256) (immutable)
```

`SKIP_UPLOAD=1` builds + packages into `apps/provider/dist/` without uploading.

### Adding targets

- **Linux arm64**: `LINUX_TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu" release-local.sh`
- **Windows**: `aws-lc-sys` needs the MSVC toolchain, so build on a Windows box /
  VM: `cargo build --release --target x86_64-pc-windows-msvc`. Zip it as
  `doodleiq-windows-x86_64.zip`, drop it (and its `.sha256`) into
  `apps/provider/dist/archives/`, then push just those:
  ```sh
  wrangler r2 object put doodleiq-releases/latest/doodleiq-windows-x86_64.zip \
    --file doodleiq-windows-x86_64.zip --remote --ct application/zip
  # ...and the same under <version>/ plus the .sha256
  ```

## Installing (end users)

macOS / Linux:
```sh
curl -fsSL https://get.doodleiq.com/install.sh | sh
```
Windows (PowerShell) — once a Windows build is published:
```powershell
irm https://get.doodleiq.com/install.ps1 | iex
```

Overrides: `DOODLEIQ_VERSION` (a version instead of `latest`), `DOODLEIQ_BASE_URL` /
`-BaseUrl`, `DOODLEIQ_BIN_DIR` / `-BinDir`. From a checkout, `sh
apps/provider/packaging/install.sh` falls back to GitHub Releases.

Then `doodleiq configure` → `doodleiq pair` → `doodleiq run`.

## Keeping it running

Copy the binary to a stable path, run `configure` + `pair` once as the service
user, then install the supervisor (each restarts the agent on failure/reboot —
the agent exits non-zero when its gateway or heartbeat loop dies):

| OS | File | Install |
|---|---|---|
| Linux | `packaging/doodleiq.service` | `sudo cp` to `/etc/systemd/system/`, edit `User=`, `systemctl enable --now doodleiq` |
| macOS | `packaging/com.doodleiq.provider.plist` | `cp` to `~/Library/LaunchAgents/`, edit paths, `launchctl load -w` |
| Windows | `packaging/install-windows-service.ps1` | run elevated in the binary's folder |
