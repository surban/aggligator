//! Bluetooth RFCOMM transport using profile for connecting.

use async_trait::async_trait;
use bluer::{
    agent::{Agent, AgentHandle},
    rfcomm::{Profile, ProfileHandle, ReqError, Role, SocketAddr},
    Adapter, AdapterEvent, Address, Session, Uuid,
};
use futures::{pin_mut, FutureExt, StreamExt};
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt, future,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch, Mutex},
    time::timeout,
};

use super::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, IoBox, LinkTag, LinkTagBox, StreamBox};
use aggligator::{control::Direction, Link};

static NAME: &str = "rfcomm_profile";

/// Link tag for Bluetooth RFCOMM profile link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RfcommProfileLinkTag {
    /// Outgoing connection.
    Outgoing(Address),
    /// Incoming connection.
    Incoming {
        /// Local RFCOMM address.
        local: SocketAddr,
        /// Remote RFCOMM address.
        remote: SocketAddr,
    },
}

impl fmt::Display for RfcommProfileLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Outgoing(remote) => write!(f, "-> {remote}"),
            Self::Incoming { local, remote } => write!(f, "{local} <- {remote}"),
        }
    }
}

impl RfcommProfileLinkTag {
    /// Creates a new outgoing link tag for a Bluetooth RFCOMM profile link.
    pub fn outgoing(remote: Address) -> Self {
        Self::Outgoing(remote)
    }

    /// Creates a new incoming link tag for a Bluetooth RFCOMM profile link.
    pub fn incoming(local: SocketAddr, remote: SocketAddr) -> Self {
        Self::Incoming { local, remote }
    }
}

impl LinkTag for RfcommProfileLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        match self {
            Self::Outgoing(_) => Direction::Outgoing,
            Self::Incoming { .. } => Direction::Incoming,
        }
    }

    fn user_data(&self) -> Vec<u8> {
        Vec::new()
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

/// Bluetooth RFCOMM transport using a profile for outgoing connections.
#[derive(Debug)]
pub struct RfcommProfileConnector {
    remote: Address,
    uuid: Uuid,
    adapter: Adapter,
    _agent_handle: AgentHandle,
    profile_handle: Mutex<ProfileHandle>,
    connected_tx: watch::Sender<bool>,
    connected_rx: watch::Receiver<bool>,
    tag: RfcommProfileLinkTag,
}

impl RfcommProfileConnector {
    /// Creates a new Bluetooth RFCOMM transport for RFCOMM connections.
    ///
    /// The transport establishes one connection to the specified RFCOMM socket address
    /// using the specified profile UUID.
    ///
    /// This uses a profile that requires no authentication and authorization.
    pub async fn new(remote: Address, uuid: Uuid) -> Result<Self> {
        let profile = Profile {
            uuid,
            role: Some(Role::Client),
            require_authentication: Some(false),
            require_authorization: Some(false),
            //auto_connect: Some(true),
            ..Default::default()
        };
        Self::custom(remote, profile, Agent::default()).await
    }

    /// Creates a new Bluetooth RFCOMM transport for RFCOMM connections using a custom profile and agent.
    ///
    /// The transport establishes one connection to the specified RFCOMM socket address.
    pub async fn custom(remote: Address, profile: Profile, agent: Agent) -> Result<Self> {
        let session = Session::new().await?;
        let adapter = session.default_adapter().await?;
        let _ = adapter.set_powered(true).await;
        let _agent_handle = session.register_agent(agent).await?;
        let uuid = profile.uuid;
        let profile_handle = session.register_profile(profile).await?;
        let (connected_tx, connected_rx) = watch::channel(false);

        Ok(Self {
            remote,
            uuid,
            adapter,
            _agent_handle,
            profile_handle: Mutex::new(profile_handle),
            connected_tx,
            connected_rx,
            tag: RfcommProfileLinkTag::outgoing(remote),
        })
    }
}

#[async_trait]
impl ConnectingTransport for RfcommProfileConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        let mut connected_rx = self.connected_rx.clone();

        let mut last_present = false;
        let mut present = false;

        loop {
            let connected = *connected_rx.borrow_and_update();

            let scan_task = async {
                if !connected {
                    tracing::debug!("performing Bluetooth discovery");
                    let mut discovery = self.adapter.discover_devices().await?;
                    while let Some(evt) = discovery.next().await {
                        match evt {
                            AdapterEvent::DeviceAdded(addr) if addr == self.remote => present = true,
                            AdapterEvent::DeviceRemoved(addr) if addr == self.remote => present = false,
                            _ => (),
                        }

                        if last_present != present {
                            if present {
                                tx.send_replace(
                                    [Box::new(self.tag.clone()) as Box<dyn LinkTag>].into_iter().collect(),
                                );
                            } else {
                                tx.send_replace(HashSet::new());
                            }
                            last_present = present;
                        }
                    }

                    Err::<(), _>(Error::new(ErrorKind::BrokenPipe, "discovery session terminated".to_string()))
                } else {
                    tracing::debug!("stopped Bluetooth discovery");
                    future::pending().await
                }
            };

            tokio::select! {
                res = scan_task => return res,
                _ = connected_rx.changed() => (),
            }
        }
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &RfcommProfileLinkTag = tag.as_any().downcast_ref().unwrap();
        let RfcommProfileLinkTag::Outgoing(remote) = tag else { unreachable!() };

        let mut hndl = self.profile_handle.lock().await;

        let dev = self.adapter.device(self.remote)?;
        let connect_task = async {
            let _ = dev.connect().await;
            dev.connect_profile(&self.uuid).await?;
            bluer::Result::Ok(())
        }
        .fuse();
        pin_mut!(connect_task);

        let stream = timeout(Duration::from_secs(15), async {
            loop {
                tokio::select! {
                    res = &mut connect_task => res?,
                    Some(req) = hndl.next() => {
                        if req.device() == *remote {
                            return req.accept();
                        } else {
                            tracing::warn!("Rejecting connect request from other Bluetooth device {}", req.device());
                            req.reject(ReqError::Rejected);
                        }
                    },
                }
            }
        })
        .await??;

        let (rh, wh) = stream.into_split();
        Ok(IoBox::new(rh, wh).into())
    }

    async fn connected_links(&self, links: &[Link<LinkTagBox>]) {
        let connected = links.iter().any(|link| &**link.tag() == (&self.tag as &dyn LinkTag));
        self.connected_tx.send_if_modified(|conn| {
            if *conn != connected {
                *conn = connected;
                true
            } else {
                false
            }
        });
    }
}

/// Bluetooth RFCOMM transport using a profile for incoming connection.
///
/// This transport is IO-stream based.
#[derive(Debug)]
pub struct RfcommProfileAcceptor {
    profile_handle: Mutex<ProfileHandle>,
    _agent_handle: AgentHandle,
}

impl RfcommProfileAcceptor {
    /// Creates a new Bluetooth RFCOMM transport for incoming connections.
    ///
    /// It registers a profile with the specified UUID that requires no authentication and authorization.
    pub async fn new(uuid: Uuid) -> Result<Self> {
        let profile = Profile {
            uuid,
            channel: Some(0),
            role: Some(Role::Server),
            require_authentication: Some(false),
            require_authorization: Some(false),
            //auto_connect: Some(true),
            ..Default::default()
        };

        Self::custom(profile, Agent::default()).await
    }

    /// Creates a new Bluetooth RFCOMM transport for incoming connections using a custom profile and agent.
    pub async fn custom(profile: Profile, agent: Agent) -> Result<Self> {
        let session = Session::new().await?;

        if let Ok(adapter) = session.default_adapter().await {
            let _ = adapter.set_powered(true).await;
            let _ = adapter.set_discoverable_timeout(0).await;
            let _ = adapter.set_discoverable(true).await;
        }

        let _agent_handle = session.register_agent(agent).await?;
        let profile_handle = session.register_profile(profile).await?;

        Ok(Self { profile_handle: Mutex::new(profile_handle), _agent_handle })
    }
}

#[async_trait]
impl AcceptingTransport for RfcommProfileAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        let mut hndl = self.profile_handle.lock().await;

        while let Some(req) = hndl.next().await {
            let Ok(stream) = req.accept() else { continue };

            let remote = stream.peer_addr()?;
            let local = stream.as_ref().local_addr()?;

            tracing::debug!("Accepted RFCOMM connection from {remote} on {local}");
            let tag = RfcommProfileLinkTag::incoming(local, remote);

            let (rh, wh) = stream.into_split();
            let _ = tx.send(AcceptedStreamBox::new(IoBox::new(rh, wh).into(), tag)).await;
        }

        Err(Error::new(ErrorKind::BrokenPipe, "profile event stream terminated".to_string()))
    }
}
