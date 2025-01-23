use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::{Parser, Subcommand};
use colored::*;
use flate2::{Compression, write::GzEncoder};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::Regex;
use serde::{Serialize, Deserialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
    fs,
    io::{self, Write},
};
use tar::Builder;
use tokio::{
    net::TcpStream,
    io::{AsyncReadExt, AsyncWriteExt},
    time,
};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    #[command(subcommand)]
    command: DistributedBuildCommand,
}

#[derive(Subcommand, Debug)]
enum DistributedBuildCommand {
    /// Distributed build command
    Build {
        #[arg(short, long, default_value = "localhost:9876")]
        node: String,

        #[arg(short, long)]
        release: bool,

        #[arg(short, long)]
        verbose: bool,

        #[arg(short, long, default_value = "600")]
        timeout: u64,
    },
}

#[derive(Serialize, Deserialize, Debug)]
enum BuildRequest {
    BuildUnit {
        unit: BuildUnit,
        release: bool,
        tarball_data: Vec<u8>,
    },
    TransferArtifact {
        from_unit: String,
        artifact_path: PathBuf,
    },
    Heartbeat,
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
    HeartbeatAck,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BuildUnit {
    package_name: String,
    dependencies: Vec<String>,
    source_files: Vec<PathBuf>,
    artifacts: Vec<PathBuf>,
}

fn get_gitignore_patterns() -> Vec<String> {
    let mut patterns = vec![
        ".git".to_string(),
        "target".to_string(),
        "node_modules".to_string(),
        "Cargo.lock".to_string(),
    ];

    if let Ok(content) = fs::read_to_string(".gitignore") {
        patterns.extend(
            content.lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .map(|line| line.trim().to_string())
        );
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

fn create_source_tarball(
    package_name: &str, 
    source_files: &[PathBuf]
) -> Result<Vec<u8>> {
    let temp_dir = tempfile::tempdir()
        .context("Failed to create temporary directory")?;
    let temp_path = temp_dir.path();
    let patterns = get_gitignore_patterns();

    let mut added_files = HashSet::new();
    let mut tarball = Vec::new();

    {
        let mut encoder = GzEncoder::new(&mut tarball, Compression::default());
        let mut tar = Builder::new(&mut encoder);

        // Find the project root (directory containing Cargo.toml)
        let project_root = source_files[0].ancestors()
            .find(|p| p.join("Cargo.toml").exists())
            .context("Could not find project root")?;

        for source_path in source_files {
            if !source_path.exists() {
                eprintln!("Warning: Source file not found: {}", source_path.display());
                continue;
            }

            if is_path_ignored(source_path, &patterns) {
                continue;
            }

            // Create relative path from project root
            let relative_path = source_path.strip_prefix(project_root)
                .context("Failed to get relative path")?;
            
            let dest_path = temp_path.join(relative_path);
            
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)
                    .context("Failed to create parent directory")?;
            }
            
            if !added_files.contains(&dest_path) {
                fs::copy(source_path, &dest_path)
                    .with_context(|| format!("Failed to copy file: {}", source_path.display()))?;
                added_files.insert(dest_path.clone());

                // Use relative_path for tar entry to maintain original structure
                tar.append_path_with_name(&dest_path, relative_path)
                    .with_context(|| format!("Failed to add file to tarball: {}", relative_path.display()))?;
            }
        }

        // Also add Cargo.toml to ensure project structure is preserved
        let cargo_toml = source_files[0].ancestors()
            .find(|p| p.join("Cargo.toml").exists())
            .context("Could not find Cargo.toml")?
            .join("Cargo.toml");
        
        if cargo_toml.exists() {
            let relative_path = cargo_toml.strip_prefix(project_root)?;
            let dest_path = temp_path.join(relative_path);
            
            fs::copy(&cargo_toml, &dest_path)?;
            tar.append_path_with_name(&dest_path, relative_path)?;
        }

        tar.finish()
            .context("Failed to finalize tar archive")?;
    }
    
    Ok(tarball)
}

async fn send_request(
    stream: &mut TcpStream, 
    request: &BuildRequest, 
    timeout_duration: Duration
) -> Result<BuildResponse> {
    time::timeout(timeout_duration, async {
        let data = bincode::serialize(request)
            .context("Failed to serialize request")?;
        let len = (data.len() as u32).to_be_bytes();
        
        stream.write_all(&len).await?;
        stream.write_all(&data).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf);

        let mut buf = vec![0; len as usize];
        stream.read_exact(&mut buf).await?;
        
        bincode::deserialize(&buf)
            .context("Failed to deserialize response")
    }).await.context("Request timed out")?
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    match args.command {
        DistributedBuildCommand::Build { 
            node, 
            release, 
            verbose, 
            timeout 
        } => {
            let multi_progress = MultiProgress::new();
            let overall_pb = multi_progress.add(ProgressBar::new(100));
            overall_pb.set_style(ProgressStyle::default_bar()
                .template("{msg} {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {percent}%")?
                .progress_chars("#>-"));

            println!("{}", "🔌 Connecting to build cluster...".green());
            let mut stream = TcpStream::connect(&node).await
                .context("Failed to connect to build node")?;
            stream.set_nodelay(true)?;

            let heartbeat_response = send_request(
                &mut stream, 
                &BuildRequest::Heartbeat, 
                Duration::from_secs(10)
            ).await?;

            if !matches!(heartbeat_response, BuildResponse::HeartbeatAck) {
                return Err(anyhow::anyhow!("Invalid response from build node"));
            }

            let metadata = MetadataCommand::new().exec()?;
            let package = metadata.root_package()
                .context("No package found in current directory")?;

            let current_dir = std::env::current_dir()?;
            let ignore_patterns = get_gitignore_patterns();
            
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
                return Err(anyhow::anyhow!("No Rust source files found"));
            }

            for path in &source_files {
                if !path.exists() {
                    return Err(anyhow::anyhow!("Source file not found: {}", path.display()));
                }
            }

            let build_unit = BuildUnit {
                package_name: package.name.clone(),
                dependencies: package.dependencies.iter()
                    .map(|d| d.name.clone())
                    .collect(),
                source_files,
                artifacts: package.targets.iter()
                    .filter(|t| t.kind.iter().any(|k| k == "bin" || k == "lib"))
                    .map(|t| PathBuf::from(&t.name))
                    .collect(),
            };

            if verbose {
                println!("Build unit: {:#?}", build_unit);
            }

            let tarball_data = create_source_tarball(&package.name, &build_unit.source_files)
                .context("Failed to create source tarball")?;

            println!("{}", "🚀 Starting distributed build...".green());
            let build_request = BuildRequest::BuildUnit { 
                unit: build_unit, 
                release, 
                tarball_data 
            };

            let response = time::timeout(
                Duration::from_secs(timeout),
                send_request(&mut stream, &build_request, Duration::from_secs(timeout))
            ).await??;

            match response {
                BuildResponse::BuildComplete { unit_name, artifacts } => {
                    println!("{}", "✅ Build successful".green());
                    println!("{}", "📥 Downloading artifacts...".yellow());

                    let target_base = std::env::current_dir()?.join("target");
                    let target_dir = target_base.join(if release { "release" } else { "debug" });
                    fs::create_dir_all(&target_dir)?;

                    for (path, data) in artifacts {
                        let artifact_path = target_dir.join(path);
                        
                        if verbose {
                            println!("Writing artifact: {}", artifact_path.display());
                        }

                        if let Some(parent) = artifact_path.parent() {
                            fs::create_dir_all(parent)?;
                        }

                        fs::write(&artifact_path, data)?;
                    }

                    println!("{}", "Build process complete! 🎉".green());
                }
                BuildResponse::BuildError { unit_name, error } => {
                    eprintln!("{}: {} - {}", 
                        "Build Error".red(), 
                        unit_name, 
                        error
                    );
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Unexpected response from build cluster");
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}