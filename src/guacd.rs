//! guacd binary protocol client.
//!
//! The Guacamole protocol is line-based with `;`-terminated, comma-
//! separated, length-prefixed elements: `<char-count>.<value>` where
//! char-count is the count of Unicode codepoints in `value`. Example:
//!
//! ```text
//! 6.select,3.vnc;
//! ```
//!
//! Spec: https://guacamole.apache.org/doc/gug/guacamole-protocol-reference.html

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::debug;

use crate::token::GuacConnection;

/// Default display the bridge negotiates with guacd. Browser-side
/// guacamole-common.js will resize as needed once connected.
const DEFAULT_WIDTH: u32 = 1024;
const DEFAULT_HEIGHT: u32 = 768;
const DEFAULT_DPI: u32 = 96;

/// Encode a single Guacamole instruction.
///
/// The first element is the opcode; the rest are arguments. The output
/// always ends with the `;` instruction terminator.
pub fn encode_instruction<I, S>(elements: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    let mut first = true;
    for e in elements {
        if !first {
            out.push(',');
        }
        first = false;
        let value = e.as_ref();
        let n = value.chars().count();
        out.push_str(&n.to_string());
        out.push('.');
        out.push_str(value);
    }
    out.push(';');
    out
}

/// Parse a single complete instruction from `buf`. On success returns
/// `(elements, bytes_consumed)`. Returns `Ok(None)` if the buffer ends
/// before the `;` terminator (caller should read more).
pub fn parse_instruction(buf: &[u8]) -> Result<Option<(Vec<String>, usize)>> {
    let mut elements = Vec::new();
    let mut cursor = 0usize;

    loop {
        // Length prefix.
        let dot = match buf[cursor..].iter().position(|&b| b == b'.') {
            Some(idx) => cursor + idx,
            None => return Ok(None),
        };
        let len_str = std::str::from_utf8(&buf[cursor..dot])
            .context("instruction length prefix not utf-8")?;
        let len: usize = len_str
            .parse()
            .with_context(|| format!("instruction length prefix not an integer: {len_str:?}"))?;

        // Value: read len Unicode codepoints from after the dot.
        let value_start = dot + 1;
        let s = match std::str::from_utf8(&buf[value_start..]) {
            Ok(s) => s,
            Err(err) if err.error_len().is_none() => return Ok(None), // partial multi-byte
            Err(err) => return Err(anyhow!("instruction value not utf-8: {err}")),
        };

        let mut consumed_bytes = 0usize;
        let mut chars_seen = 0usize;
        let mut chars_iter = s.chars();
        while chars_seen < len {
            match chars_iter.next() {
                Some(c) => {
                    consumed_bytes += c.len_utf8();
                    chars_seen += 1;
                }
                None => return Ok(None), // need more bytes
            }
        }
        elements.push(s[..consumed_bytes].to_string());
        cursor = value_start + consumed_bytes;

        // Separator.
        if cursor >= buf.len() {
            return Ok(None);
        }
        match buf[cursor] {
            b',' => cursor += 1,
            b';' => return Ok(Some((elements, cursor + 1))),
            other => {
                return Err(anyhow!(
                    "expected ',' or ';' after element, got byte 0x{other:02x}"
                ));
            }
        }
    }
}

/// Reader that yields parsed instructions one at a time from a TcpStream.
pub struct GuacdReader<R> {
    inner: BufReader<R>,
    pending: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> GuacdReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
            pending: Vec::with_capacity(4096),
        }
    }

    /// Read until a complete instruction is available, then return it.
    /// Returns `Ok(None)` if EOF is reached cleanly between instructions.
    pub async fn next_instruction(&mut self) -> Result<Option<Vec<String>>> {
        loop {
            if let Some((elements, consumed)) = parse_instruction(&self.pending)? {
                self.pending.drain(..consumed);
                return Ok(Some(elements));
            }
            let mut chunk = [0u8; 4096];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Err(anyhow!(
                    "guacd closed mid-instruction; {} bytes pending",
                    self.pending.len()
                ));
            }
            self.pending.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn into_inner(self) -> BufReader<R> {
        self.inner
    }
}

/// Run the guacd handshake for `conn` over an already-connected `stream`.
/// Returns the underlying stream split into a reader/writer pair, ready
/// for bidirectional protocol traffic.
pub async fn handshake(
    stream: TcpStream,
    conn: &GuacConnection,
) -> Result<(
    GuacdReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
)> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = GuacdReader::new(reader);

    // 1. select
    let select = encode_instruction(["select", conn.kind.protocol_name()]);
    debug!(payload = %select, "guacd handshake: select");
    writer.write_all(select.as_bytes()).await?;

    // 2. args from server
    let args = reader
        .next_instruction()
        .await?
        .ok_or_else(|| anyhow!("guacd closed before sending args"))?;
    if args.first().map(String::as_str) != Some("args") {
        return Err(anyhow!(
            "expected `args` from guacd, got `{}`",
            args.first().cloned().unwrap_or_default()
        ));
    }
    debug!(?args, "guacd handshake: args received");

    // 3. size, audio, video, image
    writer
        .write_all(
            encode_instruction([
                "size",
                &DEFAULT_WIDTH.to_string(),
                &DEFAULT_HEIGHT.to_string(),
                &DEFAULT_DPI.to_string(),
            ])
            .as_bytes(),
        )
        .await?;
    writer
        .write_all(encode_instruction(["audio", "audio/L8", "audio/L16"]).as_bytes())
        .await?;
    writer
        .write_all(encode_instruction(["video"]).as_bytes())
        .await?;
    writer
        .write_all(
            encode_instruction(["image", "image/jpeg", "image/png", "image/webp"]).as_bytes(),
        )
        .await?;

    // 4. connect — value for each arg name in the order guacd sent.
    let mut connect_args: Vec<String> = Vec::with_capacity(args.len());
    connect_args.push("connect".to_string());
    for arg_name in args.iter().skip(1) {
        connect_args.push(value_for_arg(arg_name, conn));
    }
    let connect = encode_instruction(connect_args.iter().map(String::as_str));
    debug!(payload = %connect, "guacd handshake: connect");
    writer.write_all(connect.as_bytes()).await?;
    writer.flush().await?;

    // 5. ready (skip non-ready server-initiated instructions like log/sync).
    loop {
        let inst = reader
            .next_instruction()
            .await?
            .ok_or_else(|| anyhow!("guacd closed before sending ready"))?;
        match inst.first().map(String::as_str) {
            Some("ready") => {
                debug!(?inst, "guacd handshake: ready");
                break;
            }
            Some("error") => {
                return Err(anyhow!("guacd error during handshake: {:?}", inst));
            }
            _ => {
                debug!(?inst, "guacd handshake: skipping pre-ready instruction");
            }
        }
    }

    Ok((reader, writer))
}

fn value_for_arg(name: &str, conn: &GuacConnection) -> String {
    match name {
        "hostname" => conn.hostname.clone(),
        "port" => conn.port.to_string(),
        "username" => conn.username.clone().unwrap_or_default(),
        "password" => conn.password.clone().unwrap_or_default(),
        // All other args (read-only, security, recording-path, color-depth, …)
        // get their guacd-side default by sending the empty string.
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::ConnectionKind;

    fn vnc_conn() -> GuacConnection {
        GuacConnection {
            kind: ConnectionKind::Vnc,
            hostname: "10.0.0.42".to_string(),
            port: 5901,
            username: Some("kasm_user".to_string()),
            password: Some("secret".to_string()),
        }
    }

    #[test]
    fn encode_basic_instruction() {
        assert_eq!(encode_instruction(["select", "vnc"]), "6.select,3.vnc;");
        assert_eq!(encode_instruction(["video"]), "5.video;");
    }

    #[test]
    fn encode_uses_codepoint_count_not_byte_count() {
        // "café" = 4 codepoints, 5 bytes.
        let s = encode_instruction(["x", "café"]);
        assert_eq!(s, "1.x,4.café;");
    }

    #[test]
    fn parse_one_instruction() {
        let raw = b"6.select,3.vnc;leftover";
        let (elements, consumed) = parse_instruction(raw).unwrap().unwrap();
        assert_eq!(elements, vec!["select".to_string(), "vnc".to_string()]);
        assert_eq!(consumed, b"6.select,3.vnc;".len());
    }

    #[test]
    fn parse_returns_none_on_partial() {
        // "6.sele" — incomplete
        let raw = b"6.sele";
        assert!(parse_instruction(raw).unwrap().is_none());
    }

    #[test]
    fn parse_handles_multibyte_codepoints() {
        // "café" -> 4 codepoints
        let raw = "1.x,4.café;".as_bytes();
        let (elements, _) = parse_instruction(raw).unwrap().unwrap();
        assert_eq!(elements, vec!["x".to_string(), "café".to_string()]);
    }

    #[test]
    fn parse_rejects_garbage() {
        let raw = b"6.select|3.vnc;"; // '|' instead of ',' is a syntax error
        let err = parse_instruction(raw).unwrap_err();
        assert!(format!("{err}").contains("expected"));
    }

    #[test]
    fn value_for_arg_picks_known_keys() {
        let conn = vnc_conn();
        assert_eq!(value_for_arg("hostname", &conn), "10.0.0.42");
        assert_eq!(value_for_arg("port", &conn), "5901");
        assert_eq!(value_for_arg("username", &conn), "kasm_user");
        assert_eq!(value_for_arg("password", &conn), "secret");
        assert_eq!(value_for_arg("color-depth", &conn), "");
    }

    /// Live integration test against a real guacd + VNC server.
    /// Run with `cargo test -- --ignored guacd_real_handshake_against_vnc`
    /// after spinning up:
    ///   docker network create guactest
    ///   docker run -d --rm --name vnc-smoke --network guactest \
    ///       consol/ubuntu-xfce-vnc:latest
    ///   docker run -d --rm --name guacd-smoke --network guactest \
    ///       -p 14822:4822 guacamole/guacd:1.5.5
    /// Default VNC password for the consol image is `vncpassword`.
    #[tokio::test]
    #[ignore = "needs guacd + VNC docker containers; run manually"]
    async fn guacd_real_handshake_against_vnc() {
        let conn = GuacConnection {
            kind: ConnectionKind::Vnc,
            hostname: "vnc-smoke".to_string(),
            port: 5901,
            username: None,
            password: Some("vncpassword".to_string()),
        };

        let stream = TcpStream::connect("127.0.0.1:14822")
            .await
            .expect("guacd not reachable on 127.0.0.1:14822");
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(15), handshake(stream, &conn))
                .await
                .expect("handshake timed out");
        let (_reader, _writer) = result.expect("handshake should succeed");
    }

    #[tokio::test]
    async fn reader_reassembles_split_instructions() {
        // Pipe two instructions across a chunk boundary.
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut r = GuacdReader::new(reader);

        let task = tokio::spawn(async move {
            writer.write_all(b"6.select,3.").await.unwrap();
            writer.write_all(b"vnc;5.ready,1.x;").await.unwrap();
            drop(writer);
        });

        let first = r.next_instruction().await.unwrap().unwrap();
        let second = r.next_instruction().await.unwrap().unwrap();
        let third = r.next_instruction().await.unwrap();

        assert_eq!(first, vec!["select", "vnc"]);
        assert_eq!(second, vec!["ready", "x"]);
        assert_eq!(third, None); // EOF

        task.await.unwrap();
    }
}
