//! Task 3 Step 4: 500 × 100KB serial vs 8-way throughput bench.
//!
//! Each file task simulates one executor download slot: 2ms of network
//! latency + 100KB payload hash + write to a `.part` file + rename — the
//! same shape as [`moodle_mcp::sync::executor`]’s concurrent downloads
//! (Semaphore 8 over the shared client governor). The serial lane does the
//! same work one file at a time.
//!
//! Run: `cargo bench --bench sync_throughput` (also runs under
//! `cargo test --bench sync_throughput`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

const FILES: usize = 500;
const SIZE: usize = 100 * 1024;
const NET_LATENCY: Duration = Duration::from_millis(2);
const CONCURRENCY: usize = 8;

fn payload(i: usize) -> Vec<u8> {
    // Deterministic 100KB payload (no RNG dependency).
    let mut buf = Vec::with_capacity(SIZE);
    let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
    while buf.len() < SIZE {
        // xorshift64.
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf.truncate(SIZE);
    buf
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

async fn one_file(dir: &std::path::Path, i: usize) {
    let bytes = payload(i);
    tokio::time::sleep(NET_LATENCY).await;
    let _ = sha256_hex(&bytes);
    let part = dir.join(format!("f{i:04}.part"));
    let dest = dir.join(format!("f{i:04}.bin"));
    tokio::fs::write(&part, &bytes).await.expect("bench write");
    tokio::fs::rename(&part, &dest).await.expect("bench rename");
}

async fn run_serial(dir: &std::path::Path) -> Duration {
    let started = Instant::now();
    for i in 0..FILES {
        one_file(dir, i).await;
    }
    started.elapsed()
}

async fn run_concurrent(dir: &std::path::Path) -> Duration {
    let started = Instant::now();
    let sem = Arc::new(Semaphore::new(CONCURRENCY));
    let mut handles = Vec::with_capacity(FILES);
    for i in 0..FILES {
        let permit_sem = Arc::clone(&sem);
        let dir = dir.to_path_buf();
        handles.push(tokio::spawn(async move {
            let _permit = permit_sem.acquire_owned().await.expect("bench sem");
            one_file(&dir, i).await;
        }));
    }
    for handle in handles {
        handle.await.expect("bench task");
    }
    started.elapsed()
}

fn bench_dir(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("moodle-mcp-bench-{name}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("bench dir");
    dir
}

#[test]
fn sync_throughput_8way_beats_serial() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let serial_dir = bench_dir("serial");
    let serial = runtime.block_on(run_serial(&serial_dir));
    let conc_dir = bench_dir("conc");
    let conc = runtime.block_on(run_concurrent(&conc_dir));
    let speedup = serial.as_secs_f64() / conc.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "sync_throughput: files={FILES} size=100KB serial={serial:?} 8-way={conc:?} speedup={speedup:.2}x"
    );
    // Theoretical ceiling is 8x; the floor below keeps CI stable under
    // scheduler jitter while still proving genuine concurrency.
    assert!(
        speedup >= 6.0,
        "8-way must beat serial by >=6x (got {speedup:.2}x)"
    );
    let _ = std::fs::remove_dir_all(&serial_dir);
    let _ = std::fs::remove_dir_all(&conc_dir);
}
