//! Unique identifiers.
//!
//! All identifier are generated automatically from random numbers
//! and managed internally.
//!

use byteorder::{ByteOrder, LE};
use rand::{random, rngs::OsRng, Rng};
use std::{fmt, num::NonZeroU128, sync::Arc};
use tokio::sync::mpsc;
use x25519_dalek::SharedSecret;

/// Connection identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u128);

impl fmt::Debug for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Display for ConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl ConnId {
    /// Generates a new connection id.
    pub(crate) fn generate() -> Self {
        Self(OsRng.gen())
    }
}

/// Encrypted connection identifier.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncryptedConnId(pub u128);

impl fmt::Debug for EncryptedConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "*{:016x}*", self.0)
    }
}

impl EncryptedConnId {
    /// Encrypts the connection id with the first 16 bytes of the shared secret.
    pub fn new(id: ConnId, secret: &SharedSecret) -> Self {
        let key = LE::read_u128(secret.as_bytes());
        Self(key ^ id.0)
    }

    /// Decrypts the connection id with the first 16 bytes of the shared secret.
    pub fn decrypt(self, secret: &SharedSecret) -> ConnId {
        let key = LE::read_u128(secret.as_bytes());
        ConnId(key ^ self.0)
    }
}

/// A connection id wrapper that can report when it is dropped.
#[derive(Clone)]
pub(crate) struct OwnedConnId(Arc<OwnedConnIdInner>);

struct OwnedConnIdInner {
    id: ConnId,
    dropped_tx: Option<mpsc::UnboundedSender<ConnId>>,
}

impl fmt::Display for OwnedConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", &self.0.id)
    }
}

impl fmt::Debug for OwnedConnId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", &self.0.id)
    }
}

impl Drop for OwnedConnIdInner {
    fn drop(&mut self) {
        if let Some(dropped_tx) = self.dropped_tx.take() {
            let _ = dropped_tx.send(self.id);
        }
    }
}

impl OwnedConnId {
    /// Creates a new connection id wrapper that reports when it is dropped over the provided channel.
    pub fn new(id: ConnId, dropped_tx: mpsc::UnboundedSender<ConnId>) -> Self {
        Self(Arc::new(OwnedConnIdInner { id, dropped_tx: Some(dropped_tx) }))
    }

    /// Crates a new connection id wrapper.
    pub fn untracked(id: ConnId) -> Self {
        Self(Arc::new(OwnedConnIdInner { id, dropped_tx: None }))
    }

    /// The connection id.
    pub fn get(&self) -> ConnId {
        self.0.id
    }
}

/// Link identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkId(pub u128);

impl fmt::Debug for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl LinkId {
    /// Generates a new link id.
    pub(crate) fn generate() -> Self {
        Self(random())
    }
}

/// Server identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerId(pub NonZeroU128);

impl fmt::Debug for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl ServerId {
    /// Generates a new server id.
    pub(crate) fn generate() -> Self {
        loop {
            match random() {
                0 => (),
                id => return Self(NonZeroU128::new(id).unwrap()),
            }
        }
    }
}
