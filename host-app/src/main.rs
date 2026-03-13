use bytes::Bytes;
use futures::stream;
use http_body_util::{BodyExt, Empty, StreamBody, combinators::BoxBody};
use hyper::body::Frame;
use hyper::client::conn::http1;
use hyper::Request;
use hyper_util::rt::TokioIo;

#[cfg(feature = "vsock")]
use tokio_vsock::{VsockAddr, VsockStream};
#[cfg(not(feature = "vsock"))]
use tokio::net::TcpStream;

const ENCLAVE_PORT: u16 = 8000;
const ENCLAVE_WS_PORT: u16 = 8001;
const CHUNK_SIZE: usize = 1024 * 1024; // 1 MB per chunk

fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("GB") {
        n.parse::<usize>().map(|v| v * 1024 * 1024 * 1024).map_err(|e| e.to_string())
    } else if let Some(n) = s.strip_suffix("MB") {
        n.parse::<usize>().map(|v| v * 1024 * 1024).map_err(|e| e.to_string())
    } else if let Some(n) = s.strip_suffix("KB") {
        n.parse::<usize>().map(|v| v * 1024).map_err(|e| e.to_string())
    } else {
        Err(format!("invalid size '{}': expected format like 100KB, 500MB, 1GB", s))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: host-app <cid> <cmd> [size|count]");
        eprintln!("  cid:   enclave CID (vsock feature only; ignored otherwise)");
        eprintln!("  cmd:   upload | gen_doc | websocket");
        eprintln!("  size:  required for upload — 100KB | 500MB | 1GB");
        eprintln!("  count: required for websocket — number of frames to send");
        eprintln!("  size:  required for websocket — frame size e.g. 1KB, 1MB");
        std::process::exit(1);
    }

    #[cfg(feature = "vsock")]
    let cid: u32 = args[1].parse().map_err(|_| format!("invalid CID '{}'", args[1]))?;

    let cmd = args[2].as_str();

    if cmd == "stream" {
        use tokio_tungstenite::tungstenite::Message;
        use futures::SinkExt;

        let count: usize = args.get(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        let frame_size: usize = args.get(4)
            .map(|s| parse_size(s))
            .transpose()?
            .unwrap_or(1024); // default 1KB

        #[cfg(feature = "vsock")]
        let stream = {
            eprintln!("[host] Connecting WS via vsock CID {} port {}", cid, ENCLAVE_WS_PORT);
            VsockStream::connect(VsockAddr::new(cid, ENCLAVE_WS_PORT as u32)).await?
        };

        #[cfg(not(feature = "vsock"))]
        let stream = {
            let addr = format!("127.0.0.1:{}", ENCLAVE_WS_PORT);
            eprintln!("[host] Connecting WS via TCP {}", addr);
            TcpStream::connect(&addr).await?
        };

        let url = format!("ws://enclave:{}", ENCLAVE_WS_PORT);
        let (mut ws, _) = tokio_tungstenite::client_async(url, stream).await?;
        eprintln!("[host] WS connected, sending {} frames of {} bytes each", count, frame_size);

        let payload = Bytes::from(vec![0xABu8; frame_size]);
        for i in 1..=count {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            eprintln!("[host] WS send frame {} at {}ms ({} bytes)", i, now, frame_size);
            ws.send(Message::Binary(payload.clone().into())).await?;
        }

        ws.close(None).await?;
        eprintln!("[host] WS done");
        return Ok(());
    }

    #[cfg(feature = "vsock")]
    let io = {
        eprintln!("[host] Connecting via vsock CID {} port {}", cid, ENCLAVE_PORT);
        TokioIo::new(VsockStream::connect(VsockAddr::new(cid, ENCLAVE_PORT as u32)).await?)
    };

    #[cfg(not(feature = "vsock"))]
    let io = {
        let addr = format!("127.0.0.1:{}", ENCLAVE_PORT);
        eprintln!("[host] Connecting via TCP {}", addr);
        TokioIo::new(TcpStream::connect(&addr).await?)
    };

    let (mut sender, conn) = http1::handshake(io).await?;
    tokio::spawn(conn);

    let start = std::time::Instant::now();

    let res = match cmd {
        "upload" => {
            if args.len() < 4 {
                eprintln!("upload requires a size argument");
                std::process::exit(1);
            }
            let upload_size = parse_size(&args[3])?;
            let num_chunks = upload_size.div_ceil(CHUNK_SIZE);
            let chunk = Bytes::from(vec![0xABu8; CHUNK_SIZE]);
            let body = StreamBody::new(stream::iter(
                (0..num_chunks)
                    .map(move |_| Ok::<Frame<Bytes>, std::io::Error>(Frame::data(chunk.clone()))),
            ));
            eprintln!("[host] Uploading {} bytes ({} chunks)...", upload_size, num_chunks);
            let req = Request::builder()
                .method("POST")
                .uri("/upload")
                .header("host", "enclave")
                .header("content-type", "application/octet-stream")
                .body(BoxBody::new(body))?;
            sender.send_request(req).await?
        }
        "gen_doc" => {
            eprintln!("[host] Requesting attestation doc...");
            let req = Request::builder()
                .method("GET")
                .uri("/attestation")
                .header("host", "enclave")
                .body(BoxBody::new(Empty::<Bytes>::new().map_err(|_| unreachable!())))?;
            sender.send_request(req).await?
        }
        other => {
            eprintln!("unknown command '{}': expected upload or gen_doc", other);
            std::process::exit(1);
        }
    };

    let status = res.status();
    let body_bytes = res.collect().await?.to_bytes();
    let elapsed = start.elapsed();

    if cmd == "gen_doc" {
        if status.is_success() {
            println!("{}", String::from_utf8_lossy(&body_bytes));
        } else {
            eprintln!("[host] attestation failed ({}): {}", status, String::from_utf8_lossy(&body_bytes));
            std::process::exit(1);
        }
    } else {
        println!("Status: {}", status);
        println!("Response: {}", String::from_utf8_lossy(&body_bytes));
        println!("Duration: {:.3}s", elapsed.as_secs_f64());
    }

    Ok(())
}
