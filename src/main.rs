use clap::{Command, Arg};
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use walkdir::WalkDir;
use std::path::{Path, PathBuf};
use std::io;
use serde::{Serialize, Deserialize};
use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use std::sync::Arc;
use std::time::Duration;
use colored::*;

/// Sanitizes and validates file paths
fn sanitize_path(original_path: &Path) -> Result<PathBuf, io::Error> {
    // Convert to absolute path
    let base_dir = std::env::current_dir()?;
    
    // Sanitize filename
    let clean_filename = original_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .map(|name| name
            .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_")
            .trim_start_matches('.')
            .to_string()
        )
        .unwrap_or_else(|| "unknown_file".to_string());
    
    // Construct a safe path
    let relative_path = original_path
        .strip_prefix(&base_dir)
        .unwrap_or(original_path);
    
    let safe_path = base_dir.join(relative_path)
        .parent()
        .map(|parent| parent.join(&clean_filename))
        .unwrap_or_else(|| base_dir.join(&clean_filename));
    
    // Canonicalize to resolve any remaining path issues
    safe_path.canonicalize()
}

#[derive(Serialize, Deserialize, Debug)]
enum BuildRequest {
    StartBuild { 
        package_name: String,
        release: bool 
    },
    UploadSource {
        path: String,
        data: Vec<u8>
    },
    DownloadArtifact {
        path: String
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum BuildResponse {
    BuildStarted,
    BuildComplete { success: bool },
    BuildError { message: String },
    ArtifactData { data: Vec<u8> }
}

/// Filters files to be uploaded, excluding common build and VCS directories
fn should_upload_file(path: &Path) -> bool {
    let ignored_dirs = [
        ".git", ".gitignore", 
        "target", "node_modules", 
        ".cargo", ".vscode", 
        ".idea"
    ];

    let path_str = path.to_string_lossy();
    !ignored_dirs.iter().any(|&dir| path_str.contains(dir)) &&
    path.is_file() &&
    // Limit file size to prevent overwhelming transfer
    !path.metadata().map(|m| m.len() > 50_000_000).unwrap_or(false)
}

/// Creates a styled progress bar
fn create_progress_bar(len: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(ProgressStyle::default_bar()
        .template("{msg} {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}")
        .progress_chars("#>-"));
    pb.set_message(message);
    pb
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command-line arguments
    let matches = Command::new("cargo-tess")
        .bin_name("cargo")
        .subcommand(
            Command::new("tess")
                .arg(
                    Arg::new("node")
                        .short('n')
                        .long("node")
                        .help("Address of any node in cluster (host:port)")
                        .default_value("localhost:9876")
                )
                .arg(
                    Arg::new("release")
                        .long("release")
                        .help("Build with release profile")
                )
                .arg(
                    Arg::new("verbose")
                        .short('v')
                        .long("verbose")
                        .help("Enable verbose logging")
                )
        )
        .get_matches();

    // Exit if not the tess subcommand
    let Some(matches) = matches.subcommand_matches("tess") else {
        println!("Use 'cargo tess' to run the distributed build.");
        return Ok(());
    };

    // Configuration
    let node = matches.get_one::<String>("node").unwrap();
    let release = matches.contains_id("release");
    let verbose = matches.contains_id("verbose");

    // Prepare connection
    println!("{}", "🔌 Connecting to build cluster...".green());
    let mut stream = TcpStream::connect(node).await?;

    // Collect files to upload
    let files: Vec<_> = WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|entry| should_upload_file(entry.path()))
        .collect();

    // Multiprogressbar for upload
    let multi_progress = Arc::new(MultiProgress::new());
    let overall_pb = multi_progress.add(create_progress_bar(
        files.iter().map(|f| f.metadata().unwrap().len()).sum(), 
        "Uploading source files"
    ));

    // Upload source files with progress tracking
    println!("{}", "📤 Preparing to upload source files...".yellow());
    for entry in files {
        let data = tokio::fs::read(entry.path()).await?;
        
        // Use sanitized path
        let sanitized_path = match sanitize_path(entry.path()) {
            Ok(path) => path.to_string_lossy().to_string(),
            Err(e) => {
                eprintln!("Path sanitization error: {}", e);
                continue;
            }
        };

        let request = BuildRequest::UploadSource {
            path: sanitized_path,
            data,
        };

        // Serialize and send request
        stream.write_all(&bincode::serialize(&request)?).await?;
        
        // Read response
        let mut buf = vec![0; 1024];
        let n = stream.read(&mut buf).await?;
        let response: BuildResponse = bincode::deserialize(&buf[..n])?;
        
        // Handle potential upload errors
        if let BuildResponse::BuildError { message } = response {
            eprintln!("{} {}: {}", 
                "❌ Failed to upload".red(), 
                entry.path().display(), 
                message
            );
            return Ok(());
        }

        // Update progress bar
        overall_pb.inc(entry.metadata().unwrap().len());
    }
    overall_pb.finish_with_message("✨ Upload complete");

    // Start build
    println!("{}", "🚀 Starting distributed build...".green());
    let request = BuildRequest::StartBuild {
        package_name: env!("CARGO_PKG_NAME").to_string(),
        release,
    };
    stream.write_all(&bincode::serialize(&request)?).await?;

    // Build progress tracking
    let build_pb = create_progress_bar(100, "Building project");
    build_pb.enable_steady_tick(120);

    // Read build response
    let mut buf = vec![0; 1024 * 1024];
    let n = stream.read(&mut buf).await?;
    match bincode::deserialize(&buf[..n])? {
        BuildResponse::BuildComplete { success } => {
            if !success {
                build_pb.finish_with_message("❌ Build failed");
                eprintln!("{}", "Build process encountered errors".red());
                return Ok(());
            }
            build_pb.finish_with_message("✅ Build successful");
        },
        BuildResponse::BuildError { message } => {
            build_pb.finish_with_message("❌ Build failed");
            eprintln!("{}: {}", "Build error".red(), message);
            return Ok(());
        },
        _ => {}
    }

    // Download artifacts
    println!("{}", "📥 Downloading build artifacts...".yellow());
    let request = BuildRequest::DownloadArtifact {
        path: "target".to_string()
    };
    stream.write_all(&bincode::serialize(&request)?).await?;

    // Artifact download progress
    let artifact_pb = create_progress_bar(1024 * 1024, "Downloading artifacts");

    // Read artifact response
    let n = stream.read(&mut buf).await?;
    match bincode::deserialize(&buf[..n])? {
        BuildResponse::ArtifactData { data } => {
            // Use standard path handling
            let target_dir = std::env::current_dir()?.join("target");
            std::fs::create_dir_all(&target_dir)?;
            tokio::fs::write(target_dir.join("artifacts.tar.gz"), data).await?;
            artifact_pb.finish_with_message("✨ Artifacts downloaded successfully!");
            println!("{}", "Build process complete! 🎉".green());
        },
        BuildResponse::BuildError { message } => {
            artifact_pb.finish_with_message("❌ Artifact download failed");
            eprintln!("{}: {}", "Artifact download error".red(), message);
        },
        _ => {}
    }

    Ok(())
}