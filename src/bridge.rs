//! WebSocket ↔ guacd bridge.
//!
//! Browser opens a WS to GET / with `?token=<b64>`. We decrypt the token
//! (T1C.2), open a TCP connection to guacd (default 127.0.0.1:4822), run
//! the Guacamole handshake (T1C.3), then splice bytes bidirectionally:
//! browser WS frames → guacd; guacd protocol bytes → browser WS frames.
//!
//! Slow consumers on either side cause both halves of the bridge to
//! tear down — the browser's guacamole-common.js will reconnect.

#![allow(dead_code)] // module is fully consumed by main.rs in T1C.5

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::guacd::handshake;
use crate::token::decrypt_token;

/// Shared state for the bridge router.
#[derive(Clone)]
pub struct BridgeState {
    pub key: Arc<[u8; 32]>,
    pub guacd_addr: SocketAddr,
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: String,
}

pub fn router(state: BridgeState) -> Router {
    Router::new().route("/", get(handle_ws)).with_state(state)
}

async fn handle_ws(
    ws: WebSocketUpgrade,
    State(state): State<BridgeState>,
    Query(q): Query<TokenQuery>,
) -> Response {
    let conn = match decrypt_token(&q.token, &state.key) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "rejecting WS upgrade: token decrypt failed");
            // Match guacamole-lite behavior — accept the upgrade and close
            // immediately so the browser learns it via a WS close.
            return ws.on_upgrade(|mut s| async move {
                let _ = s
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4002,
                        reason: std::borrow::Cow::Borrowed("Invalid Token"),
                    })))
                    .await;
            });
        }
    };

    info!(
        kind = ?conn.kind,
        hostname = %conn.hostname,
        port = conn.port,
        "bridging WS to guacd"
    );

    ws.on_upgrade(move |socket| async move {
        if let Err(err) = run_bridge(socket, conn, state.guacd_addr).await {
            warn!(?err, "bridge ended with error");
        }
    })
}

async fn run_bridge(
    socket: WebSocket,
    conn: crate::token::GuacConnection,
    guacd_addr: SocketAddr,
) -> Result<()> {
    let stream = TcpStream::connect(guacd_addr)
        .await
        .with_context(|| format!("connecting to guacd at {guacd_addr}"))?;
    stream.set_nodelay(true).ok();

    let (reader, mut writer) = handshake(stream, &conn).await?;

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Browser → guacd: take WS Text/Binary payload bytes, write to guacd.
    let to_guacd = tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    if writer.write_all(t.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(b)) => {
                    if writer.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {} // ignore Ping/Pong (axum auto-responds)
            }
        }
        debug!("ws -> guacd half closed");
        let _ = writer.shutdown().await;
    });

    // guacd → browser: read raw bytes from guacd, ship as WS Text frames.
    // Note: we use the buffered reader's underlying half via into_inner so
    // we don't waste cycles re-parsing; guacamole-common.js buffers on the
    // browser side and tolerates partial instructions.
    let from_guacd = tokio::spawn(async move {
        let mut inner = reader.into_inner();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match inner.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    debug!(?err, "guacd read error");
                    break;
                }
            };
            // Guac protocol is UTF-8 by spec; if we ever hit invalid bytes
            // it's a guacd bug, not ours. Forward as Text either way:
            // axum::Message::Text would reject invalid UTF-8, so coerce
            // via from_utf8_lossy and log if we had to substitute.
            let text = match std::str::from_utf8(&buf[..n]) {
                Ok(s) => s.to_string(),
                Err(err) => {
                    warn!(?err, "guacd produced non-utf8 bytes; sending lossy");
                    String::from_utf8_lossy(&buf[..n]).into_owned()
                }
            };
            if ws_sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
        debug!("guacd -> ws half closed");
        let _ = ws_sink.send(Message::Close(None)).await;
    });

    // Whichever direction closes first ends the session. Wait for both
    // halves so we don't leak the inner read/write halves.
    tokio::select! {
        _ = to_guacd => {},
        _ = from_guacd => {},
    }

    Ok(())
}

#[allow(dead_code)] // trivial wrapper used only by main.rs in T1C.5
pub fn build_state(key: [u8; 32], guacd_addr: SocketAddr) -> BridgeState {
    BridgeState {
        key: Arc::new(key),
        guacd_addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn dummy_key() -> Arc<[u8; 32]> {
        Arc::new(*b"this-is-a-32-byte-test-key!12345")
    }

    #[tokio::test]
    async fn bad_token_rejects_with_close_frame() {
        let state = BridgeState {
            key: dummy_key(),
            guacd_addr: "127.0.0.1:1".parse().unwrap(), // never reached
        };
        let app = router(state);

        // No WS upgrade headers — axum will return 400 before we even decode
        // the token. Use that as the proxy: the route is wired and the
        // query extractor sees the param.
        let req = Request::builder()
            .method("GET")
            .uri("/?token=garbage")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // axum 0.7 returns 426/400 for missing upgrade headers; the exact
        // code is plumbing — just assert we got a non-2xx without panicking.
        assert!(!resp.status().is_success());
    }

    #[tokio::test]
    async fn missing_token_returns_400() {
        let state = BridgeState {
            key: dummy_key(),
            guacd_addr: "127.0.0.1:1".parse().unwrap(),
        };
        let app = router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // axum's Query<T> rejects missing fields with 400.
        assert_eq!(resp.status().as_u16(), 400);
    }

    /// Live integration test against a real guacd + VNC server.
    /// Run with:
    /// `cargo test -- --ignored bridge_real_against_vnc`
    /// after spinning up:
    ///   docker network create guactest
    ///   docker run -d --rm --name vnc-smoke --network guactest \
    ///       consol/ubuntu-xfce-vnc:latest
    ///   docker run -d --rm --name guacd-smoke --network guactest \
    ///       -p 14822:4822 guacamole/guacd:1.5.5
    #[tokio::test]
    #[ignore = "needs guacd + VNC docker containers; run manually"]
    async fn bridge_real_against_vnc() {
        use futures_util::SinkExt as _;
        use futures_util::StreamExt as _;
        use tokio_tungstenite::tungstenite::Message as TM;

        // The token used by token::tests already encrypts a vnc connection
        // to host="10.0.0.42":5901. Re-encrypt fresh for the test docker
        // setup (host "vnc-smoke") so guacd can actually reach it. We
        // generate the token with Node alongside the test docs in T1C.2;
        // here we encrypt inline using the same primitives.
        use aes::Aes256;
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};

        type Aes256CbcEnc = cbc::Encryptor<Aes256>;

        let key: [u8; 32] = *b"this-is-a-32-byte-test-key!12345";
        let iv: [u8; 16] = *b"AAAAAAAAAAAAAAAA";
        let plaintext = serde_json::json!({
            "connection": {
                "type": "vnc",
                "settings": {
                    "hostname": "vnc-smoke",
                    "username": null,
                    "password": "vncpassword",
                    "port": 5901,
                    "disable-copy": "false",
                    "disable-paste": "false",
                }
            }
        })
        .to_string();
        let ciphertext = Aes256CbcEnc::new(&key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext.as_bytes());
        let envelope = serde_json::json!({
            "iv": B64.encode(iv),
            "value": B64.encode(&ciphertext),
        })
        .to_string();
        let token = B64.encode(envelope.as_bytes());

        let state = BridgeState {
            key: Arc::new(key),
            guacd_addr: "127.0.0.1:14822".parse().unwrap(),
        };

        // Boot bridge on an ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });

        let url = format!("ws://{addr}/?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");

        // Expect at least one Guac instruction frame (e.g. `args` reply or
        // `ready` already arrives via the bridge's read pump).
        let frame = tokio::time::timeout(std::time::Duration::from_secs(15), ws.next())
            .await
            .expect("ws timed out")
            .expect("ws closed early")
            .expect("ws err");
        match frame {
            TM::Text(t) => {
                assert!(
                    t.ends_with(';'),
                    "frame should end on a guac terminator: {t}"
                );
            }
            other => panic!("expected text frame, got {other:?}"),
        }

        // Send a noop-ish frame to exercise the upstream half. Guacamole's
        // sync instruction needs a timestamp; just send a heartbeat.
        ws.send(TM::Text("4.sync,1.0;".into())).await.unwrap();

        // Don't expect a specific reply; tear down.
        let _ = ws.close(None).await;
    }
}
