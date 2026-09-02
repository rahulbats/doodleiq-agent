#!/usr/bin/env bash
#
# Build, sign, and publish the DoodleIQ provider agent — no CI, no GitHub Actions.
#
#   apps/provider/packaging/release-local.sh [version]
#
# Builds:
#   - macOS universal (arm64 + x86_64), on this Mac
#   - Linux, in Docker (LINUX_TARGETS; native arch is fast, cross arch is emulated)
#
# Env:
#   RELEASE_BASE_URL   public URL of your bucket, e.g. https://get.doodleiq.com   (required to upload)
#   R2_BUCKET          e.g. doodleiq-releases                                      (required to upload)
#   APPLE_CODESIGN_IDENTITY   "Developer ID Application: NAME (TEAMID)"            (optional; unsigned if unset)
#   NOTARY_PROFILE           keychain profile from `xcrun notarytool store-credentials`  (optional)
#   LINUX_TARGETS            default "x86_64-unknown-linux-gnu"; space-separated; "none" to skip
#   REPO                     default "rahulbats/doodleiq-agent" (only used in the GitHub fallback URL)
#   SKIP_UPLOAD=1            build + package only, leave archives in apps/provider/dist/
#
# Upload uses whichever is set up:
#   wrangler  — `wrangler login` (or CLOUDFLARE_API_TOKEN + CLOUDFLARE_ACCOUNT_ID); no S3 keys
#   aws       — S3 API: R2_ENDPOINT + AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY
#   force one with R2_TOOL=wrangler|aws
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crate="$(cd "$here/.." && pwd)"
cd "$crate"

REPO="${REPO:-rahulbats/doodleiq-agent}"
LINUX_TARGETS="${LINUX_TARGETS:-x86_64-unknown-linux-gnu}"
VERSION="${1:-v$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)}"

dist="$crate/dist"
rm -rf "$dist"
mkdir -p "$dist/archives" "$dist/root"

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33m!!  %s\033[0m\n' "$*" >&2; }

# ---------------------------------------------------------------- macOS universal
if [ "$(uname -s)" = "Darwin" ]; then
  say "macOS universal build"
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --locked --target aarch64-apple-darwin
  cargo build --release --locked --target x86_64-apple-darwin

  stage="$dist/doodleiq-macos-universal"
  mkdir -p "$stage"
  lipo -create -output "$stage/doodleiq" \
    target/aarch64-apple-darwin/release/doodleiq \
    target/x86_64-apple-darwin/release/doodleiq
  lipo -info "$stage/doodleiq"

  if [ -n "${APPLE_CODESIGN_IDENTITY:-}" ]; then
    say "sign + notarize"
    codesign --force --timestamp --options runtime \
      --sign "$APPLE_CODESIGN_IDENTITY" "$stage/doodleiq"
    codesign --verify --strict --verbose=2 "$stage/doodleiq"
    if [ -n "${NOTARY_PROFILE:-}" ]; then
      ( cd "$stage" && zip -q "$dist/nz.zip" doodleiq )
      xcrun notarytool submit "$dist/nz.zip" --keychain-profile "$NOTARY_PROFILE" --wait
      rm -f "$dist/nz.zip"
      # A bare binary can't be stapled; Gatekeeper checks the ticket online on first run.
    else
      warn "NOTARY_PROFILE unset — signed but NOT notarized (Gatekeeper will warn on download)."
      warn "One-time: xcrun notarytool store-credentials NOTARY_PROFILE --key AuthKey.p8 --key-id KEYID --issuer ISSUER"
    fi
  else
    warn "APPLE_CODESIGN_IDENTITY unset — shipping an UNSIGNED macOS binary."
  fi

  cp -R packaging DISTRIBUTION.md "$stage/"
  ( cd "$dist" && tar -czf "archives/doodleiq-macos-universal.tar.gz" "doodleiq-macos-universal" )
  rm -rf "$stage"
else
  warn "not on macOS — skipping the macOS build"
fi

# ----------------------------------------------------------------- Linux (Docker)
[ "$LINUX_TARGETS" = "none" ] && LINUX_TARGETS=""
if [ -n "${LINUX_TARGETS// /}" ] && command -v docker >/dev/null 2>&1; then
  host_arch="$(uname -m)"
  for target in $LINUX_TARGETS; do
    case "$target" in
      x86_64-*)  plat="linux/amd64"; slug="linux-x86_64";  native=$([ "$host_arch" = "x86_64" ] && echo 1 || echo 0) ;;
      aarch64-*) plat="linux/arm64"; slug="linux-aarch64"; native=$([ "$host_arch" = "arm64" ] || [ "$host_arch" = "aarch64" ] && echo 1 || echo 0) ;;
      *) warn "unknown LINUX_TARGET $target — skipping"; continue ;;
    esac
    say "Linux build: $target ($plat)"
    [ "$native" = "1" ] || warn "$plat is emulated on this host — expect a slow (10-40 min) build."

    docker run --rm --platform "$plat" \
      -v "$crate":/w -w /w -e CARGO_HOME=/w/.cargo-docker \
      rust:1-bookworm \
      sh -euc "apt-get update -qq >/dev/null && apt-get install -y -qq cmake perl >/dev/null; \
               cargo build --release --locked --target $target; \
               chown -R $(id -u):$(id -g) target .cargo-docker"

    stage="$dist/doodleiq-$slug"
    mkdir -p "$stage"
    cp "target/$target/release/doodleiq" "$stage/"
    cp -R packaging DISTRIBUTION.md "$stage/"
    ( cd "$dist" && tar -czf "archives/doodleiq-$slug.tar.gz" "doodleiq-$slug" )
    rm -rf "$stage"
  done
elif [ -z "${LINUX_TARGETS// /}" ]; then
  warn "LINUX_TARGETS=none — skipping Linux builds"
else
  warn "docker not found — skipping Linux builds"
fi

# ------------------------------------------------------------------- checksums
say "checksums"
( cd "$dist/archives" && for f in *.tar.gz; do shasum -a 256 "$f" | tee "$f.sha256"; done )

# ------------------------------------------------------ render install scripts
for f in install.sh install.ps1; do
  sed -e "s#@@BASE_URL@@#${RELEASE_BASE_URL:-}#g" -e "s#@@REPO@@#$REPO#g" \
    "packaging/$f" > "$dist/root/$f"
done

# --------------------------------------------------------------- upload to R2
if [ "${SKIP_UPLOAD:-}" = "1" ]; then
  say "SKIP_UPLOAD=1 — archives are in $dist/archives, installers in $dist/root"
  exit 0
fi
: "${RELEASE_BASE_URL:?set RELEASE_BASE_URL to upload (or SKIP_UPLOAD=1)}"
: "${R2_BUCKET:?set R2_BUCKET}"

tool="${R2_TOOL:-}"
if [ -z "$tool" ]; then
  if command -v wrangler >/dev/null 2>&1; then tool=wrangler
  elif command -v aws >/dev/null 2>&1; then tool=aws
  else
    warn "no uploader found. Install one:  npm i -g wrangler   OR   brew install awscli"
    warn "archives: $dist/archives    installers: $dist/root"
    exit 1
  fi
fi
say "upload to R2 ($R2_BUCKET) via $tool"

# put <local-file> <bucket-key> <content-type>
if [ "$tool" = "wrangler" ]; then
  put() {
    wrangler r2 object put "$R2_BUCKET/$2" --file "$1" --remote \
      --content-type "$3" --cache-control "$4"
  }
else
  : "${R2_ENDPOINT:?set R2_ENDPOINT for the aws uploader}"
  export AWS_DEFAULT_REGION=auto
  export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED   # R2 rejects the default CRC32 trailer
  export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
  put() {
    aws s3 cp "$1" "s3://$R2_BUCKET/$2" --endpoint-url "$R2_ENDPOINT" \
      --content-type "$3" --cache-control "$4"
  }
fi

ct() { case "$1" in *.tar.gz) echo application/gzip;; *.zip) echo application/zip;;
                     *.sha256) echo text/plain;; *) echo application/octet-stream;; esac; }

for f in "$dist"/archives/*; do
  name="$(basename "$f")"
  put "$f" "$VERSION/$name" "$(ct "$name")" "public,max-age=31536000,immutable"
  put "$f" "latest/$name"   "$(ct "$name")" "public,max-age=300"
done
put "$dist/root/install.sh"  "install.sh"  "text/x-shellscript" "public,max-age=300"
put "$dist/root/install.ps1" "install.ps1" "text/plain"         "public,max-age=300"

say "published $VERSION"
echo "   $RELEASE_BASE_URL/install.sh"
echo "   $RELEASE_BASE_URL/latest/   (and $RELEASE_BASE_URL/$VERSION/)"
