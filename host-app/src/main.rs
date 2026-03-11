use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use serde::{Deserialize, Serialize};
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
    attestation_document: String, // Base64-encoded COSE Sign1 (or mock)
    public_key: String,           // Hex-encoded secp256k1 uncompressed (65 bytes)
    docker_image_info: DockerImageInfo,
}

// Enclave response types
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum EnclaveResponse {
    #[serde(rename = "init")]
    Init {
        public_key: String,
        attestation_document: String,
    },

    #[serde(rename = "sign-batch")]
    SignBatch {
        batch_digest: String,
        signature: String,
    },

    #[serde(rename = "health")]
    #[allow(dead_code)]
    Health {
        initialized: bool,
        public_key: Option<String>,
    },

    #[serde(rename = "error")]
    Error { message: String },
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

fn send_command(stream: &mut UnixStream, command_json: &str) -> Result<EnclaveResponse, Box<dyn std::error::Error>> {
    write_frame(stream, command_json.as_bytes())?;

    let response_bytes = read_frame(stream)?;
    let response: EnclaveResponse = serde_json::from_slice(&response_bytes)?;
    Ok(response)
}

fn print_usage() {
    eprintln!("Usage: host-app <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  init <eif-path> <output-dir>     Initialize enclave and generate proof package");
    eprintln!("  sign-batch <eif-path> <params>    Sign a batch digest (requires running enclave)");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  host-app init enclave.eif ./output");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "init" => {
            if args.len() != 4 {
                eprintln!("Usage: host-app init <eif-path> <output-dir>");
                std::process::exit(1);
            }

            let eif_path = &args[2];
            let output_dir = &args[3];

            if !Path::new(eif_path).exists() {
                return Err(format!("EIF file not found: {}", eif_path).into());
            }

            fs::create_dir_all(output_dir)?;

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

            // Send init command
            eprintln!("[host] Sending init command to enclave");
            let response = match send_command(&mut stream, r#"{"command":"init"}"#) {
                Ok(r) => r,
                Err(e) => {
                    let _ = terminate_enclave(&enclave_info.enclave_id);
                    return Err(format!("Failed to communicate with enclave: {}", e).into());
                }
            };

            // Terminate enclave
            terminate_enclave(&enclave_info.enclave_id)?;

            // Process response
            match response {
                EnclaveResponse::Init {
                    public_key,
                    attestation_document,
                } => {
                    eprintln!("[host] Init successful!");
                    eprintln!("[host] Public key: {}", &public_key[..16]);
                    eprintln!(
                        "[host] Attestation document: {} chars (base64)",
                        attestation_document.len()
                    );

                    // Load docker image info
                    let docker_image_info = load_docker_image_info("docker_image_info.json");

                    // Assemble proof package
                    let proof_package = ProofPackage {
                        attestation_document,
                        public_key,
                        docker_image_info,
                    };

                    // Write proof package
                    let proof_path = Path::new(output_dir).join("proof_package.json");
                    let proof_json = serde_json::to_string_pretty(&proof_package)?;
                    fs::write(&proof_path, &proof_json)?;
                    eprintln!("[host] Proof package written to: {}", proof_path.display());
                }
                EnclaveResponse::Error { message } => {
                    return Err(format!("Enclave error: {}", message).into());
                }
                other => {
                    return Err(format!("Unexpected response: {:?}", other).into());
                }
            }

            eprintln!("[host] Done!");
        }

        "sign-batch" => {
            // For sign-batch, we expect a running enclave (daemon mode)
            // Usage: host-app sign-batch <cid> <start_root> <end_root> <l2_block> <chain_id>
            if args.len() != 7 {
                eprintln!("Usage: host-app sign-batch <cid> <start_output_root> <end_output_root> <l2_block_number> <chain_id>");
                std::process::exit(1);
            }

            let cid: u32 = args[2].parse()?;
            let start_root = &args[3];
            let end_root = &args[4];
            let l2_block: u64 = args[5].parse()?;
            let chain_id: u64 = args[6].parse()?;

            let mut stream = connect_to_enclave(cid, 5)?;

            let cmd = serde_json::json!({
                "command": "sign-batch",
                "start_output_root": start_root,
                "end_output_root": end_root,
                "l2_block_number": l2_block,
                "chain_id": chain_id
            });

            eprintln!("[host] Sending sign-batch command");
            let response = send_command(&mut stream, &cmd.to_string())?;

            match response {
                EnclaveResponse::SignBatch {
                    batch_digest,
                    signature,
                } => {
                    // Output as JSON to stdout for scripting
                    let output = serde_json::json!({
                        "batch_digest": batch_digest,
                        "signature": signature
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
                EnclaveResponse::Error { message } => {
                    return Err(format!("Enclave error: {}", message).into());
                }
                other => {
                    return Err(format!("Unexpected response: {:?}", other).into());
                }
            }
        }

        _ => {
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}
