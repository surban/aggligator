//! Test data generator and verifier.
#![allow(dead_code)]

use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use bytes::{Buf, Bytes};
use crc32fast::hash;
use futures::join;
use rand::prelude::*;
use rand_xoshiro::SplitMix64;
use std::{
    io::{self, BufRead},
    num::Wrapping,
};

use aggligator::{
    alc::{Receiver, RecvError, SendError, Sender},
    exec::time::Instant,
};

/// Test data generator.
pub struct Generator {
    min_size: usize,
    max_size: usize,
    seq: Wrapping<u32>,
    total: usize,
}

impl Generator {
    /// Initializes a new test data generator.
    pub fn new(min_size: usize, max_size: usize) -> Self {
        assert!(max_size >= min_size);
        Self { seq: Wrapping(0), min_size, max_size, total: 0 }
    }

    /// Generates the next test packet.
    pub fn packet(&mut self) -> Bytes {
        let mut rng = SplitMix64::from_rng(&mut rand::rng());

        let size = if self.seq.0 % 20 == 0 {
            self.min_size
        } else if self.seq.0 % 10 == 0 {
            self.max_size
        } else {
            rng.random_range(self.min_size..self.max_size - 8)
        };
        let size = size.saturating_sub(8);
        let mut packet = Vec::with_capacity(size + 8);

        packet.write_u32::<BE>(self.seq.0).unwrap();
        self.seq += 1;

        for _ in 0..size {
            packet.write_u8(rng.random()).unwrap();
        }

        packet.write_u32::<BE>(hash(&packet)).unwrap();

        self.total += packet.len();
        Bytes::from(packet)
    }

    /// Sequence number of next test packet.
    pub fn seq(&self) -> u32 {
        self.seq.0
    }

    /// Total bytes generated.
    pub fn total(&self) -> usize {
        self.total
    }
}

/// Test data verifier.
pub struct Verifier {
    seq: Wrapping<u32>,
    total: usize,
}

impl Verifier {
    /// Initializes a new test data verifier.
    pub fn new() -> Self {
        Self { seq: Wrapping(0), total: 0 }
    }

    /// Verifies the provided packet.
    pub fn verify(&mut self, packet: Bytes) -> Result<(), io::Error> {
        let mut reader = packet.clone().reader();

        let seq = reader.read_u32::<BE>()?;
        if seq != self.seq.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("mismatched sequence number, expected {} but received {}", self.seq.0, seq),
            ));
        }
        self.seq += 1;

        reader.consume(packet.len() - 8);

        let cksum = reader.read_u32::<BE>()?;
        let cksum2 = hash(&packet[..packet.len() - 4]);
        if cksum != cksum2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "mismatched checksum"));
        }

        self.total += packet.len();

        Ok(())
    }

    /// Sequence number of next expected test packet.
    pub fn seq(&self) -> u32 {
        self.seq.0
    }

    /// Total bytes verified.
    pub fn total(&self) -> usize {
        self.total
    }
}

const MB: f64 = 1048576.;

/// Sends and verifies over the specified sender and receiver.
#[allow(clippy::too_many_arguments)]
pub async fn send_and_verify(
    name: &str, tx: &Sender, rx: &mut Receiver, min_size: usize, max_size: usize, count: usize,
    mut send_cb: impl FnMut(usize), expected_send_error: Option<SendError>,
    expected_recv_error: Option<RecvError>,
) -> f64 {
    let send_task = async {
        let start = Instant::now();
        let mut gen = Generator::new(min_size, max_size);

        for i in 0..count {
            if i % 100 == 0 {
                println!("{name}: sending {i}");
                tx.flush().await?;
            }
            send_cb(i);
            let data = gen.packet();
            tx.send(data).await?;
        }

        let elapsed = start.elapsed().as_secs_f64();
        let speed = gen.total() as f64 / elapsed;
        println!("{name}: sent {:.2} MB in {:.1} s => {:.1} MB/s", gen.total() as f64 / MB, elapsed, speed / MB);
        Ok(speed)
    };

    let recv_task = async {
        let start = Instant::now();
        let mut verifier = Verifier::new();

        for i in 0..count {
            if i % 100 == 0 {
                println!("{name}: recving {i}");
            }
            let data = rx.recv().await?.unwrap();
            verifier.verify(data).unwrap();
        }

        let elapsed = start.elapsed().as_secs_f64();
        let speed = verifier.total() as f64 / elapsed;
        println!(
            "{name}: received {:.2} MB in {:.1} s => {:.1} MB/s",
            verifier.total() as f64 / MB,
            elapsed,
            speed / MB
        );
        Ok(speed)
    };

    let (mut send_res, mut recv_res) = join!(send_task, recv_task);
    if let Some(err) = expected_send_error {
        assert_eq!(send_res, Err(err.clone()), "send error mismatch");
        println!("{name}: got expected error: {err:?}");
        send_res = Ok(0.);
    }
    if let Some(err) = expected_recv_error {
        assert_eq!(recv_res, Err(err.clone()), "recv error mismatch");
        println!("{name}: got expected error: {err:?}");
        recv_res = Ok(0.);
    }

    send_res.unwrap().min(recv_res.unwrap())
}
