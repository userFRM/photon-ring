// Copyright 2026 Photon Ring Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use photon_ring::Photon;
use std::thread;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct Quote {
    symbol_id: u32,
    price: f64,
    volume: u32,
    ts_ns: u64,
}

fn main() {
    let bus = Photon::<Quote>::new(1024);
    let symbols = ["AAPL", "GOOG", "MSFT", "AMZN"];
    let n_quotes = 500_000u64;

    // --- subscribers ---
    let mut readers = Vec::new();
    for sym in &symbols {
        let mut sub = bus.subscribe(sym);
        let name = sym.to_string();
        readers.push(thread::spawn(move || {
            let mut count = 0u64;
            let mut total_price = 0.0f64;
            loop {
                match sub.try_recv() {
                    Ok(q) => {
                        total_price += q.price;
                        count += 1;
                        if count == n_quotes {
                            break;
                        }
                    }
                    Err(photon_ring::TryRecvError::Empty) => core::hint::spin_loop(),
                    Err(photon_ring::TryRecvError::Lagged { skipped }) => {
                        count += skipped;
                    }
                }
            }
            println!(
                "[{name}] received {count} quotes, avg price = {:.2}",
                total_price / count as f64
            );
        }));
    }

    // --- publishers (one per symbol) ---
    let start = Instant::now();
    let mut writers = Vec::new();
    for (i, sym) in symbols.iter().enumerate() {
        let mut pub_ = bus.publisher(sym);
        writers.push(thread::spawn(move || {
            for seq in 0..n_quotes {
                pub_.publish(Quote {
                    symbol_id: i as u32,
                    price: 100.0 + (seq as f64) * 0.01,
                    volume: 100 + (seq % 1000) as u32,
                    ts_ns: seq,
                });
            }
        }));
    }

    for w in writers {
        w.join().unwrap();
    }
    for r in readers {
        r.join().unwrap();
    }

    let elapsed = start.elapsed();
    let total = n_quotes * symbols.len() as u64;
    let rate = total as f64 / elapsed.as_secs_f64();
    println!(
        "\n{total} messages in {:.2?} = {:.1}M msg/s",
        elapsed,
        rate / 1_000_000.0
    );
}
