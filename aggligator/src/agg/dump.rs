//! Dump data for performance analysis.
//!
//! The purpose of the data is to debug connection performance issues
//! and to help with the development of Aggligator.
//!
//! The data can be visualized using the `PlotDump.ipynb` script
//! from the repository.
//!

use serde::{Deserialize, Serialize};
use std::{io::Error, path::Path};
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
};

/// Link dump data for analysis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkDump {
    /// Whether the link is present.
    pub present: bool,
    /// The link id.
    pub link_id: u128,
    /// Whether the link is currently unconfirmed.
    pub unconfirmed: bool,
    /// Whether the sender is idle.
    pub tx_idle: bool,
    /// Whether the sender is being polled for readiness.
    pub tx_pending: bool,
    /// Whether the sender is being flushed.
    pub tx_flushing: bool,
    /// Whether the sender has been flushed.
    pub tx_flushed: bool,
    /// Then length of the acknowledgement queue.
    pub tx_ack_queue: usize,
    /// Amount of sent, unacknowledged data.
    pub txed_unacked_data: usize,
    /// Current limit of sent, unacknowledged data.
    pub txed_unacked_data_limit: usize,
    /// How many times `txed_unacked_data_limit` has been increased
    /// without send buffer overrun.
    pub txed_unacked_data_limit_increased_consecutively: usize,
    /// Total bytes sent.
    pub total_sent: u64,
    /// Total bytes received.
    pub total_recved: u64,
    /// Roundtrip time in milliseconds.
    pub roundtrip: f32,
}

/// Connection dump data for analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnDump {
    /// Connection id.
    pub conn_id: u128,
    /// Running time in seconds.
    pub runtime: f32,
    /// Amount of sent, unacknowledged data.
    pub txed_unacked: usize,
    /// Amount of sent, unconsumed data.
    pub txed_unconsumed: usize,
    /// Amount of sent, unconsumable data.
    pub txed_unconsumable: usize,
    /// Send buffer size.
    pub send_buffer: u32,
    /// Receive buffer size of remote endpoint.
    pub remote_receive_buffer: u32,
    /// Resend queue length.
    pub resend_queue: usize,
    /// Amount of received, unconsumable data.
    pub rxed_reliable_size: usize,
    /// Amount of received data consumed since sending last
    /// Consumed message.
    pub rxed_reliable_consumed_since_last_ack: usize,
    /// Link 0.
    pub link0: LinkDump,
    /// Link 1.
    pub link1: LinkDump,
    /// Link 2.
    pub link2: LinkDump,
    /// Link 3.
    pub link3: LinkDump,
    /// Link 4.
    pub link4: LinkDump,
    /// Link 5.
    pub link5: LinkDump,
    /// Link 6.
    pub link6: LinkDump,
    /// Link 7.
    pub link7: LinkDump,
    /// Link 8.
    pub link8: LinkDump,
    /// Link 9.
    pub link9: LinkDump,
}

/// Dumps analysis data from the channel to a JSON line file.
///
/// The file has one JSON object per line.
pub async fn dump_to_json_line_file(
    path: impl AsRef<Path>, mut rx: mpsc::Receiver<ConnDump>,
) -> Result<(), Error> {
    let file = File::create(path).await?;
    let mut writer = BufWriter::new(file);

    while let Some(dump) = rx.recv().await {
        let line = serde_json::to_vec(&dump).unwrap();
        writer.write_all(&line).await?;
        writer.write_u8(b'\n').await?;
    }

    writer.flush().await?;

    Ok(())
}
