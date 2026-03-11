use nix::sys::socket::{
    accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Instant;

const VSOCK_PORT: u32 = 5001;
const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

struct ControlMessage {
    total_bytes: u64,
    buffer_size: u64,
    flags: u64,
}

impl ControlMessage {
    fn from_bytes(buf: &[u8; 24]) -> Self {
        Self {
            total_bytes: u64::from_be_bytes(buf[0..8].try_into().unwrap()),
            buffer_size: u64::from_be_bytes(buf[8..16].try_into().unwrap()),
            flags: u64::from_be_bytes(buf[16..24].try_into().unwrap()),
        }
    }

    fn reverse_requested(&self) -> bool {
        self.flags & 1 != 0
    }
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

fn handle_client(mut stream: UnixStream) -> std::io::Result<()> {
    eprintln!("[bench-enclave] Client connected");

    // Read 24-byte control message
    let mut ctrl_buf = [0u8; 24];
    stream.read_exact(&mut ctrl_buf)?;
    let ctrl = ControlMessage::from_bytes(&ctrl_buf);

    eprintln!(
        "[bench-enclave] Control: total_bytes={}, buffer_size={}, reverse={}",
        ctrl.total_bytes, ctrl.buffer_size, ctrl.reverse_requested()
    );

    // Phase 1: Receive data from host
    eprintln!("[bench-enclave] Phase 1: receiving {} bytes...", ctrl.total_bytes);
    let start = Instant::now();
    let bytes_received = receive_data(&mut stream, ctrl.total_bytes, ctrl.buffer_size)?;
    let elapsed = start.elapsed();

    let mb_per_sec = bytes_received as f64 / 1_000_000.0 / elapsed.as_secs_f64();
    eprintln!(
        "[bench-enclave] Received {} bytes in {:.3}s ({:.2} MB/s)",
        bytes_received,
        elapsed.as_secs_f64(),
        mb_per_sec
    );

    // Send 8-byte ack
    stream.write_all(&bytes_received.to_be_bytes())?;

    // Phase 2: Send data back to host (if requested)
    if ctrl.reverse_requested() {
        eprintln!("[bench-enclave] Phase 2: sending {} bytes...", ctrl.total_bytes);
        let start = Instant::now();
        send_data(&mut stream, ctrl.total_bytes, ctrl.buffer_size)?;
        let elapsed = start.elapsed();

        let mb_per_sec = ctrl.total_bytes as f64 / 1_000_000.0 / elapsed.as_secs_f64();
        eprintln!(
            "[bench-enclave] Sent {} bytes in {:.3}s ({:.2} MB/s)",
            ctrl.total_bytes,
            elapsed.as_secs_f64(),
            mb_per_sec
        );

        // Wait for 8-byte ack from host
        let mut ack_buf = [0u8; 8];
        stream.read_exact(&mut ack_buf)?;
        let ack_bytes = u64::from_be_bytes(ack_buf);
        eprintln!("[bench-enclave] Host ack: {} bytes", ack_bytes);
    }

    eprintln!("[bench-enclave] Done");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[bench-enclave] Starting benchmark enclave on port {}", VSOCK_PORT);

    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;

    let addr = VsockAddr::new(VMADDR_CID_ANY, VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(1)?)?;

    eprintln!("[bench-enclave] Waiting for connection (one-shot mode)...");
    match accept(sock_fd.as_raw_fd()) {
        Ok(client_fd) => {
            let owned_fd = unsafe { OwnedFd::from_raw_fd(client_fd) };
            let stream = UnixStream::from(owned_fd);

            if let Err(e) = handle_client(stream) {
                eprintln!("[bench-enclave] Error: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("[bench-enclave] Accept error: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
