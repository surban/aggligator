//! Connection configuration.

use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use std::{
    io,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};

use crate::protocol_err;

/// Link pinging mode.
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LinkPing {
    /// Periodic with specified interval.
    Periodic(Duration),
    /// When idle for specified time.
    WhenIdle(Duration),
    /// When a previous transmission timed out.
    WhenTimedOut,
}

/// Configuration of a connection consisting of aggregated links.
///
/// For most use cases the default configuration, i.e. [`Cfg::default()`](Self::default),
/// should be used. It has proven to work well for connections with a bandwidth of
/// up to 100 MB/s.
///
/// The parameters critical to performance are the buffer sizes, in particular
/// [`send_buffer`](Self::send_buffer), [`recv_buffer`](Self::recv_buffer)
/// and [`link_unacked_limit`](Self::link_unacked_limit).
/// Thus, if the connection is under-performing, try increasing these limits.
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "dump", serde(default))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::manual_non_exhaustive)]
pub struct Cfg {
    /// The size of a data packet when sending using [stream-based IO](crate::alc::Stream).
    pub io_write_size: NonZeroUsize,
    /// Maximum number of unacknowledged sent bytes.
    pub send_buffer: NonZeroU32,
    /// Length of queue for sending data packets.
    pub send_queue: NonZeroUsize,
    /// Maximum number of unacknowledged received bytes.
    pub recv_buffer: NonZeroU32,
    /// Length of queue for received data packets.
    pub recv_queue: NonZeroUsize,
    /// Minimum timeout waiting for a packet to be acknowledged.
    pub link_ack_timeout_min: Duration,
    /// Factor to calculate acknowledgement timeout from roundtrip time.
    ///
    /// Timeout is given by current roundtrip time (ping) of the link times this factor.
    pub link_ack_timeout_roundtrip_factor: NonZeroU32,
    /// Maximum timeout waiting for a packet to be acknowledged.
    pub link_ack_timeout_max: Duration,
    /// Start value for discovering the amount of sent unacknowledged data.
    pub link_unacked_init: NonZeroUsize,
    /// Maximum amount of sent unacknowledged data per link.
    pub link_unacked_limit: NonZeroUsize,
    /// Link pinging mode.
    pub link_ping: LinkPing,
    /// Timeout for waiting for ping response, which when exceeded leads to removal of the link.
    pub link_ping_timeout: Duration,
    /// Maximum ping for a link to be usable.
    ///
    /// A link is used anyways if all links have a ping higher than the specified value.
    pub link_max_ping: Option<Duration>,
    /// Time to wait before link is tested again after a test has failed.
    pub link_retest_interval: Duration,
    /// Timeout after which a non-working link is disconnected.
    pub link_non_working_timeout: Duration,
    /// Delay before flushing a link when it has become idle.
    pub link_flush_delay: Duration,
    /// Timeout after which connection is closed when no working links are present.
    pub no_link_timeout: Duration,
    /// Timeout after which connection is forcefully closed when sender and receiver are closed.
    pub termination_timeout: Duration,
    /// Queue length for establishing connections.
    pub connect_queue: NonZeroUsize,
    /// Link speed statistics interval durations.
    pub stats_intervals: Vec<Duration>,
    #[doc(hidden)]
    pub _non_exhaustive: (),
}

impl Default for Cfg {
    /// The default configuration.
    fn default() -> Self {
        Self {
            io_write_size: NonZeroUsize::new(8_192).unwrap(),
            send_buffer: NonZeroU32::new(67_108_864).unwrap(),
            send_queue: NonZeroUsize::new(1024).unwrap(),
            recv_buffer: NonZeroU32::new(67_108_864).unwrap(),
            recv_queue: NonZeroUsize::new(1024).unwrap(),
            link_ack_timeout_min: Duration::from_secs(1),
            link_ack_timeout_roundtrip_factor: NonZeroU32::new(5).unwrap(),
            link_ack_timeout_max: Duration::from_secs(30),
            link_unacked_init: NonZeroUsize::new(8192).unwrap(),
            link_unacked_limit: NonZeroUsize::new(33_554_432).unwrap(),
            link_ping: LinkPing::WhenIdle(Duration::from_secs(15)),
            link_ping_timeout: Duration::from_secs(40),
            link_max_ping: None,
            link_retest_interval: Duration::from_secs(15),
            link_non_working_timeout: Duration::from_secs(600),
            link_flush_delay: Duration::from_millis(500),
            no_link_timeout: Duration::from_secs(90),
            termination_timeout: Duration::from_secs(300),
            connect_queue: NonZeroUsize::new(32).unwrap(),
            stats_intervals: vec![
                Duration::from_millis(100),
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(10),
            ],
            _non_exhaustive: (),
        }
    }
}

/// Link aggregator configuration exchanged with remote endpoint.
#[derive(Clone, Debug)]
pub(crate) struct ExchangedCfg {
    /// Maximum number of unacknowledged bytes.
    pub recv_buffer: NonZeroU32,
}

impl ExchangedCfg {
    pub fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        writer.write_u32::<BE>(self.recv_buffer.get())?;
        Ok(())
    }

    pub fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        let this = Self {
            recv_buffer: NonZeroU32::new(reader.read_u32::<BE>()?)
                .ok_or_else(|| protocol_err!("recv_buffer must not be zero"))?,
        };
        Ok(this)
    }
}

impl From<&Cfg> for ExchangedCfg {
    fn from(cfg: &Cfg) -> Self {
        Self { recv_buffer: cfg.recv_buffer }
    }
}
