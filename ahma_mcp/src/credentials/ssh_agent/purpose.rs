//! What a signature would be used for, read from the data a client asks the
//! agent to sign, and which server a connection is bound to. Consent is given
//! per destination (SPEC R-CRED.3, R-CRED.4), so the broker must know it from
//! something the client cannot make up: the `session-bind@openssh.com`
//! extension carries the server's host key with the server's signature over
//! the session id, and a userauth request names that same session id.

use super::keys;
use super::proto::Reader;

/// `SSH_MSG_USERAUTH_REQUEST`.
const SSH_MSG_USERAUTH_REQUEST: u8 = 50;

/// What a client wants signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// Logging in to a server over SSH (git push and fetch, `ssh host`).
    Userauth {
        session_id: Vec<u8>,
        user: String,
        /// The server's host key, when the request names it
        /// (`publickey-hostbound-v00@openssh.com`).
        host_key: Option<Vec<u8>>,
    },
    /// An `ssh-keygen -Y sign` signature (signed commits, file signatures),
    /// in a namespace such as `git` or `file`.
    Sshsig { namespace: String },
    /// Anything else. Not something any tool the broker serves sends; it may
    /// be a signature over attacker-chosen data.
    Unknown,
}

/// Classify the data of a sign request.
pub fn classify(data: &[u8]) -> Purpose {
    if let Some(rest) = data.strip_prefix(b"SSHSIG") {
        let mut r = Reader::new(rest);
        // An SSHSIG blob to sign: namespace, reserved, hash algorithm, hash.
        if let (Some(namespace), Some(_), Some(_), Some(_)) =
            (r.utf8(), r.string(), r.string(), r.string())
            && r.is_empty()
        {
            return Purpose::Sshsig {
                namespace: namespace.to_string(),
            };
        }
        return Purpose::Unknown;
    }
    userauth(data).unwrap_or(Purpose::Unknown)
}

fn userauth(data: &[u8]) -> Option<Purpose> {
    let mut r = Reader::new(data);
    let session_id = r.string()?.to_vec();
    if r.u8()? != SSH_MSG_USERAUTH_REQUEST {
        return None;
    }
    let user = r.utf8()?.to_string();
    if r.string()? != b"ssh-connection" {
        return None;
    }
    let method = r.string()?;
    let hostbound = match method {
        b"publickey" => false,
        b"publickey-hostbound-v00@openssh.com" => true,
        _ => return None,
    };
    if !r.bool()? {
        return None;
    }
    let _algorithm = r.string()?;
    let _key = r.string()?;
    let host_key = if hostbound {
        Some(r.string()?.to_vec())
    } else {
        None
    };
    r.is_empty().then_some(Purpose::Userauth {
        session_id,
        user,
        host_key,
    })
}

/// A connection's `session-bind@openssh.com`: the server's host key, the
/// session id, the server's signature over it, and whether the connection is
/// being forwarded onward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBind {
    pub host_key: Vec<u8>,
    pub session_id: Vec<u8>,
    pub forwarding: bool,
    verified: bool,
}

impl SessionBind {
    /// Parse the extension's contents and check the server's signature. A
    /// binding whose signature does not verify is not a binding: `None`.
    pub fn parse(contents: &[u8]) -> Option<Self> {
        let mut r = Reader::new(contents);
        let host_key = r.string()?.to_vec();
        let session_id = r.string()?.to_vec();
        let signature = r.string()?;
        let forwarding = r.bool()?;
        if !r.is_empty() {
            return None;
        }
        let verified = keys::verify(&host_key, &session_id, signature);
        verified.then_some(Self {
            host_key,
            session_id,
            forwarding,
            verified,
        })
    }

    /// Whether a userauth request belongs to this bound connection: same
    /// session id, and the same host key where the request names one.
    pub fn covers(&self, purpose: &Purpose) -> bool {
        match purpose {
            Purpose::Userauth {
                session_id,
                host_key,
                ..
            } => {
                self.verified
                    && *session_id == self.session_id
                    && host_key.as_ref().is_none_or(|k| *k == self.host_key)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::{fixtures::PLAIN_ED25519, parse_private_key};
    use super::super::proto::Writer;
    use super::*;

    fn userauth_data(session: &[u8], method: &str, host_key: Option<&[u8]>) -> Vec<u8> {
        let mut w = Writer::new()
            .string(session)
            .u8(SSH_MSG_USERAUTH_REQUEST)
            .string(b"git")
            .string(b"ssh-connection")
            .string(method.as_bytes())
            .u8(1)
            .string(b"ssh-ed25519")
            .string(b"client-key");
        if let Some(k) = host_key {
            w = w.string(k);
        }
        w.finish()
    }

    #[test]
    fn a_login_and_a_file_signature_are_told_apart() {
        assert_eq!(
            classify(&userauth_data(b"sid", "publickey", None)),
            Purpose::Userauth {
                session_id: b"sid".to_vec(),
                user: "git".into(),
                host_key: None
            }
        );
        assert_eq!(
            classify(&userauth_data(
                b"sid",
                "publickey-hostbound-v00@openssh.com",
                Some(b"hk")
            )),
            Purpose::Userauth {
                session_id: b"sid".to_vec(),
                user: "git".into(),
                host_key: Some(b"hk".to_vec())
            }
        );
        let mut sshsig = b"SSHSIG".to_vec();
        sshsig.extend(
            Writer::new()
                .string(b"git")
                .string(b"")
                .string(b"sha512")
                .string(&[0u8; 64])
                .finish(),
        );
        assert_eq!(
            classify(&sshsig),
            Purpose::Sshsig {
                namespace: "git".into()
            }
        );
        assert_eq!(classify(b"arbitrary bytes"), Purpose::Unknown);
        assert_eq!(
            classify(&userauth_data(b"sid", "password", None)),
            Purpose::Unknown
        );
    }

    #[test]
    fn a_binding_counts_only_when_the_server_signed_it() {
        // The fixture key stands in for a server's host key.
        let host = parse_private_key(PLAIN_ED25519).unwrap();
        let session = b"session-id-0123456789".to_vec();
        let bind = |sig: &[u8], forwarding: bool| {
            let mut w = Writer::new()
                .string(host.public_blob())
                .string(&session)
                .string(sig);
            w = w.u8(forwarding as u8);
            w.finish()
        };
        let good = host.sign(&session).unwrap();
        let bound = SessionBind::parse(&bind(&good, false)).expect("verified");
        assert!(!bound.forwarding);
        assert!(bound.covers(&classify(&userauth_data(&session, "publickey", None))));
        assert!(bound.covers(&classify(&userauth_data(
            &session,
            "publickey-hostbound-v00@openssh.com",
            Some(host.public_blob())
        ))));
        assert!(
            !bound.covers(&classify(&userauth_data(b"another", "publickey", None))),
            "a request for another session is not covered"
        );
        assert!(
            !bound.covers(&classify(&userauth_data(
                &session,
                "publickey-hostbound-v00@openssh.com",
                Some(b"another host")
            ))),
            "nor one naming another host"
        );
        assert!(SessionBind::parse(&bind(&good, true)).unwrap().forwarding);

        let forged = host.sign(b"not the session").unwrap();
        assert_eq!(SessionBind::parse(&bind(&forged, false)), None);
    }
}
