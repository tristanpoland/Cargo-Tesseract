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
use regex::Regex;

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

async fn get_gitignore_patterns() -> Vec<String> {
    let mut patterns = vec![
        ".git".to_string(),
        "target".to_string(),
        "node_modules".to_string(),
        "builds".to_string(),
        "Cargo.lock".to_string(),
    ];

    if let Ok(content) = tokio::fs::read_to_string(".gitignore").await {
        patterns.extend(content.lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .map(|line| line.trim().to_string()));
    }
    patterns
}

fn is_path_ignored(path: &Path, patterns: &[String]) -> bool {
    let path_str = path.to_string_lossy();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches('/').trim_end_matches('/');
        if pattern.contains('*') {
            let regex = glob_to_regex(pattern);
            regex.is_match(&path_str)
        } else {
            path_str.contains(pattern)
        }
    })
}

fn glob_to_regex(pattern: &str) -> Regex {
    let regex_pattern = pattern
        .replace(".", "\\.")
        .replace("**/", "(.*/)?")
        .replace("*", "[^/]*")
        .replace("?", ".");
    Regex::new(&format!("^{}$", regex_pattern)).unwrap_or_else(|_| Regex::new("^$").unwrap())
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
                    .help("Enable verbose logging"))
                .arg(Arg::new("timeout")
                    .short('t')
                    .long("timeout")
                    .help("Build timeout in seconds")
                    .default_value("300")))
        .get_matches();

    let Some(matches) = matches.subcommand_matches("tess") else {
        println!("Use 'cargo tess' to run the distributed build.");
        return Ok(());
    };

    let node = matches.get_one::<String>("node").unwrap();
    let release = matches.contains_id("release");
    let verbose = matches.contains_id("verbose");
    let timeout = matches.get_one::<String>("timeout")
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(300);

    println!("{}", "🔌 Connecting to build cluster...".green());
    let mut stream = TcpStream::connect(node).await?;
    stream.set_nodelay(true)?;

    let heartbeat = BuildRequest::Heartbeat;
    let response = send_request(&mut stream, &heartbeat).await?;
    match response {
        BuildResponse::HeartbeatAck => {},
        _ => return Err("Failed to connect to build cluster".into())
    }

    let metadata = cargo_metadata::MetadataCommand::new().exec()?;
    let package = metadata.root_package().ok_or("No package found")?;

    let ignore_patterns = get_gitignore_patterns().await;
    
    let current_dir = std::env::current_dir()?;
    let source_files: Vec<_> = WalkDir::new(&current_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|entry| {
            let path = entry.path();
            let is_manifest = path.file_name().map_or(false, |name| name == "Cargo.toml") &&
                path.parent().map_or(false, |p| p == current_dir);
            let is_source = path.is_file() && 
                (path.extension().map_or(false, |ext| ext == "rs") || is_manifest);
            is_source && !is_path_ignored(path, &ignore_patterns)
        })
        .map(|entry| entry.path().to_owned())
        .collect();

    if source_files.is_empty() {
        return Err("No Rust source files found in current directory".into());
    }

    for path in &source_files {
        if !path.exists() {
            return Err(format!("Source file not found: {}", path.display()).into());
        }
    }

    let overall_pb = create_progress_bar(
        source_files.iter()
            .filter_map(|p| p.metadata().ok())
            .map(|m| m.len())
            .sum(),
        "Preparing source files"
    );

    for path in &source_files {
        if let Ok(metadata) = path.metadata() {
            overall_pb.inc(metadata.len());
        }
    }

    overall_pb.finish_with_message("✨ Source files prepared");

    let unit = BuildUnit {
        package_name: package.name.clone(),
        dependencies: package.dependencies.iter().map(|d| d.name.clone()).collect(),
        source_files,
        artifacts: package.targets.iter()
            .filter(|t| t.kind.iter().any(|k| k == "bin" || k == "lib"))
            .map(|t| PathBuf::from(&t.name))
            .collect(),
    };

    if verbose {
        println!("Build unit: {:#?}", unit);
    }

    println!("{}", "🚀 Starting distributed build...".green());
    let request = BuildRequest::BuildUnit { unit, release };

    let response = tokio::time::timeout(
        Duration::from_secs(timeout),
        send_request(&mut stream, &request)
    ).await.map_err(|_| "Build timed out")??;

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

                if verbose {
                    println!("Writing artifact to: {}", target_path.display());
                }

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
            std::process::exit(1);
        },
        _ => {
            eprintln!("Unexpected response from build cluster");
            std::process::exit(1);
        }
    }

    Ok(())
}