# macOS code signing (local)

`release-local.sh` signs and notarizes the macOS binary when
`APPLE_CODESIGN_IDENTITY` (and, for notarization, `NOTARY_PROFILE`) are set in
your shell. Without them it still builds, with a warning — Gatekeeper then shows
"unidentified developer" on download.

You need an Apple Developer account.

## 1. Developer ID Application certificate

1. [developer.apple.com](https://developer.apple.com/account/resources/certificates) →
   **Certificates → +** → **Developer ID Application** (not "Apple Distribution").
2. Follow the CSR flow (Keychain Access → Certificate Assistant → Request a
   Certificate from a Certificate Authority → "Saved to disk"), upload the CSR,
   download the `.cer`, double-click it to add it to your login keychain.
3. Confirm it is usable:

   ```sh
   security find-identity -v -p codesigning
   # -> "Developer ID Application: Your Name (AB12CD34EF)"
   ```

   Put that whole string in your shell profile:

   ```sh
   export APPLE_CODESIGN_IDENTITY="Developer ID Application: Your Name (AB12CD34EF)"
   ```

Because signing runs on the same Mac that holds the key, there is no `.p12`
export or base64 — the key stays in your keychain.

## 2. Notarization credentials (one-time)

1. [App Store Connect](https://appstoreconnect.apple.com/access/integrations/api) →
   **Team Keys → +**, role **Developer**. Download `AuthKey_XXXXXXXXXX.p8` (one
   download only). Note the **Key ID** and the **Issuer ID**.
2. Store them in your keychain as a named profile so the build never handles the
   key directly:

   ```sh
   xcrun notarytool store-credentials NOTARY_PROFILE \
     --key ~/Downloads/AuthKey_XXXXXXXXXX.p8 \
     --key-id XXXXXXXXXX \
     --issuer 00000000-0000-0000-0000-000000000000
   ```

3. In your shell profile:

   ```sh
   export NOTARY_PROFILE=NOTARY_PROFILE
   ```

## 3. Verify a build

```sh
RELEASE_BASE_URL=https://get.doodleiq.com SKIP_UPLOAD=1 \
  apps/provider/packaging/release-local.sh

tar xzf apps/provider/dist/archives/doodleiq-macos-universal.tar.gz -C /tmp
codesign --verify --strict --verbose=2 /tmp/doodleiq-macos-universal/doodleiq
spctl --assess --type execute -v /tmp/doodleiq-macos-universal/doodleiq
# -> "accepted   source=Notarized Developer ID"
```

## Windows

Deferred — Authenticode needs an OV cert on a hardware token or a cloud service
(Azure Trusted Signing ~$10/mo). The Linux + macOS binaries cover the vast
majority of local-LLM providers; add Windows when there is demand.
