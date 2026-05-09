# Cross-version integration test (T1C.7)

Goal: confirm the **legacy Python orchestrator** can drive the **new Rust
guac bridge** image end-to-end. The wire format between them is the
encrypted AES-256-CBC token from `flowcase/routes/droplet.py:578-616`,
plus the Guacamole protocol over WebSocket. Both have unit + Docker
integration coverage already (T1C.2 and T1C.3-T1C.4); this test closes
the loop with a real browser session.

This is a **manual checkpoint**. Steps 1–4 are scripted; step 5 needs
a human at a browser to confirm pixels move and input lands.

## Pre-reqs

- Docker (Desktop or Engine) running.
- A copy of `flowcase` (the orchestrator) checked out at this repo's
  sibling path.
- Python 3.11+ with `pip install -r flowcase/requirements.txt`. (On
  macOS: `brew install python@3.11` and use that interpreter — system
  3.14 has the pycryptodome / libexpat ABI mismatch documented in this
  repo's bench history.)

## 1. Build the Rust bridge image

```sh
cd flowcase-guac
docker build -t flowcaseweb/flowcase-guac:test .
```

Expected: 172 MB image (T1C.6 baseline). Verify:

```sh
docker images flowcaseweb/flowcase-guac:test --format '{{.Size}}'
```

## 2. Spin up a network with VNC + bridge + guacd

```sh
docker network create guactest

docker run -d --rm --name vnc-target --network guactest \
    consol/ubuntu-xfce-vnc:latest

# The bridge image runs guacd internally on 127.0.0.1:4822 and
# exposes the WS bridge on 8080.
docker run -d --rm --name flowcase-guac --network guactest \
    -p 18080:8080 \
    -e GUAC_KEY='this-is-a-32-byte-test-key!12345' \
    flowcaseweb/flowcase-guac:test
```

Default VNC password on `consol/ubuntu-xfce-vnc` is `vncpassword`.

Verify:

```sh
curl -sI http://localhost:18080/vnc.html | head -3
# HTTP/1.1 200
# content-type: text/html; charset=utf-8

docker logs flowcase-guac 2>&1 | tail -3
# guacd[…]: Listening on 127.0.0.1, port 4822
# starting flowcase-guac listen=0.0.0.0:8080 …
```

## 3. Generate a token the orchestrator way

The orchestrator builds a token at `droplet.py:595` from the user's
`auth_token` (truncated to 32 bytes) and the droplet's connection
details. We replicate that here so we don't need the full orchestrator
running just to mint one. The Node version below is byte-equivalent to
the Python version (both delegate to OpenSSL AES-256-CBC + PKCS7).

```sh
node -e "
const c = require('crypto');
const key = Buffer.from('this-is-a-32-byte-test-key!12345');
const iv = c.randomBytes(16);
const inner = { connection: { type: 'vnc', settings: {
    hostname: 'vnc-target', username: null, password: 'vncpassword',
    port: 5901, 'disable-copy': 'false', 'disable-paste': 'false',
} } };
const ci = c.createCipheriv('aes-256-cbc', key, iv);
const enc = Buffer.concat([ci.update(JSON.stringify(inner), 'utf8'), ci.final()]);
const env = { iv: iv.toString('base64'), value: enc.toString('base64') };
console.log(Buffer.from(JSON.stringify(env)).toString('base64'));
"
```

Save the output to `\$TOKEN`.

To mint a token from the **real** Python orchestrator instead, run a
session, click into the droplet, and inspect the `guac_token` value
that `flowcase/routes/droplet.py:649` injects into `droplet.html`.
That confirms the orchestrator-side encoder talks to our decoder.

## 4. Smoke the WS handshake (no browser)

```sh
# Should accept upgrade and start streaming guac instructions.
# Use any cli WS client; example with the bridge's own tokio-tungstenite
# from cargo run --example would also work, but a quick websocat:
websocat "ws://localhost:18080/vnc.html?guac_token=\$TOKEN" --binary
```

Expected: the bridge logs `bridging WS to guacd` and the WS receives
text frames ending in `;` (Guacamole instructions). Send a `4.sync,1.0;`
frame and observe the next `sync` reply.

(If you skip this step, the next step still proves the bridge works.)

## 5. Browser checkpoint — **human required**

Open a browser to:

```
http://localhost:18080/vnc.html?instance_id=manual&guac_token=$TOKEN
```

Confirm in order:

1. Page loads (HTML body renders, no console errors about guac assets).
2. WebSocket connects to `ws://localhost:18080/vnc.html?guac_token=…`
   (visible in the network tab).
3. The XFCE desktop appears within ~3 s.
4. Mouse moves and clicks register on the remote desktop.
5. Keyboard input lands in a terminal opened on the remote.
6. Closing the browser tab causes guacd-side disconnection in the
   bridge's logs (`Disconnected WebSocket` analogue).

Failure modes to watch for:
- "Invalid Token" close (code 4002) → `GUAC_KEY` mismatch.
- Bridge logs `connecting to guacd at 127.0.0.1:4822` failure → guacd
  startup race; let it settle 3 s after `docker run`.
- Black screen with no protocol errors → `disable-copy/paste` settings
  rejected by guacd; check `args` from the handshake matches the
  parameter names guacd 1.5.5 actually publishes for VNC.

## 6. Tear down

```sh
docker stop flowcase-guac vnc-target
docker network rm guactest
```

## 7. Run the same test against the **real** orchestrator

If you have the legacy orchestrator running (`docker compose up` from
`../flowcase/`):

1. Bring it up with the **new Rust guac bridge image** instead of the
   Node one. In `flowcase/docker-compose.yml`, change the `flowcase-guac`
   service `image:` to `flowcaseweb/flowcase-guac:test`.
2. Set the same `GUAC_KEY` value on both the orchestrator (`GUAC_AES_KEY`
   env) and the guac container (`GUAC_KEY`).
3. Log in, create a `vnc` droplet pointing at a test VNC host
   (e.g., a sibling `vnc-target` container), launch a session.
4. The orchestrator generates the token via `generate_guac_token` and
   embeds it in `droplet.html`. The page's
   `Guacamole.WebSocketTunnel` opens `wss://host/desktop/<id>/vnc/vnc.html?guac_token=…`,
   which after the orchestrator's reverse proxy reaches our bridge as
   `/vnc.html?guac_token=…`.
5. Same checkpoint as step 5 above.

## Status

- [x] Token format compatibility — proved byte-exact in T1C.2's
      hardcoded Node-fixture decrypt test and T1C.4's encrypt+round-trip.
- [x] guacd handshake — proved in T1C.3 against guacamole/guacd:1.5.5.
- [x] WS bridge end-to-end (no browser) — proved in T1C.4 via
      tokio-tungstenite client connecting to a real bridge instance.
- [x] Docker image works — T1C.6 smoke (curl /vnc.html → 200).
- [ ] **Browser checkpoint (steps 5 + 7) — pending human verification.**

When the browser checkpoint passes, flip `T1C.7` in REFACTOR_PLAN.md
from `[!]` to `[x]` and append the date + browser used.
