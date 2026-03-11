use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use serde::Deserialize;
use std::env;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const VSOCK_PORT: u32 = 5001;
const ENCLAVE_CPU_COUNT: u32 = 2;
const ENCLAVE_MEMORY_MB: u32 = 256;

const DEFAULT_TOTAL_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
const DEFAULT_BUFFER_SIZE: u64 = 64 * 1024; // 64 KiB

#[derive(Debug, Deserialize)]
struct EnclaveRunOutput {
    #[serde(rename = "EnclaveCID")]
    enclave_cid: u32,
    #[serde(rename = "EnclaveID")]
    enclave_id: String,
}

struct BenchConfig {
    eif_path: String,
    total_bytes: u64,
    buffer_sizes: Vec<u64>,
    reverse: bool,
}

struct BenchResult {
    direction: String,
    buffer_size: u64,
    total_bytes: u64,
    duration: Duration,
}

impl BenchResult {
    fn mb_per_sec(&self) -> f64 {
        self.total_bytes as f64 / 1_000_000.0 / self.duration.as_secs_f64()
    }

    fn mib_per_sec(&self) -> f64 {
        self.total_bytes as f64 / (1024.0 * 1024.0) / self.duration.as_secs_f64()
    }
}

// --- Enclave lifecycle ---

fn run_enclave(eif_path: &str) -> Result<EnclaveRunOutput, Box<dyn std::error::Error>> {
    eprintln!("[bench] Starting enclave from: {}", eif_path);

    let _ = Command::new("nitro-cli")
        .args(["terminate-enclave", "--all"])
        .output();

    let output = Command::new("nitro-cli")
        .args([
            "run-enclave",
            "--eif-path",
            eif_path,
            "--cpu-count",
            &ENCLAVE_CPU_COUNT.to_string(),
            "--memory",
            &ENCLAVE_MEMORY_MB.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Failed to run enclave: {}", stderr).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let info: EnclaveRunOutput = serde_json::from_str(&stdout)?;
    eprintln!("[bench] Enclave CID: {}, ID: {}", info.enclave_cid, info.enclave_id);

    Ok(info)
}

fn terminate_enclave(enclave_id: &str) {
    eprintln!("[bench] Terminating enclave: {}", enclave_id);
    let _ = Command::new("nitro-cli")
        .args(["terminate-enclave", "--enclave-id", enclave_id])
        .output();
}

fn connect_to_enclave(cid: u32) -> Result<UnixStream, Box<dyn std::error::Error>> {
    let max_retries = 20;
    for attempt in 1..=max_retries {
        let sock_fd = socket(
            AddressFamily::Vsock,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )?;

        let addr = VsockAddr::new(cid, VSOCK_PORT);
        match connect(sock_fd.as_raw_fd(), &addr) {
            Ok(()) => {
                eprintln!("[bench] Connected on attempt {}", attempt);
                return Ok(UnixStream::from(sock_fd));
            }
            Err(e) => {
                if attempt < max_retries {
                    thread::sleep(Duration::from_millis(500));
                } else {
                    return Err(format!("Failed to connect after {} attempts: {}", max_retries, e).into());
                }
            }
        }
    }
    unreachable!()
}

// --- Data transfer ---

fn send_data(stream: &mut UnixStream, total_bytes: u64, buffer_size: u64) -> std::io::Result<()> {
    let buf_len = buffer_size as usize;
    let buf = vec![0u8; buf_len];
    let mut sent: u64 = 0;

    while sent < total_bytes {
        let remaining = (total_bytes - sent) as usize;
        let to_write = remaining.min(buf_len);
        stream.write_all(&buf[..to_write])?;
        sent += to_write as u64;
    }

    Ok(())
}

fn receive_data(stream: &mut UnixStream, total_bytes: u64, buffer_size: u64) -> std::io::Result<u64> {
    let buf_len = buffer_size as usize;
    let mut buf = vec![0u8; buf_len];
    let mut received: u64 = 0;

    while received < total_bytes {
        let remaining = (total_bytes - received) as usize;
        let to_read = remaining.min(buf_len);
        let n = stream.read(&mut buf[..to_read])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Connection closed",
            ));
        }
        received += n as u64;
    }

    Ok(received)
}

// --- Single benchmark run ---

fn run_single_bench(
    eif_path: &str,
    total_bytes: u64,
    buffer_size: u64,
    reverse: bool,
) -> Result<Vec<BenchResult>, Box<dyn std::error::Error>> {
    let enclave = run_enclave(eif_path)?;

    thread::sleep(Duration::from_secs(2));

    let mut stream = match connect_to_enclave(enclave.enclave_cid) {
        Ok(s) => s,
        Err(e) => {
            terminate_enclave(&enclave.enclave_id);
            return Err(e);
        }
    };

    // Build and send control message (24 bytes)
    let flags: u64 = if reverse { 1 } else { 0 };
    let mut ctrl = [0u8; 24];
    ctrl[0..8].copy_from_slice(&total_bytes.to_be_bytes());
    ctrl[8..16].copy_from_slice(&buffer_size.to_be_bytes());
    ctrl[16..24].copy_from_slice(&flags.to_be_bytes());
    stream.write_all(&ctrl)?;

    let mut results = Vec::new();

    // Phase 1: Host -> Enclave
    eprintln!(
        "[bench] Sending {} bytes (buffer={})",
        format_bytes(total_bytes),
        format_bytes(buffer_size)
    );
    let start = Instant::now();
    send_data(&mut stream, total_bytes, buffer_size)?;
    // Wait for 8-byte ack
    let mut ack_buf = [0u8; 8];
    stream.read_exact(&mut ack_buf)?;
    let elapsed = start.elapsed();
    let ack_bytes = u64::from_be_bytes(ack_buf);
    eprintln!("[bench] Ack: {} bytes, {:.3}s", ack_bytes, elapsed.as_secs_f64());

    results.push(BenchResult {
        direction: "Host -> Enclave".to_string(),
        buffer_size,
        total_bytes,
        duration: elapsed,
    });

    // Phase 2: Enclave -> Host (if requested)
    if reverse {
        eprintln!(
            "[bench] Receiving {} bytes (buffer={})",
            format_bytes(total_bytes),
            format_bytes(buffer_size)
        );
        let start = Instant::now();
        let received = receive_data(&mut stream, total_bytes, buffer_size)?;
        // Send 8-byte ack
        stream.write_all(&received.to_be_bytes())?;
        let elapsed = start.elapsed();
        eprintln!("[bench] Received: {} bytes, {:.3}s", received, elapsed.as_secs_f64());

        results.push(BenchResult {
            direction: "Enclave -> Host".to_string(),
            buffer_size,
            total_bytes,
            duration: elapsed,
        });
    }

    terminate_enclave(&enclave.enclave_id);
    Ok(results)
}

// --- Formatting ---

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;

    if bytes >= GIB && bytes % GIB == 0 {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB && bytes % MIB == 0 {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('G') {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1024u64)
    } else {
        (s, 1u64)
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("Invalid size: {}", s))?;
    Ok(num * multiplier)
}

fn print_results(results: &[BenchResult]) {
    println!(
        "{:<20} {:>15} {:>15} {:>12} {:>14} {:>14}",
        "Direction", "Buffer Size", "Total Bytes", "Time (s)", "MB/s", "MiB/s"
    );
    println!("{}", "-".repeat(94));

    for r in results {
        println!(
            "{:<20} {:>15} {:>15} {:>12.3} {:>14.2} {:>14.2}",
            r.direction,
            format_bytes(r.buffer_size),
            format_bytes(r.total_bytes),
            r.duration.as_secs_f64(),
            r.mb_per_sec(),
            r.mib_per_sec(),
        );
    }
}

fn print_usage() {
    eprintln!("Usage: bench-host <eif-path> [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --size <SIZE>           Total data to transfer (default: 1G)");
    eprintln!("  --buffer-sizes <SIZES>  Comma-separated buffer sizes (default: 64K)");
    eprintln!("  --no-reverse            Skip enclave->host direction");
    eprintln!();
    eprintln!("Size suffixes: K (KiB), M (MiB), G (GiB)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  bench-host bench-enclave.eif");
    eprintln!("  bench-host bench-enclave.eif --size 512M --buffer-sizes 4K,64K,256K");
    eprintln!("  bench-host bench-enclave.eif --size 64M --no-reverse");
}

fn parse_args() -> Result<BenchConfig, String> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Err("Missing EIF path".to_string());
    }

    let eif_path = args[1].clone();
    let mut total_bytes = DEFAULT_TOTAL_BYTES;
    let mut buffer_sizes = vec![DEFAULT_BUFFER_SIZE];
    let mut reverse = true;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                i += 1;
                if i >= args.len() {
                    return Err("--size requires a value".to_string());
                }
                total_bytes = parse_size(&args[i])?;
            }
            "--buffer-sizes" => {
                i += 1;
                if i >= args.len() {
                    return Err("--buffer-sizes requires a value".to_string());
                }
                buffer_sizes = args[i]
                    .split(',')
                    .map(|s| parse_size(s))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            "--no-reverse" => {
                reverse = false;
            }
            other => {
                return Err(format!("Unknown option: {}", other));
            }
        }
        i += 1;
    }

    Ok(BenchConfig {
        eif_path,
        total_bytes,
        buffer_sizes,
        reverse,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !std::path::Path::new(&config.eif_path).exists() {
        eprintln!("Error: EIF file not found: {}", config.eif_path);
        std::process::exit(1);
    }

    eprintln!("=== Vsock Throughput Benchmark ===");
    eprintln!(
        "Total: {}, Buffer sizes: {}, Reverse: {}",
        format_bytes(config.total_bytes),
        config
            .buffer_sizes
            .iter()
            .map(|s| format_bytes(*s))
            .collect::<Vec<_>>()
            .join(", "),
        config.reverse
    );
    eprintln!();

    let mut all_results = Vec::new();

    for (idx, &buf_size) in config.buffer_sizes.iter().enumerate() {
        if config.buffer_sizes.len() > 1 {
            eprintln!(
                "--- Round {}/{}: buffer_size={} ---",
                idx + 1,
                config.buffer_sizes.len(),
                format_bytes(buf_size)
            );
        }

        let results = run_single_bench(
            &config.eif_path,
            config.total_bytes,
            buf_size,
            config.reverse,
        )?;
        all_results.extend(results);
        eprintln!();
    }

    println!();
    println!("=== Results ===");
    println!();
    print_results(&all_results);
    println!();

    Ok(())
}
