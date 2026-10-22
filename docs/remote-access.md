# Remote access (PandaCode Connect)

Opt-in. Off by default. Signing in lets one PandaCode window see and drive the tasks on the
user's other computers that are signed in to the same account and have **Allow remote control
of this computer** turned on. Nothing is sent anywhere until the user signs in.

## Pieces

| Piece | Where | Job |
|---|---|---|
| Relay | `relay/` (Cloudflare Worker + one Durable Object per account) | Sign-in, device list, forwards encrypted frames between an account's devices. Stores no task content. |
| Native | `src-tauri/src/remote/` | Sign-in, keys in the OS keychain, one WebSocket to the relay, host RPC, client calls. |
| UI | `src/remote*.ts`, `src/remote.css` | Settings → Remote access, sidebar **Computers**, the remote task view. |

## Accounts

Sign in with GitHub using the OAuth **device flow** (no client secret anywhere):

1. Desktop: `POST https://github.com/login/device/code` with `client_id`, and the scope is empty (identity only).
2. The user enters the code at `verification_uri`. The desktop polls `https://github.com/login/oauth/access_token`.
3. Desktop → relay `POST /v1/auth/github` `{ githubToken, device: { id, name, platform, publicKey } }`.
   The relay calls `GET https://api.github.com/user` and gets the numeric id, login and avatar_url. It
   registers the device and returns `{ token, account: { id: "gh:<id>", login, avatarUrl } }`.
4. The desktop stores only the relay token in the keychain (`app.pandacode.remote`). It discards the GitHub token.

The device key is made once, at sign-in, behind a lock (so two sign-ins cannot race) and before
polling starts. A keychain *error* (a denied prompt, a locked keychain) fails sign-in; only a
missing key is ever generated, so an existing key is never overwritten. `remote_sign_in_cancel`
stops the device-flow poll, and a new `remote_sign_in_start` cancels any earlier flow.

A relay `401` (or close code `4001`) signs this computer out only if the token that request
used is still the current token, so a late refusal of an old token never signs out a newer
sign-in.

If the relay answers `409` (this device id is already registered with a different public key), the
desktop makes a new device id, saves it, and retries once.

Relay token: `base64url(JSON {acct, dev, iat, exp}) + "." + base64url(HMAC-SHA256(SESSION_SECRET))`.
It is valid for 30 days, and only while the device is still registered on the account. Revoking a
device closes its socket and makes its token invalid.

Build config is `option_env!("PANDACODE_RELAY_URL")` and `option_env!("PANDACODE_GITHUB_CLIENT_ID")`.
Environment variables of the same names override them at run time. If either is missing, remote
access is shown as "not configured in this build".

## Relay HTTP API

All routes except auth take `Authorization: Bearer <relay token>`. Errors are `{ error: string }`.

- `POST /v1/auth/github` → `{ token, account }`
- `GET /v1/devices` → `{ devices: [{ id, name, platform, publicKey, host, online, lastSeen }] }`.
  The desktop uses it only for an explicit `remote_devices` (or trust) call made before the
  socket's first presence frame.
- `DELETE /v1/devices/:id` → `{ ok: true }`
- `GET /v1/connect?host=0|1` (WebSocket upgrade) → joins the account's Durable Object, advertising
  whether this device hosts

## Relay frames (WebSocket text, JSON)

Client → relay: `{ to: deviceId, body: string, ref?: string }`. `ref` is the request id of the
request or response the body carries (events have none). The relay adds `from` and delivers
`{ from, to, body }` to the target device's socket. If it cannot, it answers
`{ kind: "undeliverable", to, ref?, reason: "offline" | "rate" }`. With a `ref` the desktop fails
only that pending call; without one it fails every pending call to `to`.

Relay → client:
- `{ kind: "presence", devices: [{ id, name, platform, publicKey, host, online, lastSeen }] }`, the
  account's full device rows, sent on connect and on every join, leave or device change.
- `{ kind: "undeliverable", ... }` (above)
- `{ kind: "error", error }`

Client → relay control: `{ kind: "host", enabled: boolean }` changes the advertised host flag
after connecting. `"ping"` gets `"pong"` as a hibernation auto-response.

Presence is the device list: the desktop takes devices, names and keys from it and never fetches
on connect or for an unknown sender. A device missing from presence is removed, and its event
subscription with it.

Close codes: `4000` (replaced by a newer socket) makes the client wait at least 60 s before it
reconnects. `4001` (device revoked) signs this computer out locally. Otherwise the client
reconnects with a backoff that starts at 1 s and doubles to 60 s. The backoff resets to 1 s only
after a socket stayed up for 30 s or more.

The relay uses the hibernation API (`ctx.acceptWebSocket`, tagged with the device id), so idle
sockets cost no duration. Device rows live in the Durable Object's SQLite storage.

## Limits (free)

| Limit | Value | Why |
|---|---|---|
| Devices per account | 10 | Fits personal use. Stops abuse. |
| Sockets per device | 1 | A new socket closes the old one. |
| Frame size | 256 KiB | Large transcripts are paged (see `task.get`). |
| Frame rate per socket | 20/s sustained, burst 100 | Token bucket. Over the limit → `error` and drop. |
| Auth calls | 10/min per IP | Workers rate-limit binding |
| Idle socket | kept (hibernated). The client pings every 30 s. | |

## End-to-end encryption

Each device has an X25519 key pair. The private key stays in the keychain, and the public key is
registered with the device. For a pair of devices:

`key = HKDF-SHA256(ikm = X25519(mine, theirs), salt = sorted(idA, idB).join("|"), info = "pandacode-remote v1")`.

`body = base64(nonce24 || XChaCha20-Poly1305(key, nonce24, plaintext, aad = from + "|" + to))`.
Pair keys are cached by `(deviceId, publicKey)`, so a key change derives a new one.
The relay sees only device ids and ciphertext. Settings shows each device's key fingerprint (first
8 bytes of SHA-256 of the public key, hex) so the user can compare them.

### Trust (key pinning)

The relay lists the public keys, so it could add a device or swap a key. A host therefore answers
only computers that the user trusted on that host. `remote.json` keeps
`trusted: { deviceId: publicKeyBase64 }`, and the key is pinned when the user trusts the device.

- A device is trusted only while its pinned key equals the key the relay lists now. After a key
  change it is untrusted until the user trusts it again.
Trust is symmetric: `remote_set_trusted` pins a device's key for both directions.

- Host side: a request from an untrusted device gets only the error `This computer isn't trusted
  on <host name>. Trust it in Settings → Remote access on that computer.` Trust is checked before
  the replay check, so untrusted callers never fill the nonce set. Task content and events are
  encrypted only to pinned keys.
- Viewer side: the desktop refuses to call or seal to a host that isn't trusted with its current
  key: `<host name> isn't trusted on this computer yet. Trust it in Settings → Remote access.`
  Responses and events from an untrusted device are dropped.
- Subscribers that are no longer on the device list or no longer trusted are dropped.
- Revoking a device removes its pin.

## App protocol (encrypted plaintext, JSON)

- Request `{ type: "req", id, method, params, ts, nonce }`: `ts` is unix time in ms, and `nonce` is
  16–64 random printable characters. A host refuses a request if `ts` is more than 60 s old or
  ahead, and refuses any nonce it saw in that window. The seen-nonce set is pruned to the window
  and bounded.
- Response `{ type: "res", id, ok: true, result }` or `{ type: "res", id, ok: false, error }`
- Event `{ type: "event", name, data }`

A host answers only while **Allow remote control** is on. Otherwise it returns
`{ ok: false, error: "Remote control is off on this computer" }`. It answers only devices that are
trusted on it (see Trust). A host still applies its own approval policy: remote prompts get no
more power than local ones.

| Method | Params | Result |
|---|---|---|
| `subscribe` | `{ sessionId? }` | `{}`. The host sends `tasks.changed` events to this device for 10 minutes; the client re-subscribes. `sessionId` is the task the viewer has open (its focus); omitting it clears the focus. |
| `tasks.list` | `{}` | overlay `Snapshot` (`overlay.rs`) |
| `task.get` | `{ sessionId, historyVersion?, beforeId? }` | overlay `ThreadSnapshot` (`messages, hasOlder, liveText, status, provider, historyVersion, historyUnchanged`) plus `approvals: [{ requestId, title?, detail? }]` |
| `task.start` | `{ projectId?, text }` | `{ sessionId }` (like `overlay_launch`) |
| `task.send` | `{ sessionId, text }` | `{ queued: boolean }`. Queued if the task is running, like Enter. If the run ended between the check and the queue add, the host takes the entry back and sends it. |
| `task.stop` | `{ sessionId }` | `{}` |
| `approval.respond` | `{ sessionId, requestId, approve: boolean }` | `{}` |

`historyVersion` is per task: the host hashes the task's max message id, message count and total
content length (kept under 2^53 so JavaScript reads it exactly), so other tasks' writes never
change it. `task.get` without `beforeId` and with the current `historyVersion` returns
`historyUnchanged: true`, `messages: []` and `hasOlder: false`, with the live state only.

Pages are capped so the sealed frame stays under 256 KiB: the reply's JSON is kept within 180 KiB.
`liveText` keeps its last 32 KiB. Any message over 120 KiB is cut and ends with
`…[cut on the host to fit a remote page]` (JSON tool content is cut inside, so it still parses).
The oldest messages that do not fit are dropped from the page and `hasOlder` is set; the viewer
pages back with `beforeId` = the oldest id it has.

Event `tasks.changed { sessionIds: [...], focus?: { sessionId, liveText, status, historyVersion } }`:
the host batches changed tasks and sends each subscriber at most one event every 250 ms, with up
to 500 ids per event. `focus` is included when the subscriber's focused task is among the changed
ids (`liveText` is its last 32 KiB). The viewer emits one `remote://task-changed` window event per
id, and `live` only for the focused one, so streaming text needs no `task.get` until
`historyVersion` changes. Streaming therefore costs at most 4 event frames a second per
subscriber, however many tasks run and whatever the token rate.

Responses and events are handled as soon as they arrive and never wait behind a slow request.
Requests from one peer are answered in order.

## Tauri commands (UI contract)

| Command | Args | Returns |
|---|---|---|
| `remote_status` | | `{ configured, signedIn, account?: { login, avatarUrl }, device: { id, name, fingerprint }, hostEnabled, connected }` |
| `remote_sign_in_start` | | `{ userCode, verificationUri, expiresIn }` |
| `remote_sign_in_finish` | | status (waits for the device flow, error on expiry, denial or cancel) |
| `remote_sign_in_cancel` | | status. Stops the device-flow poll. |
| `remote_sign_out` | | status |
| `remote_set_host_enabled` | `{ enabled }` | status |
| `remote_devices` | | `[{ id, name, platform, fingerprint, host, online, lastSeen, current, trusted }]`, from presence (fetched only before the first presence frame) |
| `remote_set_trusted` | `{ deviceId, trusted }` | the `remote_devices` list. Pins the device's current key for both directions, or removes the pin. |
| `remote_revoke_device` | `{ deviceId }` | `{}` |
| `remote_call` | `{ deviceId, method, params }` | result. Rejects if the host isn't trusted here, with the host's error, when undeliverable, or after a 20 s timeout. |

Window events: `remote://status` (status), `remote://presence` (devices), and
`remote://task-changed` `{ deviceId, sessionId, live?: { liveText, status, historyVersion } }`
(`live` only for the subscriber's focused task).

The agent-event hook in `lib.rs` clones and allocates nothing while hosting is off or nobody
subscribes.

## Privacy

Listed in the README privacy table: remote access contacts GitHub (sign-in only) and the relay
(while signed in). Task content crosses the relay only encrypted end to end.
