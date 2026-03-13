use axum::{extract::Request, http::StatusCode, response::IntoResponse, routing::{get, post}, Router};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
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
    fn new(mut listener: VsockListener) -> Self {
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

#[cfg(feature = "vsock")]
fn generate_attestation_doc(
    user_data: &[u8],
    nonce: &[u8],
    pub_key: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use aws_nitro_enclaves_nsm_api::api::{Request as NsmRequest, Response};
    use aws_nitro_enclaves_nsm_api::driver::{nsm_exit, nsm_init, nsm_process_request};
    use serde_bytes::ByteBuf;

    let fd = nsm_init();
    if fd < 0 {
        return Err("failed to initialize NSM driver".into());
    }

    let request = NsmRequest::Attestation {
        user_data: if user_data.is_empty() { None } else { Some(ByteBuf::from(user_data.to_vec())) },
        nonce: if nonce.is_empty() { None } else { Some(ByteBuf::from(nonce.to_vec())) },
        public_key: if pub_key.is_empty() { None } else { Some(ByteBuf::from(pub_key.to_vec())) },
    };

    let response = nsm_process_request(fd, request);
    nsm_exit(fd);

    match response {
        Response::Attestation { document } => Ok(document.to_vec()),
        Response::Error(err) => Err(format!("NSM error: {:?}", err).into()),
        _ => Err("unexpected NSM response".into()),
    }
}

#[cfg(not(feature = "vsock"))]
fn generate_attestation_doc(
    _user_data: &[u8],
    _nonce: &[u8],
    _pub_key: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Err("attestation is not supported without the vsock feature".into())
}

async fn handle_attestation() -> impl IntoResponse {
    use k256::elliptic_curve::sec1::ToEncodedPoint;

    let secret_key = k256::SecretKey::random(&mut rand::rngs::OsRng);

    // Serialize to bytes and print as hex
    let secret_key_bytes = secret_key.to_bytes();
    let secret_key_hex = hex::encode(&secret_key_bytes);
    eprintln!("[enclave] secret_key (hex): {}", secret_key_hex);

    // Recover from hex string
    let recovered_bytes = hex::decode(&secret_key_hex).expect("hex decode failed");
    let recovered_key = k256::SecretKey::from_bytes(recovered_bytes.as_slice().into())
        .expect("secret key recovery failed");
    assert_eq!(secret_key.to_bytes(), recovered_key.to_bytes());

    let pub_key = secret_key.public_key().to_encoded_point(false); // uncompressed, 65 bytes
    eprintln!("[enclave] pub_key (hex): {}", hex::encode(pub_key.as_bytes()));

    // Sign and verify
    use k256::ecdsa::{signature::{Signer, Verifier}, Signature, SigningKey, VerifyingKey};
    let message = b"I am OKX";
    let signing_key = SigningKey::from(&secret_key);
    let signature: Signature = signing_key.sign(message);
    eprintln!("[enclave] message (hex): {}", hex::encode(message));
    eprintln!("[enclave] signature (hex): {}", hex::encode(signature.to_bytes()));

    let verifying_key = VerifyingKey::from(&secret_key.public_key());
    verifying_key.verify(message, &signature).expect("signature verification failed");
    eprintln!("[enclave] signature verified ok");

    match generate_attestation_doc(b"Tradezone TEE Verifier", b"", pub_key.as_bytes()) {
        Ok(doc) => (StatusCode::OK, B64.encode(&doc)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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

#[cfg(feature = "vsock")]
async fn handle_ws_connection(stream: VsockStream) {
    use tokio_tungstenite::tungstenite::Message;

    let mut ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[enclave] WS handshake error: {}", e);
            return;
        }
    };

    let mut delay_secs = 1u64;
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                eprintln!("[enclave] WS recv {} bytes: {}", data.len(), hex::encode(&data));
            }
            Ok(Message::Text(text)) => {
                eprintln!("[enclave] WS recv text: {}", text);
            }
            Ok(Message::Close(_)) => {
                eprintln!("[enclave] WS connection closed");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[enclave] WS error: {}", e);
                break;
            }
        }
        eprintln!("[enclave] WS waiting {}s before next recv", delay_secs);
        tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs)).await;
        delay_secs += 1;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/upload", post(handle_upload))
        .route("/attestation", get(handle_attestation));

    #[cfg(feature = "vsock")]
    {
        const WS_PORT: u32 = 8001;
        let mut ws_vsock = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, WS_PORT))?;
        tokio::spawn(async move {
            eprintln!("[enclave] WS listening on vsock port {}", WS_PORT);
            loop {
                match ws_vsock.accept().await {
                    Ok((stream, addr)) => {
                        eprintln!("[enclave] WS connection from {:?}", addr);
                        tokio::spawn(handle_ws_connection(stream));
                    }
                    Err(e) => eprintln!("[enclave] WS accept error: {}", e),
                }
            }
        });

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
