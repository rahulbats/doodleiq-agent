# DoodleIQ agent

The provider-side agent for [DoodleIQ](https://doodleiq.com) — a marketplace where
people rent time on AI models run by independent providers, over a standard
OpenAI-compatible API.

This ~6 MB Rust binary runs on the provider's machine. It:

- Connects **outbound only** to Cloudflare and holds a managed tunnel open — no
  inbound ports, no router config, no static IP.
- Runs a local metering gateway on `127.0.0.1:47100` that counts tokens and
  forwards requests to your model runtime (Ollama, LM Studio, oMLX, vLLM,
  llama.cpp).
- Heartbeats machine health to the control plane every 30 seconds.
- Reports **signed usage receipts** (token counts, duration, status) after each
  request. It never uploads model weights, files, or the contents of any request
  or response.

Pairing generates an Ed25519 keypair locally; the private key never leaves the
machine.

## Install

macOS and Linux:

```sh
curl -fsSL https://get.doodleiq.com/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://get.doodleiq.com/install.ps1 | iex
```

## Use

You need an OpenAI-compatible model runtime already running locally.

```sh
# 1. point the agent at your runtime
doodleiq configure --url http://127.0.0.1:11434/v1/models   # Ollama
doodleiq configure --url http://127.0.0.1:1234/v1/models    # LM Studio
doodleiq configure                                          # oMLX (default)

# 2. link this machine to your DoodleIQ account (once per machine)
doodleiq pair      # prints a URL + code — approve it in your browser

# 3. start serving
doodleiq run       # opens the tunnel + heartbeat, streams requests until Ctrl+C
```

Other commands:

```sh
doodleiq status              # configuration + dependency check
doodleiq reset               # clear pairing (keep runtime config)
doodleiq reset --all         # full reset, including the device key
```

Pass `--api-key` (or `export DOODLEIQ_MODEL_API_KEY=...`) only if your runtime
requires a bearer token — Ollama and LM Studio do not.

To keep it running across reboots, run `doodleiq run` under a supervisor. Unit
files for systemd, launchd, and Windows are in [`packaging/`](packaging/).

## Build from source

```sh
cargo build --release        # target/release/doodleiq
cargo test
```

Requires a recent stable Rust toolchain. TLS is rustls + the platform verifier —
no OpenSSL.

## Configuration

| Variable | Purpose |
|---|---|
| `DOODLEIQ_CONTROL_PLANE_URL` | control plane base URL (default `https://api.doodleiq.com`) |
| `DOODLEIQ_MODEL_API_KEY` | bearer token for your model runtime, if it needs one |
| `DOODLEIQ_CONFIG_DIR` | override the config directory (default `~/.config/doodleiq`) |

## Releases

Built and published locally — see [`DISTRIBUTION.md`](DISTRIBUTION.md) and
[`packaging/release-local.sh`](packaging/release-local.sh). No CI service.

## License

MIT — see [LICENSE](LICENSE).
