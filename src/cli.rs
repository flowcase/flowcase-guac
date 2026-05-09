use clap::Parser;

/// Length of the AES-256-CBC key used to encrypt guac connection tokens.
/// Tokens are issued by the orchestrator at
/// flowcase/routes/droplet.py:595-614 and consumed by us at T1C.2.
pub const AES_KEY_LEN: usize = 32;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "flowcase-guac",
    version,
    about = "WebSocket-to-guacd bridge serving the Flowcase VNC viewer.",
    long_about = "Listens on :8080 for WebSocket upgrade requests carrying a \
                  base64+AES-256-CBC token in ?token=…, decrypts it, opens \
                  a connection to a local guacd on :4822, and forwards the \
                  Guacamole protocol bidirectionally. Static assets in \
                  public/ are served at the root."
)]
pub struct Cli {
    /// 32-byte AES-256-CBC key, matching the orchestrator's
    /// `GUAC_AES_KEY` environment variable. Passed positionally to mirror
    /// the legacy `node server.js $GUAC_KEY` invocation in
    /// docker-entrypoint.sh.
    pub key: String,
}

impl Cli {
    /// Coerce the CLI key to a fixed 32-byte array, returning a clap error
    /// if it isn't exactly the right length.
    pub fn key_bytes(&self) -> Result<[u8; AES_KEY_LEN], KeyLenError> {
        let bytes = self.key.as_bytes();
        if bytes.len() != AES_KEY_LEN {
            return Err(KeyLenError {
                got: bytes.len(),
                expected: AES_KEY_LEN,
            });
        }
        let mut out = [0u8; AES_KEY_LEN];
        out.copy_from_slice(bytes);
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("AES key must be {expected} bytes, got {got}")]
pub struct KeyLenError {
    pub got: usize,
    pub expected: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_invocation() {
        let cli = Cli::try_parse_from(["flowcase-guac", "0123456789abcdef0123456789abcdef"])
            .expect("32-byte key parses positionally");
        assert_eq!(cli.key.len(), 32);
    }

    #[test]
    fn key_bytes_returns_32() {
        let cli = Cli {
            key: "0123456789abcdef0123456789abcdef".to_string(),
        };
        let bytes = cli.key_bytes().expect("32-byte key fits");
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn short_key_rejected() {
        let cli = Cli {
            key: "tooshort".to_string(),
        };
        let err = cli.key_bytes().unwrap_err();
        assert_eq!(err.got, 8);
        assert_eq!(err.expected, 32);
    }

    #[test]
    fn missing_key_is_err() {
        let result = Cli::try_parse_from(["flowcase-guac"]);
        assert!(result.is_err());
    }
}
