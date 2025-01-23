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

fn sanitize_path(original_path: &Path) -> Result<PathBuf, io::Error> {
    let base_dir = std::env::current_dir()?;
    let clean_filename = original_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .map(|name| name
            .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_")
            .trim_start_matches('.')
            .to_string()
        )
        .unwrap_or_else(|| "unknown_file".to_string());
    
    let relative_path = original_path
        .strip_prefix(&base_dir)
        .unwrap_or(original_path);
    
    let safe_path = base_dir.join(relative_path)
        .parent()
        .map(|parent| parent.join(&clean_filename))
        .unwrap_or_else(|| base_dir.join(&clean_filename));
    
    safe_path.canonicalize()
}

fn should_upload_file(path: &Path) -> bool {
    let ignored_dirs = [
        ".git", ".gitignore", "target", "node_modules", 
        ".cargo", ".vscode", ".idea", "builds"
    ];

    let path_str = path.to_string_lossy();
    !ignored_dirs.iter().any(|&dir| path_str.contains(dir)) &&
    path.is_file() &&
    !path.metadata().map(|m| m.len() > 50_000_000).unwrap_or(false)
}

fn create_progress_bar(len: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(ProgressStyle::default_bar()
        .template("{msg} {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}")
        .progress_chars("#>-"));
    pb.set_message(message);
    pb
}

async fn send_request(stream: &mut TcpStream, request: &BuildRequest) -> Result<BuildResponse, io::Error> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let data = bincode::serialize(request).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let len = (data.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&data).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0; len];
        stream.read_exact(&mut buf).await?;
        bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Request timed out"))?
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let matches = Command::new("cargo-tess")
        .bin_name("cargo")
        .subcommand(
            Command::new("tess")
                .arg(Arg::new("node")
                    .short('n')
                    .long("node")
                    .help("Address of any node in cluster (host:port)")
                    .default_value("localhost:9876"))
                .arg(Arg::new("release")
                    .long("release")
                    .help("Build with release profile"))
                .arg(Arg::new("verbose")
                    .short('v')
                    .long("verbose")
                    .help("Enable verbose logging")))
        .get_matches();

    let Some(matches) = matches.subcommand_matches("tess") else {
        println!("Use 'cargo tess' to run the distributed build.");
        return Ok(());
    };

    let node = matches.get_one::<String>("node").unwrap();
    let release = matches.contains_id("release");

    println!("{}", "🔌 Connecting to build cluster...".green());
    let mut stream = TcpStream::connect(node).await?;
    stream.set_nodelay(true)?;

    // Send initial heartbeat
    let heartbeat = BuildRequest::Heartbeat;
    let response = send_request(&mut stream, &heartbeat).await?;
    match response {
        BuildResponse::HeartbeatAck => {},
        _ => return Err("Failed to connect to build cluster".into())
    }

    // Read cargo manifest
    let metadata = cargo_metadata::MetadataCommand::new().exec()?;
    let package = metadata.root_package().ok_or("No package found")?;

    // Create tarball of source files
    let temp_dir = tempfile::tempdir()?;
    let tar_path = temp_dir.path().join("source.tar.gz");
    let file = std::fs::File::create(&tar_path)?;
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);

    let files: Vec<_> = WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|entry| should_upload_file(entry.path()))
        .collect();

    let overall_pb = create_progress_bar(
        files.iter().map(|f| f.metadata().unwrap().len()).sum(), 
        "Preparing source files"
    );

    for entry in &files {
        let path = entry.path();
        tar.append_path_with_name(path, path.strip_prefix("./").unwrap_or(path))?;
        overall_pb.inc(entry.metadata().unwrap().len());
    }

    tar.finish()?;
    overall_pb.finish_with_message("✨ Source files prepared");

    let unit = BuildUnit {
        package_name: package.name.clone(),
        dependencies: package.dependencies.iter().map(|d| d.name.clone()).collect(),
        source_files: files.iter().map(|f| f.path().to_path_buf()).collect(),
        artifacts: package.targets.iter().map(|t| PathBuf::from(&t.name)).collect(),
    };

    println!("{}", "🚀 Starting distributed build...".green());
    let request = BuildRequest::BuildUnit { unit, release };
    let response = send_request(&mut stream, &request).await?;

    let build_pb = create_progress_bar(100, "Building project");
    build_pb.enable_steady_tick(120);

    match response {
        BuildResponse::BuildComplete { unit_name, artifacts } => {
            build_pb.finish_with_message("✅ Build successful");
            println!("{}", "📥 Downloading artifacts...".yellow());

            let artifacts_pb = create_progress_bar(
                artifacts.iter().map(|(_, data)| data.len() as u64).sum(), 
                "Downloading artifacts"
            );

            for (path, data) in artifacts {
                let target_path = std::env::current_dir()?.join("target")
                    .join(if release { "release" } else { "debug" })
                    .join(path);

                if let Some(parent) = target_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let data_len = data.len() as u64;
                tokio::fs::write(&target_path, data).await?;
                artifacts_pb.inc(data_len);
            }

            artifacts_pb.finish_with_message("✨ Artifacts downloaded successfully!");
            println!("{}", "Build process complete! 🎉".green());
        },
        BuildResponse::BuildError { unit_name, error } => {
            build_pb.finish_with_message("❌ Build failed");
            eprintln!("{}: {} - {}", "Build error".red(), unit_name, error);
        },
        _ => eprintln!("Unexpected response from build cluster")
    }

    Ok(())
}