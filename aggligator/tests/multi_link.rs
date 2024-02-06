//! Multi-link tests.

use aggligator::control::DisconnectReason;
use futures::{future, join};
use std::{
    future::IntoFuture,
    iter,
    num::{NonZeroU32, NonZeroUsize},
    time::Duration,
};
use tokio::time::{sleep, timeout};

use crate::test_data::send_and_verify;
use aggligator::{
    alc::{RecvError, SendError},
    cfg::{Cfg, LinkPing},
    connect::{connect, Server},
};

mod test_channel;
mod test_data;

#[derive(Debug, Clone, Default)]
struct LinkDesc {
    cfg: test_channel::Cfg,
    pause: Option<(usize, Duration)>,
    fail: Option<usize>,
    block: Option<(usize, Duration)>,
}

async fn multi_link_test(
    link_descs: &[LinkDesc], cfg: Cfg, max_size: usize, count: usize, expected_speed: usize, should_fail: bool,
) {
    let mut server_links = Vec::new();
    let mut client_links = Vec::new();
    let mut a_controls = Vec::new();
    let mut b_controls = Vec::new();

    for ld in link_descs {
        let (link_a_tx, link_a_rx, link_a_control) = test_channel::channel(ld.cfg.clone());
        let (link_b_tx, link_b_rx, link_b_control) = test_channel::channel(ld.cfg.clone());

        server_links.push((link_a_rx, link_b_tx));
        client_links.push((link_b_rx, link_a_tx));
        a_controls.push(link_a_control);
        b_controls.push(link_b_control);
    }

    let server_cfg = cfg.clone();
    let server_task = async move {
        println!("server: starting");
        let server = Server::new(server_cfg);

        println!("server: obtaining listener");
        let mut listener = server.listen().unwrap();

        let mut added_links = Vec::new();
        for (n, (rx, tx)) in server_links.into_iter().enumerate() {
            println!("server: adding incoming link {n}");
            added_links.push(server.add_incoming(tx, rx, format!("{n}"), &[]).await.unwrap());
        }

        println!("server: getting incoming connection");
        let mut incoming = listener.next().await.unwrap();

        let link_names = incoming.link_tags();
        println!("server: links of incoming connection: {link_names:?}");
        assert_eq!(link_names.len(), added_links.len());
        for n in 0..added_links.len() {
            assert!(link_names.iter().any(|name| name.as_str() == format!("{n}")));
        }

        println!("server: accepting incoming connection");
        let (task, ch, mut control) = incoming.accept();
        let task = tokio::spawn(task.into_future());
        assert!(!control.is_terminated());

        println!("server: waiting for links");
        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        let links = control.links();
        assert_eq!(links.len(), added_links.len());
        for n in 0..added_links.len() {
            assert!(links.iter().any(|link| link.tag().as_str() == format!("{n}")));
        }
        for link in links {
            assert!(!link.is_disconnected());
            assert!(link.disconnect_reason().is_none());
        }

        println!("server: sending and receiving test data");
        let (tx, mut rx) = ch.into_tx_rx();
        println!("server: maximum send size is {}", tx.max_size());

        let expected_send_err = if should_fail { Some(SendError::AllLinksFailed) } else { None };
        let expected_recv_err = if should_fail { Some(RecvError::AllLinksFailed) } else { None };
        let speed = send_and_verify(
            "server",
            &tx,
            &mut rx,
            0,
            tx.max_size().min(max_size),
            count,
            |i| {
                for (n, desc) in link_descs.iter().enumerate() {
                    if let Some((when, dur)) = desc.pause {
                        if i == when {
                            println!("pausing link a {n}");
                            let ctrl = a_controls[n].clone();
                            tokio::spawn(async move {
                                let _ = ctrl.pause_for(dur).await;
                                println!("unpausing link a {n}");
                            });
                        }
                    }
                    if let Some(when) = desc.fail {
                        if i == when {
                            println!("failing link a {n}");
                            let ctrl = a_controls[n].clone();
                            tokio::spawn(async move { ctrl.disconnect().await });
                        }
                    }
                    if let Some((when, dur)) = desc.block {
                        if i == when {
                            println!("blocking link a {n}");
                            let link = added_links[n].clone();
                            link.set_blocked(true);
                            assert!(link.is_blocked());
                            tokio::spawn(async move {
                                sleep(dur).await;
                                println!("unblocking link a {n}");
                                link.set_blocked(false);
                                assert!(!link.is_blocked());
                            });
                        }
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

        for (n, (link, desc)) in added_links.iter().zip(link_descs).enumerate() {
            println!("server: link status {n}: {:?}", link.disconnect_reason());
            if desc.fail.is_some() {
                if !link.is_disconnected() {
                    println!("server: waiting for link {n} disconnect");
                }
                link.disconnected().await;
                match link.disconnect_reason() {
                    Some(reason) if reason.should_reconnect() => (),
                    other => panic!("no or wrong disconnect reason: {other:?}"),
                }
            } else {
                assert!(!link.is_disconnected());
            }
        }

        println!("server: dropping sender");
        drop(tx);

        if !should_fail {
            println!("server: waiting for receive end");
            assert_eq!(rx.recv().await.unwrap(), None);
        }

        println!("server: waiting for termination notification");
        let result = control.terminated().await;
        if should_fail {
            result.expect_err("control did not fail");
        } else {
            result.expect("control failed");
        }
        assert!(control.is_terminated());

        for (n, link) in added_links.iter().enumerate() {
            println!("server: waiting for link disconnect notification {n}");
            link.disconnected().await;
            let stats = link.stats();
            println!("server: Link {n} stats: {stats:?}");
        }

        println!("server: waiting for task termination");
        let result = task.await.unwrap();
        if !should_fail {
            result.expect("server task failed");
            println!("server: done");
        } else {
            let err = result.expect_err("server task did not fail");
            println!("server error: {err}");
        }
    };

    let client_task = async move {
        println!("client: starting outgoing link");
        let (task, outgoing, mut control) = connect(cfg);
        let task = tokio::spawn(task.into_future());

        let mut added_links_tasks = Vec::new();
        for (n, (rx, tx)) in client_links.into_iter().enumerate() {
            println!("client: adding outgoing link {n}");
            added_links_tasks.push(control.add(tx, rx, format!("{n}"), &[]));
        }
        let added_links = future::try_join_all(added_links_tasks).await.unwrap();

        println!("client: waiting for links");
        timeout(Duration::from_secs(1), async {
            while control.links().len() < added_links.len() {
                control.links_changed().await;
            }
        })
        .await
        .unwrap();

        println!("client: checking link info");
        let links = control.links();
        println!("client: links of outgoing connection: {links:?}");
        assert_eq!(links.len(), added_links.len());
        for n in 0..added_links.len() {
            assert!(links.iter().any(|link| link.tag().as_str() == format!("{n}")));
        }
        for link in links {
            assert!(!link.is_disconnected());
            assert!(link.disconnect_reason().is_none());
        }

        println!("client: establishing connection");
        let ch = outgoing.connect().await.unwrap();

        println!("client: sending and receiving test data");
        let (tx, mut rx) = ch.into_tx_rx();

        let expected_send_err = if should_fail { Some(SendError::AllLinksFailed) } else { None };
        let expected_recv_err = if should_fail { Some(RecvError::AllLinksFailed) } else { None };
        let speed = send_and_verify(
            "client",
            &tx,
            &mut rx,
            0,
            tx.max_size().min(max_size),
            count,
            |i| {
                for (n, desc) in link_descs.iter().enumerate() {
                    if let Some((when, dur)) = desc.pause {
                        if i == when {
                            println!("pausing link b {n}");
                            let ctrl = b_controls[n].clone();
                            tokio::spawn(async move {
                                let _ = ctrl.pause_for(dur).await;
                                println!("unpausing link b {n}");
                            });
                        }
                    }
                    if let Some(when) = desc.fail {
                        if i == when {
                            println!("failing link b {n}");
                            let ctrl = b_controls[n].clone();
                            tokio::spawn(async move { ctrl.disconnect().await });
                        }
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

        println!("client: waiting for termination notification");
        let result = control.terminated().await;
        if should_fail {
            result.expect_err("control did not fail");
        } else {
            result.expect("control failed");
        }
        assert!(control.is_terminated());

        println!("client: waiting for task termination");
        let result = task.await.unwrap();
        if !should_fail {
            result.expect("client task failed");
            println!("client: task done");
        } else {
            let err = result.expect_err("client task did not fail");
            println!("client error: {err}");
        }

        for (n, (link, desc)) in added_links.iter().zip(link_descs).enumerate() {
            println!("client: link status {n} (name: {}): {:?}", link.tag(), link.disconnect_reason());
            if desc.fail.is_some() {
                link.disconnected().await;
                if !link.is_disconnected() {
                    println!("client: waiting for link {n} disconnect");
                }
                match link.disconnect_reason() {
                    Some(reason) if reason.should_reconnect() => (),
                    Some(DisconnectReason::ConnectionClosed) => (),
                    other => panic!("no or wrong disconnect reason: {other:?}"),
                }
            }
        }

        for (n, link) in added_links.iter().enumerate() {
            println!("client: waiting for link disconnect notification {n}");
            link.disconnected().await;
            let stats = link.stats();
            println!("client: Link {n} stats: {stats:?}");
        }

        println!("client: done");
    };

    join!(server_task, client_task);
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn five_x_unlimited_multi_thread() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg { speed: 0, latency: None, ..Default::default() },
        ..Default::default()
    };
    let link_descs: Vec<_> = iter::repeat(link_desc).take(5).collect();
    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10000, 10_000_000, false).await;
}

#[test_log::test(tokio::test(flavor = "current_thread"))]
async fn five_x_unlimited_current_thread() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg { speed: 0, latency: None, ..Default::default() },
        ..Default::default()
    };
    let link_descs: Vec<_> = iter::repeat(link_desc).take(5).collect();
    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10000, 10_000_000, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn five_x_very_high_latency() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 10_000_000,
            latency: Some(Duration::from_millis(1000)),
            buffer_size: 10_000_000,
            buffer_items: 50_000,
        },
        ..Default::default()
    };
    let link_descs: Vec<_> = iter::repeat(link_desc).take(5).collect();

    let alc_cfg = Cfg {
        send_buffer: NonZeroU32::new(20_000_000).unwrap(),
        recv_buffer: NonZeroU32::new(20_000_000).unwrap(),
        send_queue: NonZeroUsize::new(50).unwrap(),
        recv_queue: NonZeroUsize::new(50).unwrap(),
        link_ack_timeout_max: Duration::from_secs(15),
        link_non_working_timeout: Duration::from_secs(30),
        link_unacked_init: NonZeroUsize::new(10_000_000).unwrap(),
        ..Default::default()
    };

    multi_link_test(&link_descs, alc_cfg, 16384, 30000, 4_000_000, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn five_x_blocked() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg { speed: 0, latency: None, ..Default::default() },
        ..Default::default()
    };
    let mut link_descs: Vec<_> = iter::repeat(link_desc).take(5).collect();

    link_descs[0].block = Some((0, Duration::from_secs(1)));
    link_descs[1].block = Some((1000, Duration::from_secs(1)));
    link_descs[2].block = Some((2000, Duration::from_secs(1)));
    link_descs[3].block = Some((5000, Duration::from_secs(1)));
    link_descs[4].block = Some((9990, Duration::from_secs(1)));

    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10000, 10_000_000, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn ten_x_hundert_kb_per_s() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 100_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 4096,
            ..Default::default()
        },
        ..Default::default()
    };
    let link_descs: Vec<_> = iter::repeat(link_desc).take(10).collect();

    let alc_cfg = Cfg { ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 100, 500_000, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn ten_x_paused_link() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc { pause: Some((n * 100, Duration::from_secs(3))), ..link_desc.clone() });
    }

    let alc_cfg = Cfg { link_retest_interval: Duration::from_secs(2), ..Default::default() };

    multi_link_test(&link_descs, alc_cfg, 16384, 10_000, 3_000_000, false).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn ten_x_failed_link() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc {
            pause: if n % 2 == 0 { Some((n * 100, Duration::from_secs(1))) } else { None },
            fail: if n != 9 { Some(n * 100 + 50) } else { None },
            ..link_desc.clone()
        });
    }

    let alc_cfg = Cfg {
        link_retest_interval: Duration::from_secs(2),
        no_link_timeout: Duration::from_secs(10),
        ..Default::default()
    };

    timeout(Duration::from_secs(60), multi_link_test(&link_descs, alc_cfg, 16384, 2_000, 500_000, false))
        .await
        .unwrap();
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn ten_x_all_failed_link() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc {
            pause: if n % 2 == 0 { Some((n * 100, Duration::from_secs(1))) } else { None },
            fail: Some(n * 100 + 50),
            ..link_desc.clone()
        });
    }

    let alc_cfg = Cfg {
        link_retest_interval: Duration::from_secs(2),
        no_link_timeout: Duration::from_secs(5),
        ..Default::default()
    };

    multi_link_test(&link_descs, alc_cfg, 16384, 2_000, 0, true).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn ten_x_link_timeout() {
    let link_desc = LinkDesc {
        cfg: test_channel::Cfg {
            speed: 1_000_000,
            latency: Some(Duration::from_millis(10)),
            buffer_size: 100_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut link_descs = Vec::new();
    for n in 0..10 {
        link_descs.push(LinkDesc {
            pause: if n != 9 { Some((n * 100, Duration::from_secs(10000))) } else { None },
            fail: if n != 9 { Some(1_000_000) } else { None },
            ..link_desc.clone()
        });
    }

    let alc_cfg = Cfg {
        link_ping_timeout: Duration::from_secs(10),
        link_non_working_timeout: Duration::from_secs(5),
        link_retest_interval: Duration::from_secs(2),
        no_link_timeout: Duration::from_secs(10),
        link_ping: LinkPing::WhenIdle(Duration::from_secs(1)),
        ..Default::default()
    };

    timeout(Duration::from_secs(60), multi_link_test(&link_descs, alc_cfg, 16384, 3_000, 0, false))
        .await
        .unwrap();
}
