use alloy_primitives::{keccak256, B256, U256};
use base64::Engine;
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use nix::sys::socket::{
    accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

const VSOCK_PORT: u32 = 5000;
const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

// --- Command / Response protocol ---

#[derive(Debug, Deserialize)]
#[serde(tag = "command")]
enum Command {
    #[serde(rename = "init")]
    Init,

    #[serde(rename = "sign-batch")]
    SignBatch {
        start_output_root: String, // hex bytes32
        end_output_root: String,   // hex bytes32
        l2_block_number: u64,
        chain_id: u64,
    },

    #[serde(rename = "health")]
    Health,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum Response {
    #[serde(rename = "init")]
    Init {
        public_key: String,            // hex, 65 bytes uncompressed
        attestation_document: String,  // base64, COSE Sign1 (or mock)
    },

    #[serde(rename = "sign-batch")]
    SignBatch {
        batch_digest: String, // hex bytes32
        signature: String,   // hex, 65 bytes (r+s+v)
    },

    #[serde(rename = "health")]
    Health {
        initialized: bool,
        public_key: Option<String>,
    },

    #[serde(rename = "error")]
    Error { message: String },
}

// --- Enclave state ---

struct EnclaveState {
    signing_key: SigningKey,
    public_key_bytes: Vec<u8>, // 65 bytes uncompressed
}

// --- Frame I/O (4-byte BE length prefix + payload) ---

fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 10 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Frame too large",
        ));
    }

    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

fn write_frame(stream: &mut UnixStream, data: &[u8]) -> std::io::Result<()> {
    let len = data.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()?;
    Ok(())
}

// --- NSM attestation ---

fn get_nsm_attestation(user_data: Option<&[u8]>, public_key: Option<&[u8]>) -> Result<Vec<u8>, String> {
    use aws_nitro_enclaves_nsm_api::api::{Request, Response};
    use aws_nitro_enclaves_nsm_api::driver;

    let nsm_fd = driver::nsm_init();
    if nsm_fd < 0 {
        return Err("Failed to open NSM device".to_string());
    }

    let request = Request::Attestation {
        user_data: user_data.map(|d| d.to_vec().into()),
        nonce: None,
        public_key: public_key.map(|k| k.to_vec().into()),
    };

    let response = driver::nsm_process_request(nsm_fd, request);
    driver::nsm_exit(nsm_fd);

    match response {
        Response::Attestation { document } => Ok(document),
        Response::Error(err) => Err(format!("NSM error: {:?}", err)),
        _ => Err("Unexpected NSM response".to_string()),
    }
}

fn create_mock_attestation(user_data: Option<&[u8]>, public_key: Option<&[u8]>) -> Vec<u8> {
    use serde::Serialize;

    #[derive(Serialize)]
    struct MockAttestationPayload {
        module_id: String,
        timestamp: u64,
        digest: String,
        pcrs: std::collections::HashMap<u32, String>,
        certificate: String,
        cabundle: Vec<String>,
        user_data: Option<String>,
        nonce: Option<String>,
        public_key: Option<String>,
    }

    let mut pcrs = std::collections::HashMap::new();
    let mock_pcr = "0".repeat(96);
    for i in 0..16 {
        pcrs.insert(i, mock_pcr.clone());
    }

    let payload = MockAttestationPayload {
        module_id: "mock-enclave".to_string(),
        timestamp: 0,
        digest: "SHA384".to_string(),
        pcrs,
        certificate: "MOCK_CERTIFICATE".to_string(),
        cabundle: vec!["MOCK_CA".to_string()],
        user_data: user_data.map(hex::encode),
        nonce: None,
        public_key: public_key.map(hex::encode),
    };

    let mut result = b"MOCK".to_vec();
    result.extend(serde_json::to_vec(&payload).unwrap_or_default());
    result
}

fn request_attestation(user_data: Option<&[u8]>, public_key: Option<&[u8]>) -> Vec<u8> {
    match get_nsm_attestation(user_data, public_key) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("[enclave] NSM attestation failed: {}, using mock", e);
            create_mock_attestation(user_data, public_key)
        }
    }
}

// --- Batch signing ---

fn sign_batch(
    signing_key: &SigningKey,
    start_output_root: B256,
    end_output_root: B256,
    l2_block_number: U256,
    chain_id: U256,
) -> (B256, Vec<u8>) {
    // Compute batchDigest = keccak256(abi.encode(startRoot, endRoot, l2Block, chainId))
    let encoded = [
        start_output_root.as_slice(),
        end_output_root.as_slice(),
        &l2_block_number.to_be_bytes::<32>(),
        &chain_id.to_be_bytes::<32>(),
    ]
    .concat();
    let batch_digest = keccak256(&encoded);

    // Sign with secp256k1 (sign_prehash_recoverable)
    let (sig, recovery_id) = signing_key
        .sign_prehash_recoverable(batch_digest.as_slice())
        .expect("signing failed");

    // Pack as 65 bytes: r(32) + s(32) + v(1)
    let mut signature = sig.to_bytes().to_vec(); // 64 bytes (r + s)
    signature.push(recovery_id.to_byte() + 27); // v: 0/1 -> 27/28

    (batch_digest, signature)
}

fn parse_hex_bytes32(s: &str) -> Result<B256, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).map_err(|e| format!("invalid hex: {}", e))?;
    if bytes.len() != 32 {
        return Err(format!("expected 32 bytes, got {}", bytes.len()));
    }
    Ok(B256::from_slice(&bytes))
}

// --- Command handling ---

fn handle_command(command: Command, state: &mut Option<EnclaveState>) -> Response {
    match command {
        Command::Init => {
            eprintln!("[enclave] Handling init command");

            // Generate secp256k1 keypair
            let signing_key = SigningKey::random(&mut OsRng);
            let verifying_key = signing_key.verifying_key();
            let public_key_bytes = verifying_key
                .to_encoded_point(false) // uncompressed: 0x04 + x(32) + y(32) = 65 bytes
                .as_bytes()
                .to_vec();

            eprintln!(
                "[enclave] Generated secp256k1 keypair, pubkey: {} bytes",
                public_key_bytes.len()
            );

            // Request attestation with public key embedded
            let attestation_doc = request_attestation(None, Some(&public_key_bytes));
            eprintln!(
                "[enclave] Got attestation document: {} bytes",
                attestation_doc.len()
            );

            let public_key_hex = hex::encode(&public_key_bytes);
            let attestation_b64 =
                base64::engine::general_purpose::STANDARD.encode(&attestation_doc);

            // Store state
            *state = Some(EnclaveState {
                signing_key,
                public_key_bytes,
            });

            Response::Init {
                public_key: public_key_hex,
                attestation_document: attestation_b64,
            }
        }

        Command::SignBatch {
            start_output_root,
            end_output_root,
            l2_block_number,
            chain_id,
        } => {
            eprintln!("[enclave] Handling sign-batch command");

            let st = match state.as_ref() {
                Some(s) => s,
                None => {
                    return Response::Error {
                        message: "Not initialized. Call 'init' first.".to_string(),
                    };
                }
            };

            let start_root = match parse_hex_bytes32(&start_output_root) {
                Ok(v) => v,
                Err(e) => {
                    return Response::Error {
                        message: format!("Invalid start_output_root: {}", e),
                    };
                }
            };
            let end_root = match parse_hex_bytes32(&end_output_root) {
                Ok(v) => v,
                Err(e) => {
                    return Response::Error {
                        message: format!("Invalid end_output_root: {}", e),
                    };
                }
            };

            let l2_block = U256::from(l2_block_number);
            let chain = U256::from(chain_id);

            let (batch_digest, signature) =
                sign_batch(&st.signing_key, start_root, end_root, l2_block, chain);

            eprintln!(
                "[enclave] Signed batch digest: 0x{}",
                hex::encode(batch_digest.as_slice())
            );

            Response::SignBatch {
                batch_digest: format!("0x{}", hex::encode(batch_digest.as_slice())),
                signature: format!("0x{}", hex::encode(&signature)),
            }
        }

        Command::Health => {
            let (initialized, public_key) = match state.as_ref() {
                Some(st) => (true, Some(hex::encode(&st.public_key_bytes))),
                None => (false, None),
            };

            Response::Health {
                initialized,
                public_key,
            }
        }
    }
}

fn handle_client(mut stream: UnixStream, state: &mut Option<EnclaveState>) -> std::io::Result<()> {
    eprintln!("[enclave] Client connected");

    // Read command frame
    let cmd_bytes = read_frame(&mut stream)?;
    eprintln!(
        "[enclave] Received {} bytes of command",
        cmd_bytes.len()
    );

    // Parse command
    let command: Command = match serde_json::from_slice(&cmd_bytes) {
        Ok(cmd) => cmd,
        Err(e) => {
            let response = Response::Error {
                message: format!("Invalid command JSON: {}", e),
            };
            let response_bytes = serde_json::to_vec(&response).unwrap_or_default();
            write_frame(&mut stream, &response_bytes)?;
            return Ok(());
        }
    };
    eprintln!("[enclave] Parsed command: {:?}", command);

    // Handle command
    let response = handle_command(command, state);

    // Send response frame
    let response_bytes = serde_json::to_vec(&response).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("JSON serialization failed: {}", e),
        )
    })?;
    write_frame(&mut stream, &response_bytes)?;
    eprintln!("[enclave] Sent response frame ({} bytes)", response_bytes.len());

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[enclave] Starting enclave application (daemon mode)");
    eprintln!("[enclave] Listening on vsock port {}", VSOCK_PORT);

    // Create vsock socket
    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;

    // Bind to any CID, specific port
    let addr = VsockAddr::new(VMADDR_CID_ANY, VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;

    // Listen for connections
    listen(&sock_fd, Backlog::new(10)?)?;
    eprintln!("[enclave] Socket bound and listening");

    // Persistent state across connections
    let mut state: Option<EnclaveState> = None;

    // Daemon mode: accept connections in a loop
    loop {
        eprintln!("[enclave] Waiting for connection...");
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let owned_fd = unsafe { OwnedFd::from_raw_fd(client_fd) };
                let stream = UnixStream::from(owned_fd);

                if let Err(e) = handle_client(stream, &mut state) {
                    eprintln!("[enclave] Error handling client: {}", e);
                    // Don't exit - continue serving
                }
            }
            Err(e) => {
                eprintln!("[enclave] Accept error: {}", e);
                // Don't exit on transient accept errors
            }
        }
    }
}
