use clap::{Command, Arg};
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use walkdir::WalkDir;
use std::path::{Path, PathBuf};
use std::io;
use std::time::Duration;
use serde::{Serialize, Deserialize};
use indicatif::{ProgressBar, ProgressStyle};
use colored::*;
use anyhow::{Context, Result};

#[derive(Serialize, Deserialize, Debug)]
struct BuildUnit {
    package_name: String,
    dependencies: Vec<String>,
    source_files: Vec<PathBuf>,
    artifacts: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug)]
enum BuildRequest {
    BuildUnit {
        unit: BuildUnit,
        release: bool,
        target: Option<String>,
        tarball_data: Vec<u8>,
    },
    TransferArtifact {
        from_unit: String,
        artifact_path: PathBuf,
    },
    Heartbeat
}

#[derive(Serialize, Deserialize, Debug)]
enum BuildResponse {
    BuildComplete {
        unit_name: String,
        artifacts: Vec<(PathBuf, Vec<u8>)>,
    },
    BuildError {
        unit_name: String,
        error: String,
    },
    HeartbeatAck
}

fn should_include_file(path: &Path) -> bool {
    let ignored_patterns = [
        ".git", ".gitignore", "target", "node_modules",
        ".cargo", ".vscode", ".idea", "builds",
        ".DS_Store", "Thumbs.db", // System files
        ".exe", ".dll", ".so", ".dylib", // Binaries
        ".o", ".obj", // Object files
    ];

    // Check if file exists and is regular file
    if !path.is_file() {
        return false;
    }

    // Convert path to string for pattern matching
    let path_str = path.to_string_lossy();

    // Skip files larger than 50MB
    if path.metadata().map(|m| m.len() > 50_000_000).unwrap_or(true) {
        return false;
    }

    // Check against ignored patterns
    !ignored_patterns.iter().any(|&pattern| path_str.contains(pattern))
}

fn create_progress_bar(len: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(ProgressStyle::default_bar()
        .template("{msg} {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}")
        .unwrap()
        .progress_chars("#>-"));
    pb.set_message(message.to_string());
    pb
}

async fn send_request(
    stream: &mut TcpStream,
    request: &BuildRequest,
    timeout: Duration,
) -> Result<BuildResponse> {
    tokio::time::timeout(timeout, async {
        let data = bincode::serialize(request)
            .context("Failed to serialize request")?;
        let len = (data.len() as u32).to_be_bytes();
        
        stream.write_all(&len).await?;
        stream.write_all(&data).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0; len];
        stream.read_exact(&mut buf).await?;
        
        bincode::deserialize(&buf)
            .context("Failed to deserialize response")
    })
    .await
    .context("Request timed out")?
}

fn create_tarball(files: &[PathBuf], current_dir: &Path) -> Result<Vec<u8>> {
    let mut tarball = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);

    for path in files {
        let relative_path = path.strip_prefix(current_dir)
            .with_context(|| format!("Failed to strip prefix from {}", path.display()))?;
            
        tar.append_path_with_name(path, relative_path)
            .with_context(|| format!("Failed to add {} to tarball", path.display()))?;
    }

    tar.finish().context("Failed to finish tar archive")?;
    drop(tar); // Explicitly drop tar to release the borrow on tarball
    Ok(tarball)
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = Command::new("cargo-tess")
        .bin_name("cargo")
        .subcommand(
            Command::new("tess")
                .about("Distributed Rust build system")
                .arg(Arg::new("node")
                    .short('n')
                    .long("node")
                    .help("Address of any node in cluster (host:port)")
                    .default_value("localhost:9876"))
                .arg(Arg::new("release")
                    .long("release")
                    .help("Build with release profile"))
                .arg(Arg::new("target")
                    .long("target")
                    .help("Build for specific target triple"))
                .arg(Arg::new("verbose")
                    .short('v')
                    .long("verbose")
                    .help("Enable verbose logging"))
                .arg(Arg::new("timeout")
                    .short('t')
                    .long("timeout")
                    .help("Timeout in seconds")
                    .default_value("600")))
        .get_matches();

    let Some(matches) = matches.subcommand_matches("tess") else {
        println!("Use 'cargo tess' to run the distributed build");
        return Ok(());
    };

    let node = matches.get_one::<String>("node").unwrap();
    let release = matches.contains_id("release");
    let target = matches.get_one::<String>("target").cloned();
    let verbose = matches.contains_id("verbose");
    let timeout = matches
        .get_one::<String>("timeout")
        .unwrap()
        .parse::<u64>()
        .context("Invalid timeout value")?;

    println!("{}", "🔌 Connecting to build cluster...".green());
    let mut stream = TcpStream::connect(node)
        .await
        .context("Failed to connect to build node")?;
    stream.set_nodelay(true)?;

    // Initial heartbeat check
    let heartbeat = BuildRequest::Heartbeat;
    let response = send_request(&mut stream, &heartbeat, Duration::from_secs(10)).await?;
    
    match response {
        BuildResponse::HeartbeatAck => {
            if verbose {
                println!("Successfully connected to build cluster");
            }
        },
        _ => return Err(anyhow::anyhow!("Invalid response from build cluster"))
    }

    // Read cargo metadata
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .context("Failed to read Cargo metadata")?;
        
    let package = metadata.root_package()
        .context("No package found in workspace")?;

    // Collect source files
    let current_dir = std::env::current_dir().context("Failed to get current directory")?;
    
    let mut source_files: Vec<_> = WalkDir::new(&current_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_path_buf())
        .filter(|path| should_include_file(path))
        .collect();

    // Always include Cargo.toml and Cargo.lock
    let cargo_toml = current_dir.join("Cargo.toml");
    let cargo_lock = current_dir.join("Cargo.lock");
    
    if cargo_toml.exists() {
        source_files.push(cargo_toml);
    }
    if cargo_lock.exists() {
        source_files.push(cargo_lock);
    }

    if source_files.is_empty() {
        return Err(anyhow::anyhow!("No source files found"));
    }

    if verbose {
        println!("Found {} source files", source_files.len());
        for file in &source_files {
            println!("  {}", file.display());
        }
        if let Some(target) = &target {
            println!("Building for target: {}", target);
        }
    }

    // Create progress bar for tarball creation
    let total_size: u64 = source_files
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum();
    
    let pb = create_progress_bar(total_size, "Preparing source files");

    // Create tarball
    let tarball_data = create_tarball(&source_files, &current_dir)?;
    pb.finish_with_message("✨ Source files prepared");

    // Prepare build unit
    let unit = BuildUnit {
        package_name: package.name.clone(),
        dependencies: package.dependencies.iter().map(|d| d.name.clone()).collect(),
        source_files,
        artifacts: package.targets
            .iter()
            .filter(|t| t.kind.iter().any(|k| k == "bin" || k == "lib"))
            .map(|t| PathBuf::from(&t.name))
            .collect(),
    };

    println!("{}", "🚀 Starting distributed build...".green());
    let request = BuildRequest::BuildUnit {
        unit,
        release,
        target: target.clone(),
        tarball_data,
    };

    let response = send_request(
        &mut stream,
        &request,
        Duration::from_secs(timeout),
    ).await?;

    match response {
        BuildResponse::BuildComplete { unit_name, artifacts } => {
            println!("{}", "✅ Build successful".green());
            println!("{}", "📥 Downloading artifacts...".yellow());

            let total_size: u64 = artifacts.iter().map(|(_, data)| data.len() as u64).sum();
            let pb = create_progress_bar(total_size, "Downloading artifacts");

            let mut target_dir = current_dir.join("target");
            if let Some(target_triple) = &target {
                target_dir = target_dir.join(target_triple);
            }
            target_dir = target_dir.join(if release { "release" } else { "debug" });

            tokio::fs::create_dir_all(&target_dir).await?;

            for (path, data) in artifacts {
                let target_path = target_dir.join(path);
                
                if let Some(parent) = target_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                
                pb.inc(data.len() as u64);

                tokio::fs::write(&target_path, data).await
                    .with_context(|| format!("Failed to write artifact to {}", target_path.display()))?;
            }

            pb.finish_with_message("✨ Artifacts downloaded successfully!");
            println!("{}", "Build process complete! 🎉".green());
        },
        BuildResponse::BuildError { unit_name, error } => {
            eprintln!("{}: {} - {}", "Build Error".red(), unit_name, error);
            std::process::exit(1);
        },
        _ => {
            eprintln!("Unexpected response from build cluster");
            std::process::exit(1);
        }
    }

    Ok(())
}