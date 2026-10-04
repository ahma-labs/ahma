//! The SSH key broker's building blocks (SPEC R-CRED): the agent protocol,
//! the keys it signs with, what a signature is for, and which server a
//! connection is bound to. A sandboxed command never reads a private key;
//! it asks the broker, outside the sandbox, to sign — and only for a
//! destination a human allowed.

pub mod keys;
pub mod known_hosts;
pub mod proto;
pub mod purpose;
