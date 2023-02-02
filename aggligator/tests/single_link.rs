//! Single-link tests.

use aggligator::cfg::{LinkSteering, UnackedLimit};
use futures::join;
use std::{
    future::IntoFuture,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};
use tokio::time::timeout;

use crate::test_data::send_and_verify;
use aggligator::{
    alc::{RecvError, SendError},
    cfg::Cfg,
    connect::{connect, Server},
};

mod test_channel;
mod test_data;

async fn single_link_test(
    channel_cfg: test_channel::Cfg, cfg: Cfg, max_size: usize, count: usize, expected_speed: usize,
    pause: Option<(usize, Duration)>, fail_link: Option<usize>,
) {
    let (link_a_tx, link_a_rx, link_a_control) = test_channel::channel(channel_cfg.clone());
    let (link_b_tx, link_b_rx, link_b_control) = test_channel::channel(channel_cfg);

    let server_cfg = cfg.clone();
    let server_task = async move {
        println!("server: starting");
        let server = Server::new(server_cfg);

        println!("server: obtaining listener");
        let mut listener = server.listen().unwrap();

        println!("server: adding incoming link");
        let link = server.add_incoming(link_b_tx, link_a_rx, "incoming", &[]).await.unwrap();

        println!("server: getting incoming connection");
        let mut incoming = listener.next().await.unwrap();

        let link_names = incoming.link_tags();
        println!("server: links of incoming connection: {link_names:?}");
        assert_eq!(*link_names[0], "incoming");

        println!("server: accepting incoming connection");
        let (task, ch, control) = incoming.accept();
        let task = tokio::spawn(task.into_future());
        assert!(!control.is_terminated());

        let links = control.links();
        assert_eq!(*links[0].tag(), "incoming");
        assert!(!link.is_disconnected());
        assert!(link.disconnect_reason().is_none());

        println!("server: sending and receiving test data");
        let (tx, mut rx) = ch.into_tx_rx();
        println!("server: maximum send size is {}", tx.max_size());

        let expected_send_err = if fail_link.is_some() { Some(SendError::AllLinksFailed) } else { None };
        let expected_recv_err = if fail_link.is_some() { Some(RecvError::AllLinksFailed) } else { None };
        let speed = send_and_verify(
            "server",
            &tx,
            &mut rx,
            0,
            tx.max_size().min(max_size),
            count,
            |i| {
                if let Some((interval, dur)) = pause {
                    if i % interval == 0 {
                        println!("pausing link a");
                        let ctrl = link_a_control.clone();
                        tokio::spawn(async move { ctrl.pause_for(dur).await });
                    }
                }
                if let Some(when) = fail_link {
                    if i == when {
                        println!("failing link a");
                        let ctrl = link_a_control.clone();
                        tokio::spawn(async move { ctrl.disconnect().await });
                    }
                }
            },
            expected_send_err,
            expected_recv_err,
        )
        .await;

        println!("server: measured speed is {speed:.1} and expected speed is {expected_speed:.1}");
        #[cfg(not(debug_assertions))]
        assert!(speed as usize >= expected_speed, "server too slow");

        println!("server: link status: {:?}", link.disconnect_reason());
        if fail_link.is_some() {
            link.disconnected().await;
            match link.disconnect_reason() {
                Some(reason) if reason.should_reconnect() => (),
                _ => panic!("no or wrong disconnect reason"),
            }
        }

        println!("server: dropping sender");
        drop(tx);

        if fail_link.is_none() {
            println!("server: waiting for receive end");
            assert_eq!(rx.recv().await.unwrap(), None);
        }

        println!("server: waiting for termination notification");
        control.terminated().await;
        assert!(control.is_terminated());

        println!("server: waiting for link disconnect notification");
        link.disconnected().await;

        println!("server: waiting for task termination");
        task.await.unwrap();

        println!("server: done");
    };

    let client_task = async move {
        println!("client: starting outgoing link");
        let (task, outgoing, mut control) = connect(cfg);
        let task = tokio::spawn(task.into_future());

        println!("client: adding outgoing link");
        control.add(link_a_tx, link_b_rx, "outgoing", &[]).await.unwrap();

        println!("client: waiting for link");
        timeout(Duration::from_secs(1), async {
            while control.links().is_empty() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        println!("client: checking link info");
        let links = control.links();
        println!("client: links of outgoing connection: {links:?}");
        let link = links[0].clone();
        assert_eq!(*link.tag(), "outgoing");
        assert!(!link.is_disconnected());
        assert!(!control.is_terminated());

        println!("client: establishing connection");
        let ch = outgoing.connect().await.unwrap();

        println!("client: sending and receiving test data");
        let (tx, mut rx) = ch.into_tx_rx();

        let expected_send_err = if fail_link.is_some() { Some(SendError::AllLinksFailed) } else { None };
        let expected_recv_err = if fail_link.is_some() { Some(RecvError::AllLinksFailed) } else { None };
        let speed = send_and_verify(
            "client",
            &tx,
            &mut rx,
            0,
            tx.max_size().min(max_size),
            count,
            |i| {
                if let Some((interval, dur)) = pause {
                    if i % interval == 0 {
                        println!("pausing link b");
                        let ctrl = link_b_control.clone();
                        tokio::spawn(async move { ctrl.pause_for(dur).await });
                    }
                }
                if let Some(when) = fail_link {
                    if i == when {
                        println!("failing link b");
                        let ctrl = link_b_control.clone();
                        tokio::spawn(async move { ctrl.disconnect().await });
                    }
                }
            },
            expected_send_err,
            expected_recv_err,
        )
        .await;

        println!("client: measured speed is {speed:.1} and expected speed is {expected_speed:.1}");
        #[cfg(not(debug_assertions))]
        assert!(speed as usize >= expected_speed, "client too slow");

        println!("client: dropping sender");
        drop(tx);

        if fail_link.is_none() {
            println!("client: waiting for receive end");
            assert_eq!(rx.recv().await.unwrap(), None);
        }

        println!("client: waiting for termination notification");
        control.terminated().await;
        assert!(control.is_terminated());

        println!("client: waiting for task termination");
        task.await.unwrap();

        println!("client: link status: {:?}", link.disconnect_reason());
        if fail_link.is_some() {
            link.disconnected().await;
            match link.disconnect_reason() {
                Some(reason) if reason.should_reconnect() => (),
                _ => panic!("no or wrong disconnect reason"),
            }
        }

        println!("client: waiting for link disconnect notification");
        link.disconnected().await;

        println!("client: done");
    };

    join!(server_task, client_task);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn termination() {
    let ch_cfg = test_channel::Cfg { speed: 0, latency: None, ..Default::default() };
    let alc_cfg = Cfg { ..Default::default() };

    single_link_test(ch_cfg, alc_cfg, 16384, 10, 0, None, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn unlimited_multi_thread() {
    let ch_cfg = test_channel::Cfg { speed: 0, latency: None, ..Default::default() };
    let alc_cfg = Cfg { ..Default::default() };

    single_link_test(ch_cfg, alc_cfg, 16384, 10000, 10_000_000, None, None).await;
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn unlimited_current_thread() {
    let ch_cfg = test_channel::Cfg { speed: 0, latency: None, ..Default::default() };
    let alc_cfg = Cfg { ..Default::default() };

    single_link_test(ch_cfg, alc_cfg, 16384, 10000, 10_000_000, None, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn five_x_very_high_latency_unacked_limit() {
    let ch_cfg = test_channel::Cfg {
        speed: 10_000_000,
        latency: Some(Duration::from_millis(1000)),
        buffer_size: 10_000_000,
        buffer_items: 5000,
    };
    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(20_000_000).unwrap(),
        recv_buffer: NonZeroU32::new(20_000_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_steering: LinkSteering::UnackedLimit(UnackedLimit {
            init: NonZeroUsize::new(10_000_000).unwrap(),
            ..Default::default()
        }),
        link_ack_timeout_max: Duration::from_secs(10),
        ..Default::default()
    };

    single_link_test(ch_cfg, alc_cfg, 16384, 1000, 1_000_000, None, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn five_x_very_high_latency_min_roundtrip() {
    let ch_cfg = test_channel::Cfg {
        speed: 10_000_000,
        latency: Some(Duration::from_millis(1000)),
        buffer_size: 10_000_000,
        buffer_items: 5000,
    };
    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(20_000_000).unwrap(),
        recv_buffer: NonZeroU32::new(20_000_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_steering: LinkSteering::MinRoundtrip(Default::default()),
        link_ack_timeout_max: Duration::from_secs(10),
        ..Default::default()
    };

    single_link_test(ch_cfg, alc_cfg, 16384, 1000, 1_000_000, None, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn one_mb_per_s() {
    let ch_cfg = test_channel::Cfg {
        speed: 1_000_000,
        latency: Some(Duration::from_millis(10)),
        buffer_size: 100_000,
        ..Default::default()
    };
    let alc_cfg = Cfg {
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        ..Default::default()
    };

    single_link_test(ch_cfg, alc_cfg, 16384, 1000, 500_000, None, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn ten_mb_per_s() {
    let ch_cfg = test_channel::Cfg {
        speed: 10_000_000,
        latency: Some(Duration::from_millis(10)),
        buffer_size: 10_000_000,
        buffer_items: 5000,
    };
    let alc_cfg = Cfg {
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        ..Default::default()
    };

    single_link_test(ch_cfg, alc_cfg, 16384, 10000, 5_000_000, None, None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn paused_link() {
    let ch_cfg = test_channel::Cfg {
        speed: 1_000_000,
        latency: Some(Duration::from_millis(10)),
        buffer_size: 100_000,
        ..Default::default()
    };
    let alc_cfg = Cfg { link_retest_interval: Duration::from_secs(2), ..Default::default() };

    single_link_test(ch_cfg, alc_cfg, 16384, 300, 0, Some((100, Duration::from_secs(3))), None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn timed_out_link() {
    let ch_cfg = test_channel::Cfg {
        speed: 1_000_000,
        latency: Some(Duration::from_millis(10)),
        buffer_size: 100_000,
        ..Default::default()
    };
    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(10_000).unwrap(),
        recv_buffer: NonZeroU32::new(10_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_retest_interval: Duration::from_secs(2),
        link_non_working_timeout: Duration::from_secs(5),
        no_link_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    single_link_test(ch_cfg, alc_cfg, 16384, 300, 0, Some((100, Duration::from_secs(10000))), Some(1000)).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn failed_link() {
    let ch_cfg = test_channel::Cfg {
        speed: 1_000_000,
        latency: Some(Duration::from_millis(10)),
        buffer_size: 100_000,
        ..Default::default()
    };
    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(10_000).unwrap(),
        recv_buffer: NonZeroU32::new(10_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_retest_interval: Duration::from_secs(2),
        link_non_working_timeout: Duration::from_secs(5),
        no_link_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    single_link_test(ch_cfg, alc_cfg, 16384, 1000, 0, None, Some(100)).await;
}
