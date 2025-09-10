//! Protocol messages.

use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use bytes::Bytes;
use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{fmt, io, num::NonZeroU128};
use x25519_dalek::PublicKey;

use crate::{
    cfg::ExchangedCfg,
    id::{EncryptedConnId, ServerId},
    protocol_err,
    seq::Seq,
};

/// Reason for refusal of an incoming link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusedReason {
    /// The connection was closed.
    Closed,
    /// The server is not accepting new connections.
    NotListening,
    /// The incoming connection was refused.
    ConnectionRefused,
    /// The incoming link was refused by the link filter.
    LinkRefused,
}

impl RefusedReason {
    const ID_CLOSED: u8 = 1;
    const ID_NOT_LISTENING: u8 = 2;
    const ID_CONNECTION_REFUSED: u8 = 3;
    const ID_LINK_REFUSED: u8 = 4;
}

impl From<RefusedReason> for u8 {
    fn from(rr: RefusedReason) -> Self {
        match rr {
            RefusedReason::Closed => RefusedReason::ID_CLOSED,
            RefusedReason::NotListening => RefusedReason::ID_NOT_LISTENING,
            RefusedReason::ConnectionRefused => RefusedReason::ID_CONNECTION_REFUSED,
            RefusedReason::LinkRefused => RefusedReason::ID_LINK_REFUSED,
        }
    }
}

impl TryFrom<u8> for RefusedReason {
    type Error = io::Error;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            Self::ID_CLOSED => Ok(Self::Closed),
            Self::ID_NOT_LISTENING => Ok(Self::NotListening),
            Self::ID_CONNECTION_REFUSED => Ok(Self::ConnectionRefused),
            Self::ID_LINK_REFUSED => Ok(Self::LinkRefused),
            other => Err(protocol_err!("unknown refused reason {other}")),
        }
    }
}

/// Message between two LIAG endpoints.
#[derive(Debug)]
pub(crate) enum LinkMsg {
    /// Welcome message sent from server to client.
    Welcome {
        // Magic identifier "LIAG\0".
        // Protocol version of server.
        /// Flags of supported protocol extensions.
        extensions: u32,
        /// Diffie-Hellman public key for this link of server.
        public_key: PublicKey,
        /// Unique identifier of this server.
        server_id: ServerId,
        /// User-specified link data.
        user_data: Vec<u8>,
        /// Configuration of server.
        cfg: ExchangedCfg,
    },
    /// Connect message from client to server.
    Connect {
        // Magic identifier "LIAG\0".
        // Protocol version of client.
        /// Flags of supported protocol extensions.
        extensions: u32,
        /// Diffie-Hellman public key for this link of client.
        public_key: PublicKey,
        /// Unique identifier of this server.
        /// Is zero, if the endpoint does not accept incoming connections.
        server_id: Option<ServerId>,
        /// Encrypted connection identifier.
        connection_id: EncryptedConnId,
        /// Whether connection must already exist on the server.
        existing_connection: bool,
        /// User-specified link data.
        user_data: Vec<u8>,
        /// Configuration of client.
        cfg: ExchangedCfg,
    },
    /// Connection accepted by server.
    Accepted,
    /// Connection refused by server.
    Refused {
        /// Reason for refusal.
        reason: RefusedReason,
    },
    /// Echo request.
    Ping,
    /// Echo reply.
    Pong,
    /// Data.
    ///
    /// This is followed by one data packet.
    Data {
        /// Sequence number.
        seq: Seq,
    },
    /// Acknowledges data received over this link.
    Ack {
        /// Sequence that has been received on this link.
        received: Seq,
    },
    /// Notifies that received data has been consumed.
    Consumed {
        /// Sequence number.
        seq: Seq,
        /// Number of bytes consumed.
        consumed: u32,
    },
    /// No more data will be sent.
    SendFinish {
        /// Sequence number.
        seq: Seq,
    },
    /// Not interested in receiving any more data,
    /// but already sent data will still be processed.
    ReceiveClose {
        /// Sequence number.
        seq: Seq,
    },
    /// No more received data will be processed.
    ReceiveFinish {
        /// Sequence number.
        seq: Seq,
    },
    /// Test data to check link.
    TestData {
        /// Size of data.
        size: usize,
    },
    /// Set blocking of link.
    SetBlock {
        /// Whether the link is blocked.
        blocked: bool,
    },
    /// No more message will be send, but messages will be received
    /// until `Goodbye` is received.
    Goodbye,
    /// Forcefully terminate connection.
    Terminate,
}

impl LinkMsg {
    /// Protocol version.
    pub const PROTOCOL_VERSION: u8 = 4;

    /// Magic identifier.
    const MAGIC: &'static [u8; 5] = b"LIAG\0";

    const MSG_WELCOME: u8 = 1;
    const MSG_CONNECT: u8 = 2;
    const MSG_ACCEPTED: u8 = 3;
    const MSG_REFUSED: u8 = 4;
    const MSG_PING: u8 = 5;
    const MSG_PONG: u8 = 6;
    const MSG_DATA: u8 = 7;
    const MSG_ACK: u8 = 8;
    const MSG_CONSUMED: u8 = 9;
    const MSG_SEND_FINISH: u8 = 10;
    const MSG_RECEIVE_CLOSE: u8 = 11;
    const MSG_RECEIVE_FINISH: u8 = 12;
    const MSG_TEST_DATA: u8 = 13;
    const MSG_SET_BLOCK: u8 = 14;
    const MSG_GOODBYE: u8 = 15;
    const MSG_TERMINATE: u8 = 16;

    fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        match self {
            LinkMsg::Welcome { server_id, extensions, public_key, user_data, cfg } => {
                writer.write_u8(Self::MSG_WELCOME)?;
                writer.write_all(Self::MAGIC)?;
                writer.write_u8(Self::PROTOCOL_VERSION)?;
                writer.write_u32::<BE>(*extensions)?;
                writer.write_all(public_key.as_bytes())?;
                writer.write_u128::<BE>(server_id.0.get())?;
                writer.write_u16::<BE>(
                    user_data
                        .len()
                        .try_into()
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user data is too long"))?,
                )?;
                writer.write_all(user_data)?;
                cfg.write(&mut writer)?;
            }
            LinkMsg::Connect {
                extensions,
                public_key,
                server_id,
                connection_id,
                existing_connection,
                user_data,
                cfg,
            } => {
                writer.write_u8(Self::MSG_CONNECT)?;
                writer.write_all(Self::MAGIC)?;
                writer.write_u8(Self::PROTOCOL_VERSION)?;
                writer.write_u32::<BE>(*extensions)?;
                writer.write_all(public_key.as_bytes())?;
                writer.write_u128::<BE>(server_id.map(|si| si.0.get()).unwrap_or(0))?;
                writer.write_u128::<BE>(connection_id.0)?;
                writer.write_u8(*existing_connection as u8)?;
                writer.write_u16::<BE>(
                    user_data
                        .len()
                        .try_into()
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user data is too long"))?,
                )?;
                writer.write_all(user_data)?;
                cfg.write(&mut writer)?;
            }
            LinkMsg::Accepted => {
                writer.write_u8(Self::MSG_ACCEPTED)?;
            }
            LinkMsg::Refused { reason } => {
                writer.write_u8(Self::MSG_REFUSED)?;
                writer.write_u8((*reason).into())?;
            }
            LinkMsg::Ping => {
                writer.write_u8(Self::MSG_PING)?;
            }
            LinkMsg::Pong => {
                writer.write_u8(Self::MSG_PONG)?;
            }
            LinkMsg::Data { seq } => {
                writer.write_u8(Self::MSG_DATA)?;
                writer.write_u32::<BE>((*seq).into())?;
            }
            LinkMsg::Ack { received } => {
                writer.write_u8(Self::MSG_ACK)?;
                writer.write_u32::<BE>((*received).into())?;
            }
            LinkMsg::Consumed { seq, consumed } => {
                writer.write_u8(Self::MSG_CONSUMED)?;
                writer.write_u32::<BE>((*seq).into())?;
                writer.write_u32::<BE>(*consumed)?;
            }
            LinkMsg::SendFinish { seq } => {
                writer.write_u8(Self::MSG_SEND_FINISH)?;
                writer.write_u32::<BE>((*seq).into())?;
            }
            LinkMsg::ReceiveClose { seq } => {
                writer.write_u8(Self::MSG_RECEIVE_CLOSE)?;
                writer.write_u32::<BE>((*seq).into())?;
            }
            LinkMsg::ReceiveFinish { seq } => {
                writer.write_u8(Self::MSG_RECEIVE_FINISH)?;
                writer.write_u32::<BE>((*seq).into())?;
            }
            LinkMsg::TestData { size } => {
                writer.write_u8(Self::MSG_TEST_DATA)?;
                for n in 0..*size {
                    writer.write_u8(n as u8)?;
                }
            }
            LinkMsg::SetBlock { blocked } => {
                writer.write_u8(Self::MSG_SET_BLOCK)?;
                writer.write_u8(*blocked as u8)?;
            }
            LinkMsg::Goodbye => {
                writer.write_u8(Self::MSG_GOODBYE)?;
            }
            LinkMsg::Terminate => {
                writer.write_u8(Self::MSG_TERMINATE)?;
            }
        }
        Ok(())
    }

    pub fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        let msg = match reader.read_u8()? {
            Self::MSG_WELCOME => {
                let mut magic = vec![0; Self::MAGIC.len()];
                reader.read_exact(&mut magic)?;
                if magic != Self::MAGIC {
                    return Err(protocol_err!("invalid magic"));
                }
                let version = reader.read_u8()?;
                if version != Self::PROTOCOL_VERSION {
                    return Err(protocol_err!(
                        "expected protocol version {} but got {version}",
                        Self::PROTOCOL_VERSION
                    ));
                }
                Self::Welcome {
                    extensions: reader.read_u32::<BE>()?,
                    public_key: {
                        let mut buf = [0; 32];
                        reader.read_exact(&mut buf)?;
                        buf.into()
                    },
                    server_id: ServerId(
                        NonZeroU128::new(reader.read_u128::<BE>()?)
                            .ok_or_else(|| protocol_err!("server id must not be zero"))?,
                    ),
                    user_data: {
                        let len = reader.read_u16::<BE>()?;
                        let mut buf = vec![0; len.into()];
                        reader.read_exact(&mut buf)?;
                        buf
                    },
                    cfg: ExchangedCfg::read(&mut reader)?,
                }
            }
            Self::MSG_CONNECT => {
                let mut magic = vec![0; Self::MAGIC.len()];
                reader.read_exact(&mut magic)?;
                if magic != Self::MAGIC {
                    return Err(protocol_err!("invalid magic"));
                }
                let version = reader.read_u8()?;
                if version != Self::PROTOCOL_VERSION {
                    return Err(protocol_err!(
                        "expected protocol version {} but got {version}",
                        Self::PROTOCOL_VERSION
                    ));
                }
                Self::Connect {
                    extensions: reader.read_u32::<BE>()?,
                    public_key: {
                        let mut buf = [0; 32];
                        reader.read_exact(&mut buf)?;
                        buf.into()
                    },
                    server_id: NonZeroU128::new(reader.read_u128::<BE>()?).map(ServerId),
                    connection_id: EncryptedConnId(reader.read_u128::<BE>()?),
                    existing_connection: reader.read_u8()? != 0,
                    user_data: {
                        let len = reader.read_u16::<BE>()?;
                        let mut buf = vec![0; len.into()];
                        reader.read_exact(&mut buf)?;
                        buf
                    },
                    cfg: ExchangedCfg::read(&mut reader)?,
                }
            }
            Self::MSG_ACCEPTED => Self::Accepted,
            Self::MSG_REFUSED => Self::Refused { reason: RefusedReason::try_from(reader.read_u8()?)? },
            Self::MSG_PING => Self::Ping,
            Self::MSG_PONG => Self::Pong,
            Self::MSG_DATA => Self::Data { seq: reader.read_u32::<BE>()?.into() },
            Self::MSG_ACK => Self::Ack { received: reader.read_u32::<BE>()?.into() },
            Self::MSG_CONSUMED => {
                Self::Consumed { seq: reader.read_u32::<BE>()?.into(), consumed: reader.read_u32::<BE>()? }
            }
            Self::MSG_SEND_FINISH => Self::SendFinish { seq: reader.read_u32::<BE>()?.into() },
            Self::MSG_RECEIVE_CLOSE => Self::ReceiveClose { seq: reader.read_u32::<BE>()?.into() },
            Self::MSG_RECEIVE_FINISH => Self::ReceiveFinish { seq: reader.read_u32::<BE>()?.into() },
            Self::MSG_TEST_DATA => {
                // The reader is always a memory buffer.
                #[allow(clippy::unbuffered_bytes)]
                Self::TestData { size: reader.bytes().count() }
            }
            Self::MSG_SET_BLOCK => Self::SetBlock { blocked: reader.read_u8()? != 0 },
            Self::MSG_GOODBYE => Self::Goodbye,
            Self::MSG_TERMINATE => Self::Terminate,
            other => return Err(protocol_err!("invalid message id {other}")),
        };
        Ok(msg)
    }

    pub(crate) fn encode(&self) -> Bytes {
        let mut buf = Vec::with_capacity(match self {
            Self::TestData { size } => size + 16,
            _ => 16,
        });
        self.write(&mut buf).unwrap();
        buf.into()
    }

    pub async fn send<S>(&self, mut tx: S) -> Result<(), io::Error>
    where
        S: Sink<Bytes, Error = io::Error> + Unpin,
    {
        tx.send(self.encode()).await?;
        Ok(())
    }

    pub async fn recv<S>(mut rx: S) -> Result<Self, io::Error>
    where
        S: Stream<Item = Result<Bytes, io::Error>> + Unpin,
    {
        let buf = rx
            .next()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "message too short"))??;
        let msg = Self::read(buf.as_ref())?;
        Ok(msg)
    }
}

/// A reliable message.
///
/// Its reception must be acknowledged by the receiver and it will be resent if lost.
#[derive(Clone)]
pub(crate) enum ReliableMsg {
    /// Data.
    Data(Bytes),
    /// Received data was consumed.
    Consumed(u32),
    /// No more data will be sent.
    SendFinish,
    /// Not interested in receiving any more data,
    /// but already sent data will still be processed.
    ReceiveClose,
    /// No more received data will be processed.
    ReceiveFinish,
}

impl fmt::Debug for ReliableMsg {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Data(data) => write!(f, "Data({} bytes)", data.len()),
            Self::Consumed(n) => write!(f, "Consumed({n} bytes)"),
            Self::SendFinish => write!(f, "SendFinish"),
            Self::ReceiveClose => write!(f, "ReceiveClose"),
            Self::ReceiveFinish => write!(f, "ReceiveFinish"),
        }
    }
}

impl ReliableMsg {
    /// Convert to link message.
    pub(crate) fn to_link_msg(&self, seq: Seq) -> (LinkMsg, Option<Bytes>) {
        match self {
            ReliableMsg::Data(data) => (LinkMsg::Data { seq }, Some(data.clone())),
            ReliableMsg::Consumed(n) => (LinkMsg::Consumed { seq, consumed: *n }, None),
            ReliableMsg::SendFinish => (LinkMsg::SendFinish { seq }, None),
            ReliableMsg::ReceiveClose => (LinkMsg::ReceiveClose { seq }, None),
            ReliableMsg::ReceiveFinish => (LinkMsg::ReceiveFinish { seq }, None),
        }
    }

    /// Convert from link message.
    pub(crate) fn from_link_msg(msg: LinkMsg, data: Option<Bytes>) -> (Self, Seq) {
        match msg {
            LinkMsg::Data { seq } => (Self::Data(data.unwrap()), seq),
            LinkMsg::Consumed { seq, consumed } => (Self::Consumed(consumed), seq),
            LinkMsg::SendFinish { seq } => (Self::SendFinish, seq),
            LinkMsg::ReceiveClose { seq } => (Self::ReceiveClose, seq),
            LinkMsg::ReceiveFinish { seq } => (Self::ReceiveFinish, seq),
            _ => unreachable!("not a reliable link message"),
        }
    }
}
