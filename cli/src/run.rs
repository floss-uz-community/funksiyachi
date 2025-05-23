use anyhow::{anyhow, Context, Result};
use faasta_interface::FunctionServiceClient;
use std::io;
// futures prelude removed
use s2n_quic::client::Connect;
use s2n_quic::provider::tls::default::callbacks::VerifyHostNameCallback;
use s2n_quic::provider::tls::default::Client as TlsClient;
use s2n_quic::Client;
use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::process::exit;
use tarpc::serde_transport as transport;
use tarpc::tokio_serde::formats::Bincode;
use tarpc::tokio_util::codec::LengthDelimitedCodec;
use tracing::debug;

/// Compare two file paths in a slightly more robust way.
/// (On Windows, e.g., backslash vs forward slash).
fn same_file_path(a: &str, b: &str) -> bool {
    // Convert both to a canonical PathBuf
    let path_a = StdPath::new(a).components().collect::<Vec<_>>();
    let path_b = StdPath::new(b).components().collect::<Vec<_>>();
    path_a == path_b
}

// Create a connection to the function service
pub async fn connect_to_function_service(server_addr: &str) -> Result<FunctionServiceClient> {
    // Check if we're connecting to localhost or 127.0.0.1
    let skip_tls_validation =
        server_addr.starts_with("localhost:") || server_addr.starts_with("127.0.0.1:");

    // Set up the QUIC client with minimal logging
    let client = if skip_tls_validation {
        // Create a struct that implements VerifyHostNameCallback to accept any hostname
        struct AcceptAnyHostname;
        impl VerifyHostNameCallback for AcceptAnyHostname {
            fn verify_host_name(&self, _server_name: &str) -> bool {
                // Always return true to accept any hostname
                true
            }
        }

        // Use embedded certificate for localhost/127.0.0.1 connections
        // This certificate is included at compile time
        // It is self signed and matches the one in server-wasi, Not for Production use!
        let cert_pem = include_str!("../certs/cert.pem");

        // Build a TLS configuration using the embedded certificate
        let tls_config = TlsClient::builder()
            .with_certificate(cert_pem)
            .context("Failed to add embedded certificate")?
            // Skip hostname verification to allow self-signed certs on localhost
            .with_verify_host_name_callback(AcceptAnyHostname)
            .context("Failed to set hostname verification callback")?
            .build()
            .context("Failed to build TLS config")?;

        // Use this config in the QUIC client
        Client::builder()
            .with_tls(tls_config)
            .context("Failed to set TLS config")?
            .with_io("0.0.0.0:0")
            .context("Failed to set up client IO")?
            .start()
            .context("Failed to start client")?
    } else {
        // Standard client with default TLS settings
        // For non-localhost connections, use the system's PKI
        Client::builder()
            .with_io("0.0.0.0:0")
            .context("Failed to set up client IO")?
            .start()
            .context("Failed to start client")?
    };

    // Parse the server address, handling both IP:port and hostname:port formats
    let addr: SocketAddr = match server_addr.parse() {
        Ok(addr) => addr,
        Err(_) => {
            // Try to resolve the hostname
            let parts: Vec<&str> = server_addr.split(':').collect();
            if parts.len() != 2 {
                return Err(anyhow!(
                    "Invalid server address format. Expected hostname:port or IP:port"
                ));
            }

            let hostname = parts[0];
            let port = parts[1].parse::<u16>().context("Invalid port number")?;

            // For localhost, use 127.0.0.1
            if hostname == "localhost" || hostname == "localhost.localdomain" {
                format!("127.0.0.1:{port}")
                    .parse()
                    .context("Failed to parse localhost address")?
            } else {
                // For other hostnames, try to resolve using DNS
                match tokio::net::lookup_host(format!("{hostname}:{port}")).await {
                    Ok(mut addrs) => {
                        // Take the first resolved address
                        if let Some(addr) = addrs.next() {
                            addr
                        } else {
                            return Err(anyhow!(
                                "Could not resolve hostname: {}. No addresses found.",
                                hostname
                            ));
                        }
                    }
                    Err(e) => {
                        return Err(anyhow!(
                            "Could not resolve hostname: {}. Error: {}",
                            hostname,
                            e
                        ));
                    }
                }
            }
        }
    };

    let server_name = if server_addr.starts_with("localhost:")
        || server_addr.contains("localhost.localdomain:")
    {
        "localhost".to_string()
    } else {
        // Extract the hostname from the original server_addr string for SNI
        let parts: Vec<&str> = server_addr.split(':').collect();
        parts[0].to_string()
    };

    let connect = Connect::new(addr).with_server_name(server_name.as_str());

    let mut connection = client
        .connect(connect)
        .await
        .map_err(|e| {
            // Provide minimal error info for handshake failures
            if e.to_string().contains("handshake") {
                if e.to_string().contains("timeout") {
                    anyhow!("Failed to connect: Handshake timeout. Check your network connection or firewall settings.")
                } else {
                    anyhow!("Failed to connect: TLS handshake error. The server may be down or unreachable.")
                }
            } else {
                anyhow!("Failed to connect: {}", e)
            }
        })?;

    // Open bidirectional stream
    let stream = connection
        .open_bidirectional_stream()
        .await
        .map_err(|e| anyhow!("Failed to open stream: {}", e))?;
    debug!("Opened bidirectional stream to function service");

    let framed = LengthDelimitedCodec::builder().new_framed(stream);
    let transport = transport::new(framed, Bincode::default());

    // Use default client config
    let client = FunctionServiceClient::new(Default::default(), transport).spawn();

    Ok(client)
}

/// Get the target directory and package name for the current project
pub fn get_project_info() -> Result<(PathBuf, String, PathBuf), io::Error> {
    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message("Getting project information...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    // Get package info using cargo metadata
    let output = std::process::Command::new("cargo")
        .args(["metadata", "--format-version=1"])
        .output()
        .unwrap_or_else(|e| {
            spinner.finish_and_clear();
            eprintln!("Failed to run cargo metadata: {e}");
            exit(1);
        });

    if !output.status.success() {
        spinner.finish_and_clear();
        eprintln!("Failed to retrieve cargo metadata");
        exit(1);
    }

    // Parse JSON
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        spinner.finish_and_clear();
        eprintln!("Failed to parse cargo metadata: {e}");
        exit(1);
    });

    // Extract target_directory
    let target_directory = metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            spinner.finish_and_clear();
            eprintln!("No 'target_directory' found in cargo metadata");
            exit(1);
        });

    // Get the package name from the current directory's Cargo.toml
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| {
            spinner.finish_and_clear();
            eprintln!("No 'packages' found in cargo metadata");
            exit(1);
        });

    // Find the package for the current directory
    let current_dir = std::env::current_dir().unwrap_or_else(|e| {
        spinner.finish_and_clear();
        eprintln!("Failed to get current directory: {e}");
        exit(1);
    });

    let package_name = packages
        .iter()
        .filter_map(|pkg| {
            let manifest_path = pkg.get("manifest_path")?.as_str()?;
            let pkg_dir = StdPath::new(manifest_path).parent()?;
            if same_file_path(&pkg_dir.to_string_lossy(), &current_dir.to_string_lossy()) {
                pkg.get("name")?.as_str().map(String::from)
            } else {
                None
            }
        })
        .next()
        .unwrap_or_else(|| {
            spinner.finish_and_clear();
            eprintln!("Could not find package for current directory");
            exit(1);
        });

    spinner.finish_and_clear();
    Ok((target_directory, package_name, current_dir))
}

/// Build the project for wasm32-wasip2 target
pub fn build_project(package_root: &PathBuf) -> Result<(), io::Error> {
    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message("Building optimized WASI component...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    // Validate the project structure
    if !package_root.join("src").join("lib.rs").exists() {
        spinner.finish_and_clear();
        eprintln!("Error: src/lib.rs is missing. This file is required for Faasta functions.");
        eprintln!("Hint: Run 'cargo faasta new <n>' to create a new Faasta project.");
        exit(1);
    }

    // Build with wasm32-wasip2 target
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(package_root)
        .status()
        .unwrap_or_else(|e| {
            spinner.finish_and_clear();
            eprintln!("Failed to run cargo build: {e}");
            exit(1);
        });

    if !status.success() {
        spinner.finish_and_clear();
        eprintln!("Build failed");
        exit(1);
    }

    spinner.finish_and_clear();
    println!("✅ Build successful!");
    Ok(())
}

// The function to handle the run command
pub async fn handle_run(port: u16) -> io::Result<()> {
    // Get project information
    let (target_directory, package_name, package_root) = get_project_info()?;

    // Display project info
    println!("Building project: {package_name}");
    println!("Project root: {}", package_root.display());

    // Build the project first
    build_project(&package_root)?;

    // Get the full WASM file path - use same logic as in deploy
    let rust_compiled_name = package_name.replace('-', "_");
    let wasm_filename = format!("{rust_compiled_name}.wasm");
    let wasm_path = target_directory
        .join("wasm32-wasip2")
        .join("release")
        .join(wasm_filename);

    // Ensure the WASM file exists
    if !wasm_path.exists() {
        eprintln!(
            "Error: Could not find compiled WASM at: {}",
            wasm_path.display()
        );
        eprintln!("Build seems to have failed or produced output in a different location.");
        exit(1);
    }

    println!("Starting local server on port {port}...");
    let status = std::process::Command::new("wasmtime")
        .args(["serve", &wasm_path.to_string_lossy()])
        .current_dir(&package_root)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Failed to run wasmtime serve: {e}");
            exit(1);
        });

    if !status.success() {
        eprintln!("wasmtime serve exited with an error");
        exit(1);
    }

    Ok(())
}
