# Agent ↔ control-plane protocol

This documents every network call the agent makes, so you can verify exactly
what leaves your machine — and, if you want, point the agent at your own server
with `DOODLEIQ_CONTROL_PLANE_URL`.

The hosted control plane (marketplace, payments, discovery, reputation) is a
proprietary service. What is specified here is only the **agent-facing contract**:
six HTTPS endpoints plus the local metering gateway. Anything a compatible server
needs to answer consistently is below; how it issues grants, prices sessions, or
settles money is entirely its own concern.

- Base URL: `DOODLEIQ_CONTROL_PLANE_URL` (default `https://api.doodleiq.com`),
  trailing slash trimmed.
- All request/response bodies are JSON unless noted.
- The agent's HTTP client uses a 5s connect / 15s total timeout (no total
  timeout on the request-proxy path).

---

## Identity & auth

Pairing a machine generates an **Ed25519 keypair** locally
(`~/.config/doodleiq/device.key`, 32-byte seed, base64url no-pad). The private key
never leaves the machine. Two credentials flow from it:

| Credential | Sent as | Used for |
|---|---|---|
| **Pairing token** — opaque URL-safe token (32 random bytes) returned by `POST /v1/device-pairings` | `X-DoodleIQ-Pairing-Token: <token>` header | pairing status, heartbeat, grant validate, grant status |
| **Ed25519 signature** — `sign(private_key, raw_request_body)` | `X-DoodleIQ-Signature: <base64url-nopad>` + `X-DoodleIQ-Device: <device_id>` headers | usage receipts |

A server authenticates the pairing token by comparing `sha256(token)` to the hash
it stored at pairing time, and requires the device to be active. It authenticates
a receipt by verifying the signature against the device's registered public key
over the **exact bytes** of the request body.

---

## 1. `POST /v1/device-pairings` — begin pairing

No auth. Called by `doodleiq pair`.

```jsonc
// request
{
  "device_name": "rahul's MacBook Pro",
  "public_key": "9x1...Qk"        // base64url no-pad of the 32-byte Ed25519 public key
}
```

```jsonc
// 201 response
{
  "id": "0e5c…",                  // pairing id (uuid)
  "code": "A1B2C3D4",             // short code the user approves in a browser
  "pairing_token": "s7Kd…",       // opaque; the agent stores this
  "verification_url": "https://…/pair-device?code=A1B2C3D4",
  "expires_at": "2026-01-01T00:10:00Z"
}
```

The agent prints `verification_url` + `code` and polls endpoint 2 until the user
approves it out of band. (In the hosted service, approval is
`POST /v1/device-pairings/{code}/claim`, an authenticated browser call — not part
of the agent contract. A self-host can approve however it likes.)

---

## 2. `GET /v1/device-pairings/{pairing_id}` — pairing status / tunnel token

Auth: `X-DoodleIQ-Pairing-Token`. Polled during `doodleiq pair`, and again on
every `doodleiq run` to (re)fetch the tunnel token.

```jsonc
// response while unapproved
{ "status": "pending" }

// response once approved
{
  "status": "complete",
  "user_id": "…",                 // account id
  "provider_id": "…",
  "device_id": "…",               // the agent stores this
  "hostname": "<device_id>.doodleiq.com",
  "tunnel_token": "eyJ…",         // Cloudflare tunnel run token (see "Ingress")
  "tunnel_ready": true
}
```

`404`/`410` here means the pairing was removed server-side — the agent treats
that as "this machine was delisted" and stops with instructions to re-pair.

---

## 3. `POST /v1/devices/{device_id}/heartbeat` — liveness + machine info

Auth: `X-DoodleIQ-Pairing-Token`. Sent every 30s by `doodleiq run`.

```jsonc
// request
{
  "model_id": "qwen3.5:32b",      // model the runtime currently serves
  "available": true,              // false = going offline / not serving
  "model_loaded": true,           // false = runtime is reloading a checkpoint
  "tunnel_healthy": true,         // is the local cloudflared connector up
  "seq": 42,                      // monotonic per-process counter
  "agent_version": "0.1.3",
  "reason": null,                 // "shutdown" | "model_switch" | "model_reloading" | "model_unloaded" | null
  "machine_name": "rahul's MacBook Pro",
  "machine_model": "Mac17,8",
  "machine_location": "America/Denver"   // IANA tz, best effort
}
```

```jsonc
// response
{
  "status": "active",
  "hostname": "<device_id>.doodleiq.com",   // agent uses this for its "online at" URL
  "listing_id": "…"
}
```

`401`/`404`/`410` ⇒ device delisted (agent stops cleanly). The control plane is
expected to derive health purely from the age of the last heartbeat — the agent
sends no explicit "I'm alive" beyond this.

---

## 4. `POST /v1/devices/{device_id}/inference-grants/validate` — check a consumer's grant

Auth: `X-DoodleIQ-Pairing-Token`. Called by the gateway on **every** proxied
request (the consumer presents `Authorization: Bearer <grant_id>.<request_token>`),
so a revoked or expired grant stops working immediately.

```jsonc
// request
{ "grant_id": "…", "request_token": "…" }
```

```jsonc
// 200 response — the grant is valid for this device right now
{
  "model_id": "qwen3.5:32b",
  "max_tokens": 2147483647,
  "expires_at": "2026-01-01T01:00:00Z",
  "expires_at_epoch": 1767229200,
  "started_at": "2026-01-01T00:00:00Z",
  "started_at_epoch": 1767225600,
  "price_per_second": 0.002
}
```

Non-2xx ⇒ the gateway rejects the consumer request with `401`.

---

## 5. `GET /v1/devices/{device_id}/inference-grants/{grant_id}/status` — session state

Auth: `X-DoodleIQ-Pairing-Token`. Used by the gateway's `/v1/session-connect` and
`/v1/session-disconnect` handlers for the local "consumer connected" display.

```jsonc
// response
{
  "status": "issued",             // "issued" | "completed" | "expired"
  "expires_at_epoch": 1767229200,
  "started_at_epoch": 1767225600,
  "price_per_second": 0.002,
  "provider_revenue": 0.9         // running provider-side total, for display only
}
```

---

## 6. `POST /v1/usage-receipts` — signed metering record

Auth: **Ed25519 signature over the raw body** — `X-DoodleIQ-Signature` +
`X-DoodleIQ-Device` headers, `Content-Type: application/json`. **No** pairing
token. Submitted once per completed request (fire-and-forget; a failure is logged,
not retried).

```jsonc
// request body — sign these exact bytes
{
  "grant_id": "…",
  "request_token": "…",
  "request_id": "<grant_id>-<unix_nanos>",   // unique; server dedupes on it
  "input_tokens": 812,
  "output_tokens": 240,
  "total_tokens": 1052,
  "duration_ms": 3400,
  "status": "completed"                       // "completed" | "failed"
}
```

The server verifies `verify(device.public_key, signature, raw_body)`, checks the
grant belongs to this device and matches `sha256(request_token)`, and that the
receipt arrives within a grace window after the grant expiry (hosted default:
120 min). `201` on success; `401` "Invalid device signature" tells the user their
local key no longer matches the registration (`doodleiq reset && doodleiq pair`).

No message content is ever sent — only counts, duration, and status.

---

## Ingress: the tunnel token

`tunnel_token` from endpoint 2 is a **Cloudflare Tunnel run token**. The agent:

1. downloads `cloudflared` if absent (from Cloudflare's GitHub releases),
2. runs `cloudflared tunnel run --token <tunnel_token>`,
3. Cloudflare routes `https://<hostname>/…` to the connector, which forwards to
   the gateway on `127.0.0.1:47100`.

This is the one hard external dependency. A self-hosted control plane must either:

- own a Cloudflare account + zone, create a `cfd_tunnel` per device, configure its
  ingress to `http://127.0.0.1:47100`, add the proxied CNAME, and return the run
  token as `tunnel_token`; **or**
- return `tunnel_ready: false` / no token and provide ingress some other way — in
  which case `doodleiq run` currently exits, since it treats a missing token as
  fatal. Loosening that (a "bring your own tunnel" / plain reverse-proxy mode) is
  on the roadmap but not implemented.

---

## The local metering gateway

Bound to `127.0.0.1:47100`, reachable only by `cloudflared` on the same machine.
This is what consumers actually talk to (through the tunnel).

| Route | Purpose |
|---|---|
| `GET /health` | agent status: `configured`, `paired`, rolling usage + heartbeat health |
| `POST /v1/session-connect` `{grant_id}` | mark a consumer session active locally (verifies via endpoint 5) |
| `POST /v1/session-disconnect` `{grant_id}` | clear the local session; returns `provider_revenue` |
| `ANY /v1/{*path}` | proxy to the configured model runtime |

Proxy behaviour:

- Requires `Authorization: Bearer <grant_id>.<request_token>`; **every** proxied
  request re-validates it against endpoint 4, so a revoked or expired grant stops
  working immediately.
- For a streaming `POST /v1/chat/completions`, injects
  `stream_options.include_usage` so the terminal `usage` block can be metered.
- Buffers the request body (bounded at 32 MiB), forwards to the runtime host from
  `doodleiq configure --url`, streams the response back untouched.
- On seeing `usage` in the response, submits a signed receipt (endpoint 6).
- Strips hop-by-hop headers; adds `X-Content-Type-Options: nosniff`.

---

## Minimal compatible server checklist

To run the agent against your own control plane you need to implement, at
minimum: endpoints 1–6 with the auth above, a device registry keyed by Ed25519
public key, per-device pairing tokens, grant issuance + `validate`/`status`, and
Cloudflare tunnel provisioning (or a fork that removes the hard tunnel
requirement). The health state machine and settlement logic are yours to define —
the agent only reports; it never trusts its own view of "am I listed".
