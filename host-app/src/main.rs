use bytes::Bytes;
use futures::stream;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::client::conn::http1;
use hyper::Request;
use hyper_util::rt::TokioIo;

#[cfg(feature = "vsock")]
use tokio_vsock::{VsockAddr, VsockStream};
#[cfg(not(feature = "vsock"))]
use tokio::net::TcpStream;

const ENCLAVE_PORT: u16 = 8000;
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
        eprintln!("Usage: host-app <size> <cid>");
        eprintln!("  size: 100KB | 500MB | 1GB");
        eprintln!("  cid:  enclave CID (vsock feature only; ignored otherwise)");
        std::process::exit(1);
    }

    let upload_size = parse_size(&args[1])?;

    #[cfg(feature = "vsock")]
    let cid: u32 = args[2].parse().map_err(|_| format!("invalid CID '{}'", args[2]))?;

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
        .body(body)?;

    let start = std::time::Instant::now();
    let res = sender.send_request(req).await?;
    let status = res.status();
    let body_bytes = res.collect().await?.to_bytes();
    let elapsed = start.elapsed();

    println!("Status: {}", status);
    println!("Response: {}", String::from_utf8_lossy(&body_bytes));
    println!("Duration: {:.3}s", elapsed.as_secs_f64());

    Ok(())
}
