use nix::sys::socket::{
    accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

const VSOCK_PORT: u32 = 5000;
const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

#[derive(Debug, Deserialize)]
struct Input {
    values: Vec<i64>,
    #[allow(dead_code)]
    operation: String,
}

#[derive(Debug, Serialize)]
struct Output {
    result: i64,
    processed: bool,
}

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

fn compute_user_data(input_bytes: &[u8], output_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input_bytes);
    hasher.update(output_bytes);
    let result = hasher.finalize();
    let mut user_data = [0u8; 32];
    user_data.copy_from_slice(&result);
    user_data
}

fn request_attestation(user_data: &[u8; 32]) -> Vec<u8> {
    // Attempt to get real attestation from NSM
    match get_nsm_attestation(user_data) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("[enclave] NSM attestation failed: {}, using mock", e);
            create_mock_attestation(user_data)
        }
    }
}

fn get_nsm_attestation(user_data: &[u8; 32]) -> Result<Vec<u8>, String> {
    use aws_nitro_enclaves_nsm_api::api::{Request, Response};
    use aws_nitro_enclaves_nsm_api::driver;

    // Open NSM device
    let nsm_fd = driver::nsm_init();
    if nsm_fd < 0 {
        return Err("Failed to open NSM device".to_string());
    }

    // Create attestation request with user_data
    let request = Request::Attestation {
        user_data: Some(user_data.to_vec().into()),
        nonce: None,
        public_key: None,
    };

    // Process the request
    let response = driver::nsm_process_request(nsm_fd, request);

    // Close the device
    driver::nsm_exit(nsm_fd);

    // Extract attestation document from response
    match response {
        Response::Attestation { document } => Ok(document),
        Response::Error(err) => Err(format!("NSM error: {:?}", err)),
        _ => Err("Unexpected NSM response".to_string()),
    }
}

fn create_mock_attestation(user_data: &[u8; 32]) -> Vec<u8> {
    // Create a mock attestation document structure for testing outside enclave
    // Real attestation documents are COSE Sign1 structures
    // This mock is just for development/testing when NSM is not available

    use serde::Serialize;

    #[derive(Serialize)]
    struct MockAttestationPayload {
        module_id: String,
        timestamp: u64,
        digest: String,
        pcrs: std::collections::HashMap<u32, String>,
        certificate: String,
        cabundle: Vec<String>,
        user_data: String,
        nonce: Option<String>,
        public_key: Option<String>,
    }

    let mut pcrs = std::collections::HashMap::new();
    // Mock PCR values (48 bytes each, hex encoded)
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
        user_data: hex::encode(user_data),
        nonce: None,
        public_key: None,
    };

    // Note: In a real attestation, this would be COSE Sign1 structure
    // For mock purposes, we just return a simple CBOR encoding
    // The verifier will detect this as mock and skip signature verification

    // Simple mock: prefix with "MOCK" magic bytes so verifier knows it's fake
    let mut result = b"MOCK".to_vec();
    result.extend(serde_json::to_vec(&payload).unwrap_or_default());
    result
}

// Simple hex encoding for mock attestation
mod hex {
    pub fn encode(data: &[u8]) -> String {
        data.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

fn handle_client(mut stream: UnixStream) -> std::io::Result<()> {
    eprintln!("[enclave] Client connected");

    // Read input frame
    let input_bytes = read_frame(&mut stream)?;
    eprintln!(
        "[enclave] Received {} bytes of input",
        input_bytes.len()
    );

    // Parse input JSON
    let input: Input = serde_json::from_slice(&input_bytes).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("Invalid JSON: {}", e))
    })?;
    eprintln!("[enclave] Parsed input: {:?}", input);

    // Perform mock computation (sum the values)
    let result: i64 = input.values.iter().sum();
    let output = Output {
        result,
        processed: true,
    };
    eprintln!("[enclave] Computed output: {:?}", output);

    // Serialize output
    let output_bytes = serde_json::to_vec(&output).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("JSON serialization failed: {}", e))
    })?;

    // Compute user_data = SHA-256(input || output)
    let user_data = compute_user_data(&input_bytes, &output_bytes);
    eprintln!(
        "[enclave] Computed user_data (SHA-256): {}",
        hex::encode(&user_data)
    );

    // Request attestation document with user_data
    let attestation_doc = request_attestation(&user_data);
    eprintln!(
        "[enclave] Got attestation document: {} bytes",
        attestation_doc.len()
    );

    // Send output frame (first frame)
    write_frame(&mut stream, &output_bytes)?;
    eprintln!("[enclave] Sent output frame");

    // Send attestation document frame (second frame)
    write_frame(&mut stream, &attestation_doc)?;
    eprintln!("[enclave] Sent attestation document frame");

    eprintln!("[enclave] Request completed successfully");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[enclave] Starting enclave application");
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

    // One-Shot mode: accept a single connection, handle it, then exit
    eprintln!("[enclave] Waiting for connection (one-shot mode)...");
    match accept(sock_fd.as_raw_fd()) {
        Ok(client_fd) => {
            // Safety: we just got this fd from accept
            let owned_fd = unsafe { OwnedFd::from_raw_fd(client_fd) };
            let stream = UnixStream::from(owned_fd);

            if let Err(e) = handle_client(stream) {
                eprintln!("[enclave] Error handling client: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("[enclave] Accept error: {}", e);
            std::process::exit(1);
        }
    }

    eprintln!("[enclave] One-shot request completed, exiting");
    Ok(())
}
