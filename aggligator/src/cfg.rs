//! Connection configuration.

use byteorder::{BE, ReadBytesExt, WriteBytesExt};
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

/// Step of the schedule for increasing the limit of sent unacknowledged data of a link.
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnackedIncrease {
    /// Minimum number of consecutive increases without buffer overrun for this step to apply.
    pub consecutive: u32,
    /// Percentage of the current limit the new limit is set to.
    ///
    /// Must be greater than 100 for the limit to actually increase.
    pub percent: u32,
}

/// Configuration of a connection consisting of aggregated links.
///
/// For most use cases the default configuration, i.e. [`Cfg::default()`](Self::default),
/// should be used. It has proven to work well for connections with a bandwidth of
/// up to 100 MB/s.
///
/// The parameters critical to performance are the buffer sizes, in particular
/// [`send_buffer`](Self::send_buffer), [`recv_buffer`](Self::recv_buffer)
/// and [`LinkCfg::unacked_limit`].
/// Thus, if the connection is under-performing, try increasing these limits.
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "dump", serde(default))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::manual_non_exhaustive)]
pub struct Cfg {
    /// The size of a data packet when sending using [stream-based IO](crate::alc::Stream).
    pub io_write_size: NonZeroUsize,
    /// Ignore explicit flush requests by [`Sender::flush`](crate::alc::Sender::flush)
    /// and the [sender sink](crate::alc::SenderSink).
    ///
    /// Automatic flushing by [`LinkCfg::flush_delay`] and [`LinkCfg::flush_interval`]
    /// is unaffected.
    pub ignore_flush: bool,
    /// Maximum number of unacknowledged sent bytes.
    pub send_buffer: NonZeroU32,
    /// Length of queue for sending data packets.
    pub send_queue: NonZeroUsize,
    /// Maximum number of unacknowledged received bytes.
    pub recv_buffer: NonZeroU32,
    /// Length of queue for received data packets.
    pub recv_queue: NonZeroUsize,
    /// Maximum factor by which highest ping may exceed lowest ping.
    pub link_max_ping_spread: Option<NonZeroU32>,
    /// Timeout after which connection is closed when no working links are present.
    pub no_link_timeout: Duration,
    /// Timeout after which connection is forcefully closed when sender and receiver are closed.
    pub termination_timeout: Duration,
    /// Queue length for establishing connections.
    pub connect_queue: NonZeroUsize,
    /// Disconnect the aggregated connection when a server id mismatch occurs while connecting a link.
    pub disconnect_on_server_id_mismatch: bool,
    /// Link speed statistics interval durations.
    pub stats_intervals: Vec<Duration>,
    /// Link-specific configuration. Can be overridden per link using
    /// [`Link::set_link_cfg`](crate::Link::set_link_cfg).
    pub link: LinkCfg,
    #[doc(hidden)]
    pub _non_exhaustive: (),
}

impl Default for Cfg {
    /// The default configuration.
    fn default() -> Self {
        Self {
            io_write_size: NonZeroUsize::new(8_192).unwrap(),
            ignore_flush: false,
            send_buffer: NonZeroU32::new(134_217_728).unwrap(),
            send_queue: NonZeroUsize::new(16).unwrap(),
            recv_buffer: NonZeroU32::new(134_217_728).unwrap(),
            recv_queue: NonZeroUsize::new(16).unwrap(),
            link_max_ping_spread: Some(NonZeroU32::new(5).unwrap()),
            no_link_timeout: Duration::from_secs(120),
            termination_timeout: Duration::from_secs(300),
            connect_queue: NonZeroUsize::new(32).unwrap(),
            disconnect_on_server_id_mismatch: true,
            stats_intervals: vec![
                Duration::from_millis(100),
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(10),
            ],
            link: LinkCfg::default(),
            _non_exhaustive: (),
        }
    }
}

/// Link-specific configuration.
///
/// For most use cases the default configuration, i.e. [`LinkCfg::default()`](Self::default),
/// should be used.
#[cfg_attr(feature = "dump", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "dump", serde(default))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(clippy::manual_non_exhaustive)]
pub struct LinkCfg {
    /// Target packet size for [IO-stream-based links](crate::io).
    pub io_packet_size: NonZeroUsize,
    /// Minimum timeout waiting for a packet to be acknowledged.
    pub ack_timeout_min: Duration,
    /// Factor to calculate acknowledgement timeout from roundtrip time.
    ///
    /// Timeout is given by current roundtrip time (ping) of the link times this factor.
    pub ack_timeout_roundtrip_factor: NonZeroU32,
    /// Additional factor applied to the acknowledgement timeout of a packet that has
    /// already been resent over another link.
    ///
    /// This avoids a packet cascading over all links in quick succession.
    pub ack_timeout_resent_factor: NonZeroU32,
    /// Additional factor applied to the acknowledgement timeout while the roundtrip time
    /// of the link has not been measured often enough to be considered reliable.
    pub ack_timeout_unreliable_factor: NonZeroU32,
    /// Maximum timeout waiting for a packet to be acknowledged.
    pub ack_timeout_max: Duration,
    /// Start value for discovering the amount of sent unacknowledged data.
    pub unacked_init: NonZeroUsize,
    /// Maximum amount of sent unacknowledged data per link.
    pub unacked_limit: NonZeroUsize,
    /// Schedule for increasing the limit of sent unacknowledged data of a link.
    ///
    /// Whenever a link is blocked by its limit and no other link is available for sending,
    /// the limit is increased. The step with the highest number of matching consecutive
    /// increases is applied; if no step matches, the limit is not increased.
    ///
    /// This does not apply when only one link is working; see
    /// [`unacked_increase_single_link`](Self::unacked_increase_single_link).
    pub unacked_increase: Vec<UnackedIncrease>,
    /// Percentage of the current limit the new limit is set to when only one link is working.
    ///
    /// Must be greater than 100 for the limit to actually increase.
    pub unacked_increase_single_link: u32,
    /// Link pinging mode.
    pub ping: LinkPing,
    /// Timeout for waiting for ping response, which when exceeded leads to removal of the link.
    pub ping_timeout: Duration,
    /// Maximum ping for a link to be usable.
    ///
    /// A link is used anyways if all links have a ping higher than the specified value.
    pub max_ping: Option<Duration>,
    /// Maximum amount of data to send to test the functionality of a link before using it.
    ///
    /// If zero, no test data is sent and a newly established link is used immediately,
    /// relying on the roundtrip time measured during the handshake.
    pub test_data_limit: usize,
    /// Test a link after an acknowledgement timeout.
    pub test_after_ack_timeout: bool,
    /// Time to wait before link is tested again after a test has failed.
    pub retest_interval: Duration,
    /// Timeout after which a non-working link is disconnected.
    pub non_working_timeout: Duration,
    /// Delay before flushing a link when it has become idle.
    pub flush_delay: Duration,
    /// Interval for flushing non-idle links.
    pub flush_interval: Option<Duration>,
    /// Maximum age of unflushed acknowledgements.
    pub ack_flush_interval: Option<Duration>,
    /// Maximum amount of sent data on a link before triggering a flush.
    pub unflushed_limit: Option<NonZeroUsize>,
    #[doc(hidden)]
    pub _non_exhaustive: (),
}

impl Default for LinkCfg {
    /// The default link-specific configuration.
    fn default() -> Self {
        Self {
            io_packet_size: NonZeroUsize::new(65_536).unwrap(),
            ack_timeout_min: Duration::from_secs(1),
            ack_timeout_roundtrip_factor: NonZeroU32::new(3).unwrap(),
            ack_timeout_resent_factor: NonZeroU32::new(3).unwrap(),
            ack_timeout_unreliable_factor: NonZeroU32::new(3).unwrap(),
            ack_timeout_max: Duration::from_secs(30),
            unacked_init: NonZeroUsize::new(8192).unwrap(),
            unacked_limit: NonZeroUsize::new(134_217_728).unwrap(),
            unacked_increase: vec![
                UnackedIncrease { consecutive: 0, percent: 101 },
                UnackedIncrease { consecutive: 10, percent: 102 },
                UnackedIncrease { consecutive: 25, percent: 105 },
                UnackedIncrease { consecutive: 50, percent: 110 },
                UnackedIncrease { consecutive: 100, percent: 120 },
            ],
            unacked_increase_single_link: 200,
            ping: LinkPing::WhenIdle(Duration::from_secs(15)),
            ping_timeout: Duration::from_secs(40),
            max_ping: None,
            test_data_limit: 65_536,
            test_after_ack_timeout: false,
            retest_interval: Duration::from_secs(3),
            non_working_timeout: Duration::from_secs(20),
            flush_delay: Duration::from_millis(50),
            flush_interval: None,
            ack_flush_interval: Some(Duration::from_millis(50)),
            unflushed_limit: Some(NonZeroUsize::new(131_072).unwrap()),
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
