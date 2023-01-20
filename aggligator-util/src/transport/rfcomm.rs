//! Bluetooth RFCOMM transport.

use std::any::Any;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hasher, Hash};
use bluer::Address;

use aggligator::control::Direction;
use tokio::sync::Mutex;
use super::{LinkTag, LinkTagBox};

static NAME: &str = "rfcomm";

/// Link tag for Bluetooth RFCOMM link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RfcommLinkTag {
    /// Local Bluetooth adapter name.
    pub adapter: String,
    /// Remote Bluetooth address.
    pub remote: Address,
    /// Link direction.
    pub direction: Direction,
}

impl fmt::Display for RfcommLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dir = match self.direction {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        };
        write!(f, "{:16} {dir} {}", &self.adapter, self.remote)
    }
}

impl RfcommLinkTag {
    /// Creates a new link tag for a Bluetooth RFCOMM link.
    pub fn new(adapter: &str, remote: Address, direction: Direction) -> Self {
        Self { adapter: adapter.to_string(), remote, direction }
    }
}


impl LinkTag for RfcommLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        self.direction
    }

    fn user_data(&self) -> Vec<u8> {
        self.adapter.as_bytes().to_vec()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }

    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

#[derive(Debug, Clone)]
pub struct RfcommConnector {
    profile_handle: Mutex<ProfileHandle>
}