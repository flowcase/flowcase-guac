//! WebSocket ↔ guacd bridge.
//!
//! Browser opens a WS to GET /vnc.html with `?guac_token=<b64>` (the
//! orchestrator-generated URL is `/desktop/<id>/vnc/vnc.html?…`; behind
//! the orchestrator's reverse proxy our bridge sees just `/vnc.html`).
//! Without WS upgrade headers the same path serves `public/vnc.html`
//! verbatim. With WS upgrade we decrypt the token (T1C.2), open guacd
//! (T1C.3), and splice bytes both ways.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::guacd::handshake;
use crate::token::decrypt_token;

/// Shared state for the bridge router. Must be cloneable and Send + 'static
/// so axum can stash it in every request.
#[derive(Clone)]
pub struct BridgeState {
    pub key: Arc<[u8; 32]>,
    pub guacd_addr: SocketAddr,
    pub public_dir: Arc<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    /// Browser uses `guac_token` in the query string (see
    /// flowcase/static/js/droplet/main.js:17). The legacy guacamole-lite
    /// also accepted `token`, so we deserialize either.
    #[serde(alias = "guac_token")]
    pub token: String,
}

/// Handler for GET /vnc.html. Branches on whether the request carries
/// WebSocket upgrade headers:
///   * upgrade present → decrypt token, hand off to guacd bridge
///   * no upgrade      → serve public/vnc.html as text/html
///
/// `Option<WebSocketUpgrade>` extracts to None when upgrade headers are
/// missing instead of rejecting the request.
pub async fn handle_vnc_html(
    upgrade: Option<WebSocketUpgrade>,
    query: Option<Query<TokenQuery>>,
    State(state): State<BridgeState>,
) -> Response {
    let Some(ws) = upgrade else {
        return serve_vnc_html(&state.public_dir).await;
    };

    let token = match query {
        Some(Query(q)) => q.token,
        None => {
            warn!("WS upgrade with no token query param");
            return ws.on_upgrade(|s| close_with(s, 4002, "Missing token"));
        }
    };

    let conn = match decrypt_token(&token, &state.key) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "rejecting WS upgrade: token decrypt failed");
            return ws.on_upgrade(|s| close_with(s, 4002, "Invalid Token"));
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

async fn close_with(mut socket: WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: std::borrow::Cow::Borrowed(reason),
        })))
        .await;
}

async fn serve_vnc_html(public_dir: &Path) -> Response {
    let path = public_dir.join("vnc.html");
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(err) => {
            warn!(?err, path=%path.display(), "vnc.html read failed");
            (StatusCode::NOT_FOUND, "vnc.html not found").into_response()
        }
    }
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
            let payload: Vec<u8> = match msg {
                Ok(Message::Text(t)) => t.into_bytes(),
                Ok(Message::Binary(b)) => b,
                Ok(Message::Close(_)) | Err(_) => break,
                _ => continue, // Ping/Pong handled by axum
            };
            if writer.write_all(&payload).await.is_err() {
                break;
            }
        }
        debug!("ws -> guacd half closed");
        let _ = writer.shutdown().await;
    });

    // guacd → browser: read raw bytes from guacd, ship as WS Text frames.
    // We use the buffered reader's underlying half via into_inner so we
    // don't waste cycles re-parsing; guacamole-common.js buffers on the
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

    tokio::select! {
        _ = to_guacd => {},
        _ = from_guacd => {},
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn dummy_state() -> (BridgeState, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = BridgeState {
            key: Arc::new(*b"this-is-a-32-byte-test-key!12345"),
            guacd_addr: "127.0.0.1:1".parse().unwrap(),
            public_dir: Arc::new(dir.path().to_path_buf()),
        };
        (state, dir)
    }

    fn app(state: BridgeState) -> Router {
        Router::new()
            .route("/vnc.html", get(handle_vnc_html))
            .with_state(state)
    }

    #[tokio::test]
    async fn plain_get_serves_vnc_html() {
        let (state, dir) = dummy_state();
        std::fs::write(dir.path().join("vnc.html"), b"<html>OK</html>").unwrap();

        let req = Request::builder()
            .method("GET")
            .uri("/vnc.html")
            .body(Body::empty())
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap()),
            Some("text/html; charset=utf-8")
        );
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(body.as_ref(), b"<html>OK</html>");
    }

    #[tokio::test]
    async fn missing_vnc_html_returns_404() {
        let (state, _dir) = dummy_state();
        let req = Request::builder()
            .method("GET")
            .uri("/vnc.html")
            .body(Body::empty())
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

        // Encrypt a fresh AES-256-CBC token in-process. Same primitive
        // and key the orchestrator uses, so a successful round-trip here
        // proves wire compatibility with droplet.py's encrypt_token.
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

        let dir = tempfile::tempdir().unwrap();
        let state = BridgeState {
            key: Arc::new(key),
            guacd_addr: "127.0.0.1:14822".parse().unwrap(),
            public_dir: Arc::new(dir.path().to_path_buf()),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });

        let url = format!("ws://{addr}/vnc.html?guac_token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");

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

        ws.send(TM::Text("4.sync,1.0;".into())).await.unwrap();
        let _ = ws.close(None).await;
    }
}
