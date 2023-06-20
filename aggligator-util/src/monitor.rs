//! Interactive connection and link monitor.

use crossterm::{
    cursor,
    cursor::{MoveTo, MoveToColumn, MoveToNextLine},
    event::{poll, read, Event, KeyCode, KeyEvent},
    execute, queue,
    style::{Print, Stylize},
    terminal,
    terminal::{disable_raw_mode, enable_raw_mode, ClearType},
};
use futures::{future, stream::FuturesUnordered, FutureExt, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, Write},
    hash::Hash,
    io::{stdout, Error},
    time::Duration,
};
use tokio::sync::{broadcast, broadcast::error::TryRecvError, watch};

use crate::transport::{ConnectingTransport, LinkError, LinkTagBox};
use aggligator::{control::Control, id::ConnId};

/// Watches the available tags of the specified transports.
///
/// The output of this function can be passed as `tags_rx` to [`interactive_monitor`].
pub fn watch_tags(
    transports: impl IntoIterator<Item = Box<dyn ConnectingTransport>>,
) -> watch::Receiver<HashSet<LinkTagBox>> {
    let (tags_tx, tags_rx) = watch::channel(HashSet::new());

    // Start tag getting task for each transport.
    let mut transport_tasks = FuturesUnordered::new();
    let mut transport_tags: Vec<watch::Receiver<HashSet<LinkTagBox>>> = Vec::new();
    for transport in transports {
        let (tx, rx) = watch::channel(HashSet::new());
        transport_tags.push(rx);
        transport_tasks.push(async move { transport.link_tags(tx).await });
    }

    tokio::spawn(async move {
        loop {
            // Remove channels from terminated transports.
            transport_tags.retain(|tt| tt.has_changed().is_ok());

            // Collect and publish tags from all transports.
            let mut all_tags = HashSet::new();
            for tt in &mut transport_tags {
                let tags = tt.borrow_and_update();
                for tag in &*tags {
                    all_tags.insert(tag.clone());
                }
            }
            tags_tx.send_if_modified(|tags| {
                if *tags == all_tags {
                    false
                } else {
                    *tags = all_tags;
                    true
                }
            });

            // Quit when no transports are left.
            if transport_tags.is_empty() {
                break;
            }

            // Monitor all transport tags for changes.
            let tags_changed = future::select_all(transport_tags.iter_mut().map(|tt| tt.changed().boxed()));

            // Wait for changes.
            tokio::select! {
                _ = tags_changed => (),
                Some(_) = transport_tasks.next() => (),
                () = tags_tx.closed() => break,
            };
        }
    });

    tags_rx
}

/// Runs the interactive connection and link monitor.
///
/// The channel `header_rx` is used to receive and update the header line to display on top of the screen.
///
/// The channel `control_rx` is used to receive newly established connections
/// that should be displayed. Terminated connections are removed automatically.
///
/// `time_stats_idx` specifies the index of the time interval
/// in [`Cfg::stats_intervals`](aggligator::cfg::Cfg::stats_intervals)
/// to use for displaying the link statistics.
///
/// The optional channel `tags_rx` is used to receive available link tags that should
/// be displayed even if no link is using them.
///
/// The optional channel `tag_error_rx` is used to receive error messages from failed
/// connection attempts that should be displayed.
///
/// The optional channel `disabled_tags_tx` is used to send the set of link tags
/// disabled interactively by the user. If not present, the user cannot disable link tags.
///
/// This function returns when the channel `control_rx` is closed or the user presses `q`.
pub fn interactive_monitor<TX, RX, TAG>(
    mut header_rx: watch::Receiver<String>, mut control_rx: broadcast::Receiver<(Control<TX, RX, TAG>, String)>,
    time_stats_idx: usize, mut tags_rx: Option<watch::Receiver<HashSet<TAG>>>,
    mut tag_error_rx: Option<broadcast::Receiver<LinkError<TAG>>>,
    disabled_tags_tx: Option<watch::Sender<HashSet<TAG>>>,
) -> Result<(), Error>
where
    TAG: Display + Hash + PartialEq + Eq + Clone + 'static,
{
    const STATS_COL: u16 = 35;

    let mut controls: Vec<(Control<TX, RX, TAG>, String)> = Vec::new();
    let mut errors: HashMap<(ConnId, TAG), String> = HashMap::new();
    let mut disabled: HashSet<TAG> = HashSet::new();
    let mut toggle_link_block: Option<usize> = None;
    let mut interval = Duration::from_secs(3);

    enable_raw_mode()?;

    'main: loop {
        // Update data.
        controls.retain(|c| !c.0.is_terminated());
        loop {
            match control_rx.try_recv() {
                Ok(control_info) => {
                    if controls.iter().all(|c| c.0.id() != control_info.0.id()) {
                        interval = control_info.0.cfg().stats_intervals[time_stats_idx];
                        controls.push(control_info);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) if controls.is_empty() => break 'main,
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => tracing::warn!("monitor lost incoming connection"),
            }
        }
        if let Some(tag_error_rx) = tag_error_rx.as_mut() {
            while let Ok(LinkError { id, tag, error }) = tag_error_rx.try_recv() {
                if let Some(id) = id {
                    errors.insert((id, tag), error.to_string());
                }
            }
        }
        if let Some(disabled_tags) = disabled_tags_tx.as_ref() {
            disabled_tags.send_replace(disabled.clone());
        }
        let mut tags: Option<Vec<_>> =
            tags_rx.as_mut().map(|rx| rx.borrow_and_update().clone().into_iter().collect());
        if let Some(tags) = &mut tags {
            tags.sort_by_key(|tag| tag.to_string());
        }

        // Clear display.
        execute!(stdout(), terminal::Clear(ClearType::All), cursor::MoveTo(0, 0)).unwrap();
        let (_cols, rows) = terminal::size().unwrap();

        // Header.
        {
            let header = header_rx.borrow_and_update();
            queue!(stdout(), Print(&*header), MoveToNextLine(1)).unwrap();
        }
        queue!(stdout(), Print("━".repeat(80).dark_grey()), MoveToNextLine(1)).unwrap();
        queue!(
            stdout(),
            MoveToColumn(STATS_COL),
            Print("  TX speed    RX speed      TXed      RXed"),
            MoveToNextLine(1)
        )
        .unwrap();
        queue!(stdout(), Print("━".repeat(80).dark_grey()), MoveToNextLine(2)).unwrap();

        // Connections.
        for (control, info) in &controls {
            // Display:
            // conn_id - age - total speeds - total data
            //   tag num - tag name - enabled/disabled - connected or error
            //   current speeds - ping - txed unacked/limit - total data

            let conn_id = control.id();

            // Sort links by tags.
            let links = control.links();
            let tag_links: Vec<_> = match &tags {
                Some(tags) => {
                    let mut tag_links: Vec<_> =
                        tags.iter().map(|tag| (tag, links.iter().find(|link| link.tag() == tag))).collect();
                    for link in &links {
                        if !tag_links.iter().any(|(tag, _)| *tag == link.tag()) {
                            tag_links.push((link.tag(), Some(link)));
                        }
                    }
                    tag_links
                }
                None => links.iter().map(|link| (link.tag(), Some(link))).collect(),
            };

            // Calculate connection totals and disconnect disabled links.
            let mut conn_sent = 0;
            let mut conn_recved = 0;
            let mut conn_tx_speed = 0.;
            let mut conn_rx_speed = 0.;
            for link in &links {
                let stats = link.stats();
                conn_sent += stats.total_sent;
                conn_recved += stats.total_recved;
                if let Some(ts) = stats.time_stats.get(time_stats_idx) {
                    conn_tx_speed += ts.send_speed();
                    conn_rx_speed += ts.recv_speed();
                }

                if disabled.contains(link.tag()) {
                    link.start_disconnect();
                }
            }

            // Connection lines.
            let stats = control.stats();
            let mut short_id = conn_id.to_string();
            short_id.truncate(8);
            queue!(
                stdout(),
                Print("Connection ".grey()),
                Print(short_id.bold().blue()),
                Print("  "),
                Print(format_duration(stats.established.map(|e| e.elapsed()).unwrap_or_default())),
                MoveToColumn(STATS_COL),
                Print(format_speed(conn_tx_speed)),
                Print(" "),
                Print(format_speed(conn_rx_speed)),
                Print("   "),
                Print(format_bytes(conn_sent)),
                Print(" "),
                Print(format_bytes(conn_recved)),
                MoveToNextLine(1),
            )
            .unwrap();
            queue!(
                stdout(),
                Print("TX:".grey()),
                Print("  avail ".grey()),
                Print(format_bytes(stats.send_space as _)),
                Print("   unack ".grey()),
                Print(format_bytes(stats.sent_unacked as _)),
                Print("   uncsmable ".grey()),
                Print(format_bytes(stats.sent_unconsumable as _)),
                Print("   uncsmed ".grey()),
                Print(format_bytes(stats.sent_unconsumed as _)),
                MoveToNextLine(1),
                Print("RX:".grey()),
                MoveToColumn(62),
                Print(" uncsmed ".grey()),
                Print(format_bytes(stats.recved_unconsumed as _)),
                MoveToNextLine(1),
            )
            .unwrap();
            if !info.is_empty() {
                queue!(stdout(), Print(info), MoveToNextLine(1)).unwrap();
            }
            queue!(stdout(), MoveToNextLine(1)).unwrap();

            // Link lines for connection.
            for (n, (tag, link)) in tag_links.iter().enumerate() {
                queue!(
                    stdout(),
                    Print("  "),
                    Print(format!("{}{}", format!("{n:1}").white(), ". ".grey())),
                    Print(format!("{:<66}", tag.to_string()).cyan()),
                    Print(
                        format!(
                            " {:>8}",
                            link.map(|l| String::from_utf8_lossy(l.remote_user_data()).to_string())
                                .unwrap_or_default()
                                .chars()
                                .take(8)
                                .collect::<String>()
                        )
                        .cyan()
                    ),
                    MoveToNextLine(1),
                    Print("     "),
                )
                .unwrap();

                if disabled.contains(tag) {
                    queue!(stdout(), Print("disabled".dark_red())).unwrap();
                } else if let Some(link) = link {
                    let stats = link.stats();
                    match (link.not_working_reason(), link.not_working_since()) {
                        (Some(reason), Some(since)) => {
                            queue!(
                                stdout(),
                                Print("unconfirmed ".yellow()),
                                Print(format_duration(since.elapsed())),
                                Print(": ".dark_grey()),
                                Print(reason.to_string().white())
                            )
                            .unwrap();
                        }
                        _ => queue!(
                            stdout(),
                            Print("connected ".green()),
                            Print(format_duration(stats.established.elapsed())),
                        )
                        .unwrap(),
                    }

                    if toggle_link_block == Some(n) {
                        link.set_blocked(!link.is_blocked());
                    }

                    if link.is_blocked() {
                        queue!(stdout(), Print(" blocked".dark_red())).unwrap();
                    } else if link.is_remotely_blocked() {
                        queue!(stdout(), Print(" remotely blocked".dark_red())).unwrap();
                    }

                    let hangs = link.stats().hangs;
                    if hangs > 0 {
                        queue!(stdout(), Print(format!(" ({hangs})").grey())).unwrap();
                    }
                } else if let Some(err) = errors.get(&(conn_id, (*tag).clone())) {
                    queue!(stdout(), Print(format!("{err:40}").red())).unwrap();
                }
                queue!(stdout(), MoveToNextLine(1)).unwrap();

                if let Some(link) = link {
                    let stats = link.stats();

                    let mut tx_speed = 0.;
                    let mut rx_speed = 0.;
                    if let Some(ts) = stats.time_stats.get(time_stats_idx) {
                        tx_speed = ts.send_speed();
                        rx_speed = ts.recv_speed();
                    }

                    queue!(
                        stdout(),
                        Print("    "),
                        Print(format!(
                            "{} {}",
                            format!("{:4}", stats.roundtrip.as_millis()).white(),
                            "ms".dark_grey()
                        )),
                        Print(" "),
                        Print(format_bytes(stats.sent_unacked)),
                        Print(" /".grey()),
                        Print(format_bytes(stats.unacked_limit)),
                        MoveToColumn(STATS_COL),
                        Print(format_speed(tx_speed)),
                        Print(" "),
                        Print(format_speed(rx_speed)),
                        Print("   "),
                        Print(format_bytes(stats.total_sent)),
                        Print(" "),
                        Print(format_bytes(stats.total_recved)),
                        MoveToNextLine(2),
                    )
                    .unwrap();
                } else {
                    queue!(stdout(), MoveToNextLine(1)).unwrap();
                }
            }

            // Seperation line.
            queue!(stdout(), MoveToNextLine(1), Print("━".repeat(80).dark_grey()), MoveToNextLine(2)).unwrap();
        }

        // Usage line.
        execute!(
            stdout(),
            MoveTo(0, rows - 2),
            Print("Press 0-9 to toggle a link, q to quit.".grey()),
            MoveToNextLine(1)
        )
        .unwrap();

        // Handle user events.
        toggle_link_block = None;
        if poll(interval)? {
            match read()? {
                Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) if c.is_ascii_digit() => {
                    let n = c.to_digit(10).unwrap();
                    if disabled_tags_tx.is_some() {
                        if let Some(tag) = tags.and_then(|tags| tags.get(n as usize).cloned()) {
                            if !disabled.remove(&tag) {
                                disabled.insert(tag);
                            }
                        }
                    } else {
                        toggle_link_block = Some(n as usize);
                    }
                }
                Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }) => break,
                _ => (),
            }
        }
    }

    disable_raw_mode()?;
    Ok(())
}

const KB: u64 = 1024;
const MB: u64 = KB * KB;
const GB: u64 = MB * KB;
const TB: u64 = GB * KB;

/// Formats a byte count.
pub fn format_bytes(bytes: u64) -> String {
    let (factor, unit, n) = if bytes >= TB {
        (TB, "TB", 1)
    } else if bytes >= GB {
        (GB, "GB", 1)
    } else if bytes >= MB {
        (MB, "MB", 1)
    } else if bytes >= KB {
        (KB, "KB", 1)
    } else {
        (1, "B ", 0)
    };

    format!("{} {}", format!("{:6.n$}", bytes as f32 / factor as f32, n = n).white(), unit.dark_grey())
}

/// Formats a speed.
pub fn format_speed(speed: f64) -> String {
    let (factor, unit, n) = if speed >= TB as f64 {
        (TB, "TB/s", 1)
    } else if speed >= GB as f64 {
        (GB, "GB/s", 1)
    } else if speed >= MB as f64 {
        (MB, "MB/s", 1)
    } else if speed >= KB as f64 {
        (KB, "KB/s", 1)
    } else {
        (1, "B/s ", 0)
    };

    format!("{} {}", format!("{:6.n$}", speed / factor as f64, n = n).white(), unit.dark_grey())
}

/// Formats a duration.
pub fn format_duration(dur: Duration) -> String {
    let mut time = dur.as_secs();
    let hours = time / 3600;
    time -= hours * 3600;
    let minutes = time / 60;
    time -= minutes * 60;
    let seconds = time;

    let mut output = String::new();

    if hours > 0 {
        write!(output, "{}{}", format!("{hours:2}").white(), "h".dark_grey()).unwrap();
    } else {
        write!(output, "   ").unwrap();
    }

    if hours > 0 || minutes > 0 {
        write!(output, "{}{}", format!("{minutes:2}").white(), "m".dark_grey()).unwrap();
    } else {
        write!(output, "   ").unwrap();
    }

    write!(output, "{}{}", format!("{seconds:2}").white(), "s".dark_grey()).unwrap();

    output
}
