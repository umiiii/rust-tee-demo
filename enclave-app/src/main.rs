use axum::{extract::Request, response::IntoResponse, routing::post, Router};
use futures::StreamExt;
use sha2::{Digest, Sha256};

#[cfg(feature = "vsock")]
use {
    axum::serve::Listener,
    std::io,
    tokio_vsock::{VsockAddr, VsockListener, VsockStream},
};

const PORT: u16 = 8000;

#[cfg(feature = "vsock")]
const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

#[cfg(feature = "vsock")]
struct VsockListenerAdapter {
    rx: tokio::sync::mpsc::Receiver<(VsockStream, VsockAddr)>,
}

#[cfg(feature = "vsock")]
impl VsockListenerAdapter {
    fn new(listener: VsockListener) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(conn) => {
                        if tx.send(conn).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("[enclave] Accept error: {}", e),
                }
            }
        });
        Self { rx }
    }
}

#[cfg(feature = "vsock")]
impl Listener for VsockListenerAdapter {
    type Io = VsockStream;
    type Addr = VsockAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        self.rx.recv().await.expect("vsock accept channel closed")
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(VsockAddr::new(VMADDR_CID_ANY, PORT as u32))
    }
}

async fn handle_upload(req: Request) -> impl IntoResponse {
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;

    let mut stream = req.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(data) => {
                size += data.len() as u64;
                hasher.update(&data);
            }
            Err(e) => {
                eprintln!("[enclave] Body read error: {}", e);
                break;
            }
        }
    }

    let hash_hex = hex::encode(hasher.finalize());
    eprintln!("[enclave] Hashed {} bytes -> {}", size, hash_hex);

    serde_json::json!({ "hash": hash_hex, "size": size }).to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new().route("/upload", post(handle_upload));

    #[cfg(feature = "vsock")]
    {
        eprintln!("[enclave] Listening on vsock port {}", PORT);
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, PORT as u32))?;
        axum::serve(VsockListenerAdapter::new(listener), app).await?;
    }

    #[cfg(not(feature = "vsock"))]
    {
        eprintln!("[enclave] Listening on 127.0.0.1:{}", PORT);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT)).await?;
        axum::serve(listener, app).await?;
    }

    Ok(())
}
