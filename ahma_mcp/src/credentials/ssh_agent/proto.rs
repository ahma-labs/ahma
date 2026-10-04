//! The SSH agent wire protocol (draft-miller-ssh-agent), the subset the broker
//! speaks: listing identities, signing, and the `session-bind@openssh.com`
//! extension. Both directions, so the same code serves sandboxed clients and
//! talks to the human's own agent.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest message either side accepts. OpenSSH's agent uses the same
/// bound; a sign request carries a session id and a key, not bulk data.
pub const MAX_MESSAGE: usize = 256 * 1024;

pub const SSH_AGENT_FAILURE: u8 = 5;
pub const SSH_AGENT_SUCCESS: u8 = 6;
pub const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
pub const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
pub const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
pub const SSH_AGENT_SIGN_RESPONSE: u8 = 14;
pub const SSH_AGENTC_EXTENSION: u8 = 27;

/// Reads the big-endian fields of one message body.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.buf.len() < n {
            return None;
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Some(head)
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    pub fn bool(&mut self) -> Option<bool> {
        self.u8().map(|b| b != 0)
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A length-prefixed byte string.
    pub fn string(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// A length-prefixed string that must be UTF-8.
    pub fn utf8(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.string()?).ok()
    }

    pub fn rest(&self) -> &'a [u8] {
        self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Builds one message body.
#[derive(Debug, Default)]
pub struct Writer(Vec<u8>);

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }

    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn string(mut self, v: &[u8]) -> Self {
        self = self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }

    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// What a client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    RequestIdentities,
    Sign {
        key_blob: Vec<u8>,
        data: Vec<u8>,
        flags: u32,
    },
    Extension {
        name: String,
        contents: Vec<u8>,
    },
    /// Any other message type: adding, removing or locking keys, smartcards.
    /// The broker refuses every one of them.
    Other(u8),
}

/// Parse one message body from a client. `None` when it is malformed.
pub fn parse_request(body: &[u8]) -> Option<Request> {
    let mut r = Reader::new(body);
    match r.u8()? {
        SSH_AGENTC_REQUEST_IDENTITIES => Some(Request::RequestIdentities),
        SSH_AGENTC_SIGN_REQUEST => {
            let key_blob = r.string()?.to_vec();
            let data = r.string()?.to_vec();
            let flags = r.u32()?;
            Some(Request::Sign {
                key_blob,
                data,
                flags,
            })
        }
        SSH_AGENTC_EXTENSION => {
            let name = r.utf8()?.to_string();
            Some(Request::Extension {
                name,
                contents: r.rest().to_vec(),
            })
        }
        other => Some(Request::Other(other)),
    }
}

/// `SSH_AGENT_FAILURE`.
pub fn failure() -> Vec<u8> {
    vec![SSH_AGENT_FAILURE]
}

/// `SSH_AGENT_SUCCESS`.
pub fn success() -> Vec<u8> {
    vec![SSH_AGENT_SUCCESS]
}

/// `SSH_AGENT_IDENTITIES_ANSWER` for `(key blob, comment)` pairs.
pub fn identities_answer(ids: &[(Vec<u8>, String)]) -> Vec<u8> {
    let mut w = Writer::new()
        .u8(SSH_AGENT_IDENTITIES_ANSWER)
        .u32(ids.len() as u32);
    for (blob, comment) in ids {
        w = w.string(blob).string(comment.as_bytes());
    }
    w.finish()
}

/// `SSH_AGENT_SIGN_RESPONSE` carrying a signature blob.
pub fn sign_response(signature_blob: &[u8]) -> Vec<u8> {
    Writer::new()
        .u8(SSH_AGENT_SIGN_RESPONSE)
        .string(signature_blob)
        .finish()
}

/// `SSH_AGENTC_SIGN_REQUEST`, as the broker sends it to the human's agent.
pub fn sign_request(key_blob: &[u8], data: &[u8], flags: u32) -> Vec<u8> {
    Writer::new()
        .u8(SSH_AGENTC_SIGN_REQUEST)
        .string(key_blob)
        .string(data)
        .u32(flags)
        .finish()
}

/// The `(key blob, comment)` pairs of an identities answer.
pub fn parse_identities_answer(body: &[u8]) -> Option<Vec<(Vec<u8>, String)>> {
    let mut r = Reader::new(body);
    if r.u8()? != SSH_AGENT_IDENTITIES_ANSWER {
        return None;
    }
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let blob = r.string()?.to_vec();
        let comment = String::from_utf8_lossy(r.string()?).into_owned();
        out.push((blob, comment));
    }
    Some(out)
}

/// The signature blob of a sign response.
pub fn parse_sign_response(body: &[u8]) -> Option<Vec<u8>> {
    let mut r = Reader::new(body);
    if r.u8()? != SSH_AGENT_SIGN_RESPONSE {
        return None;
    }
    Some(r.string()?.to_vec())
}

/// Read one framed message body. `Ok(None)` at a clean end of stream.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("agent message of {len} bytes"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Write one framed message body.
pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        assert_eq!(
            parse_request(&[SSH_AGENTC_REQUEST_IDENTITIES]),
            Some(Request::RequestIdentities)
        );
        let sign = sign_request(b"blob", b"data", 2);
        assert_eq!(
            parse_request(&sign),
            Some(Request::Sign {
                key_blob: b"blob".to_vec(),
                data: b"data".to_vec(),
                flags: 2
            })
        );
        let ext = Writer::new()
            .u8(SSH_AGENTC_EXTENSION)
            .string(b"session-bind@openssh.com")
            .string(b"x")
            .finish();
        assert_eq!(
            parse_request(&ext),
            Some(Request::Extension {
                name: "session-bind@openssh.com".into(),
                contents: Writer::new().string(b"x").finish()
            })
        );
        // ADD_IDENTITY (17) and LOCK (22) are refused by type, not parsed.
        assert_eq!(parse_request(&[17, 0, 0]), Some(Request::Other(17)));
        assert_eq!(parse_request(&[22]), Some(Request::Other(22)));
    }

    #[test]
    fn truncated_messages_do_not_parse() {
        assert_eq!(parse_request(&[]), None);
        assert_eq!(
            parse_request(&[SSH_AGENTC_SIGN_REQUEST, 0, 0, 0, 9, 1]),
            None
        );
        assert_eq!(
            parse_identities_answer(&[SSH_AGENT_IDENTITIES_ANSWER, 0, 0, 0, 2]),
            None
        );
    }

    #[test]
    fn answers_round_trip() {
        let ids = vec![
            (b"k1".to_vec(), "one".to_string()),
            (b"k2".to_vec(), "two".to_string()),
        ];
        assert_eq!(parse_identities_answer(&identities_answer(&ids)), Some(ids));
        assert_eq!(
            parse_sign_response(&sign_response(b"sig")),
            Some(b"sig".to_vec())
        );
        assert_eq!(parse_sign_response(&failure()), None);
    }

    #[tokio::test]
    async fn framing_round_trips_and_bounds_the_length() {
        let mut buf = Vec::new();
        write_message(&mut buf, b"hello").await.unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_message(&mut r).await.unwrap(), Some(b"hello".to_vec()));
        assert_eq!(read_message(&mut r).await.unwrap(), None, "clean end");

        let huge = ((MAX_MESSAGE + 1) as u32).to_be_bytes();
        let mut r = &huge[..];
        assert!(read_message(&mut r).await.is_err());
        let empty = 0u32.to_be_bytes();
        let mut r = &empty[..];
        assert!(read_message(&mut r).await.is_err());
    }
}
