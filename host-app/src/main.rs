use base64::Engine;
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use serde::{Deserialize, Serialize};
// sha2 not needed on host side; hash is computed inside the enclave
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const VSOCK_PORT: u32 = 5000;
const ENCLAVE_CPU_COUNT: u32 = 2;
const ENCLAVE_MEMORY_MB: u32 = 512;

#[derive(Debug, Deserialize)]
struct EnclaveRunOutput {
    #[serde(rename = "EnclaveCID")]
    enclave_cid: u32,
    #[serde(rename = "EnclaveID")]
    enclave_id: String,
    #[serde(rename = "ProcessID")]
    #[allow(dead_code)]
    process_id: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct DockerImageInfo {
    dockerfile_content: String,
    git_commit: String,
    build_command: String,
    pcr0: String,
    pcr1: String,
    pcr2: String,
}

#[derive(Debug, Serialize)]
struct ProofPackage {
    attestation_document: String, // Base64-encoded CBOR
    output: serde_json::Value,
    output_raw: String,           // Base64-encoded raw output bytes (for exact hash reproduction)
    docker_image_info: DockerImageInfo,
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

fn run_enclave(eif_path: &str) -> Result<EnclaveRunOutput, Box<dyn std::error::Error>> {
    eprintln!("[host] Starting enclave from: {}", eif_path);

    // First terminate any existing enclaves
    let _ = Command::new("nitro-cli")
        .args(["terminate-enclave", "--all"])
        .output();

    // Run the enclave
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
    eprintln!("[host] Enclave started: {}", stdout.trim());

    let enclave_info: EnclaveRunOutput = serde_json::from_str(&stdout)?;
    eprintln!(
        "[host] Enclave CID: {}, ID: {}",
        enclave_info.enclave_cid, enclave_info.enclave_id
    );

    Ok(enclave_info)
}

fn terminate_enclave(enclave_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[host] Terminating enclave: {}", enclave_id);

    let output = Command::new("nitro-cli")
        .args(["terminate-enclave", "--enclave-id", enclave_id])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[host] Warning: Failed to terminate enclave: {}", stderr);
    } else {
        eprintln!("[host] Enclave terminated successfully");
    }

    Ok(())
}

fn connect_to_enclave(cid: u32, max_retries: u32) -> Result<UnixStream, Box<dyn std::error::Error>> {
    eprintln!("[host] Connecting to enclave CID {} port {}", cid, VSOCK_PORT);

    for attempt in 1..=max_retries {
        eprintln!("[host] Connection attempt {}/{}", attempt, max_retries);

        match try_connect(cid) {
            Ok(stream) => {
                eprintln!("[host] Connected to enclave");
                return Ok(stream);
            }
            Err(e) => {
                eprintln!("[host] Connection failed: {}", e);
                if attempt < max_retries {
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    Err("Failed to connect to enclave after max retries".into())
}

fn try_connect(cid: u32) -> Result<UnixStream, Box<dyn std::error::Error>> {
    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;

    let addr = VsockAddr::new(cid, VSOCK_PORT);
    connect(sock_fd.as_raw_fd(), &addr)?;

    let stream = UnixStream::from(sock_fd);
    Ok(stream)
}

fn load_docker_image_info(info_path: &str) -> DockerImageInfo {
    if let Ok(content) = fs::read_to_string(info_path) {
        if let Ok(info) = serde_json::from_str(&content) {
            return info;
        }
    }

    // Return placeholder if file doesn't exist
    eprintln!("[host] Warning: docker_image_info.json not found, using placeholders");
    DockerImageInfo {
        dockerfile_content: "<not available - run build.sh to generate>".to_string(),
        git_commit: get_git_commit().unwrap_or_else(|| "unknown".to_string()),
        build_command: "docker build --platform linux/amd64 -t enclave-app:v1 ./enclave-app/ && nitro-cli build-enclave --docker-uri enclave-app:v1 --output-file enclave.eif".to_string(),
        pcr0: "<run build.sh to get actual PCR0>".to_string(),
        pcr1: "<run build.sh to get actual PCR1>".to_string(),
        pcr2: "<run build.sh to get actual PCR2>".to_string(),
    }
}

fn get_git_commit() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn print_usage() {
    eprintln!("Usage: host-app <eif-path> <input-file> <output-dir>");
    eprintln!();
    eprintln!("Arguments:");
    eprintln!("  eif-path    Path to the enclave EIF file");
    eprintln!("  input-file  Path to the JSON input file");
    eprintln!("  output-dir  Directory to write proof package output");
    eprintln!();
    eprintln!("Example:");
    eprintln!("  host-app enclave.eif test_input.json ./output");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 4 {
        print_usage();
        std::process::exit(1);
    }

    let eif_path = &args[1];
    let input_file = &args[2];
    let output_dir = &args[3];

    // Validate paths
    if !Path::new(eif_path).exists() {
        return Err(format!("EIF file not found: {}", eif_path).into());
    }
    if !Path::new(input_file).exists() {
        return Err(format!("Input file not found: {}", input_file).into());
    }

    // Create output directory
    fs::create_dir_all(output_dir)?;

    // Read input
    let input_bytes = fs::read(input_file)?;
    eprintln!("[host] Read {} bytes from input file", input_bytes.len());

    // Validate input is valid JSON
    let _: serde_json::Value = serde_json::from_slice(&input_bytes)?;

    // Run enclave
    let enclave_info = run_enclave(eif_path)?;

    // Give enclave time to start up
    thread::sleep(Duration::from_secs(2));

    // Connect to enclave
    let mut stream = match connect_to_enclave(enclave_info.enclave_cid, 10) {
        Ok(s) => s,
        Err(e) => {
            let _ = terminate_enclave(&enclave_info.enclave_id);
            return Err(e);
        }
    };

    // Send input
    eprintln!("[host] Sending input to enclave");
    if let Err(e) = write_frame(&mut stream, &input_bytes) {
        let _ = terminate_enclave(&enclave_info.enclave_id);
        return Err(format!("Failed to send input: {}", e).into());
    }

    // Receive output frame
    eprintln!("[host] Waiting for output from enclave");
    let output_bytes = match read_frame(&mut stream) {
        Ok(b) => b,
        Err(e) => {
            let _ = terminate_enclave(&enclave_info.enclave_id);
            return Err(format!("Failed to receive output: {}", e).into());
        }
    };
    eprintln!("[host] Received {} bytes of output", output_bytes.len());

    // Receive attestation document frame
    eprintln!("[host] Waiting for attestation document from enclave");
    let attestation_bytes = match read_frame(&mut stream) {
        Ok(b) => b,
        Err(e) => {
            let _ = terminate_enclave(&enclave_info.enclave_id);
            return Err(format!("Failed to receive attestation: {}", e).into());
        }
    };
    eprintln!(
        "[host] Received {} bytes of attestation document",
        attestation_bytes.len()
    );

    // Terminate enclave
    terminate_enclave(&enclave_info.enclave_id)?;

    // Parse output JSON
    let output: serde_json::Value = serde_json::from_slice(&output_bytes)?;
    eprintln!("[host] Output: {}", serde_json::to_string_pretty(&output)?);

    // Load docker image info
    let docker_image_info = load_docker_image_info("docker_image_info.json");

    // Assemble proof package
    // output_raw preserves the exact bytes for hash verification (avoids re-serialization mismatch)
    let proof_package = ProofPackage {
        attestation_document: base64::engine::general_purpose::STANDARD.encode(&attestation_bytes),
        output,
        output_raw: base64::engine::general_purpose::STANDARD.encode(&output_bytes),
        docker_image_info,
    };

    // Write proof package
    let proof_path = Path::new(output_dir).join("proof_package.json");
    let proof_json = serde_json::to_string_pretty(&proof_package)?;
    fs::write(&proof_path, &proof_json)?;
    eprintln!("[host] Proof package written to: {}", proof_path.display());

    // Also copy input for verification
    let input_copy_path = Path::new(output_dir).join("input.json");
    fs::copy(input_file, &input_copy_path)?;
    eprintln!("[host] Input copied to: {}", input_copy_path.display());

    eprintln!("[host] Done!");
    Ok(())
}
