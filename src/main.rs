use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::Parser;
use colored::*;
use flate2::{write::GzEncoder, Compression};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tar::Builder;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};
use tracing::{error, info, warn, Level};
use tracing_subscriber::{FmtSubscriber};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
enum Cargo {
    #[command(name = "tess")]
    Tesseract(CliArgs),
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// Server address (host:port)
    #[arg(short, long)]
    server: String,

    /// Build in release mode
    #[arg(short, long)]
    release: bool,

    /// Target triple (e.g., x86_64-pc-windows-msvc)
    #[arg(short, long)]
    target: Option<String>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Number of retry attempts for failed builds
    #[arg(short = 'n', long, default_value = "3")]
    retries: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct BuildUnit {
    package_name: String,
    dependencies: Vec<String>,
    source_files: Vec<PathBuf>,
    artifacts: Vec<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
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
    Heartbeat,
}

#[derive(Debug, Serialize, Deserialize)]
enum BuildResponse {
    BuildOutput {
        unit_name: String,
        output: String,
        is_error: bool,
    },
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

struct BuildProgress {
    package_bar: ProgressBar,
    build_output: Vec<String>,
}

struct TesseractClient {
    server_addr: String,
    release: bool,
    target: Option<String>,
    workspace_path: PathBuf,
    progress: Arc<Mutex<HashMap<String, BuildProgress>>>,
    multi_progress: MultiProgress,
    retries: u32,
}

impl TesseractClient {
    fn new(
        server_addr: String,
        release: bool,
        target: Option<String>,
        retries: u32,
    ) -> Result<Self> {
        let workspace_path = std::env::current_dir()?;
        Ok(Self {
            server_addr,
            release,
            target,
            workspace_path,
            progress: Arc::new(Mutex::new(HashMap::new())),
            multi_progress: MultiProgress::new(),
            retries,
        })
    }

    fn create_progress_bar(&self, msg: &str) -> ProgressBar {
        let pb = self.multi_progress.add(ProgressBar::new(100));
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
        );
        pb.set_message(msg.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));
        pb
    }

    fn read_gitignore(path: &Path) -> Vec<String> {
        let mut patterns = vec![
            ".git".to_string(),
            "target".to_string(),
            "Cargo.lock".to_string(),
        ];

        if let Ok(content) = std::fs::read_to_string(path.join(".gitignore")) {
            patterns.extend(content
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .map(|line| line.trim().to_string()));
        }
        patterns
    }

    fn is_ignored(path: &Path, workspace_root: &Path, ignore_patterns: &[String]) -> bool {
        let relative_path = path.strip_prefix(workspace_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());

        for pattern in ignore_patterns {
            let pattern = pattern.trim_matches('/');
            if pattern.contains('*') {
                let regex_pattern = pattern
                    .replace(".", "\\.")
                    .replace("**/", "(.*/)?")
                    .replace("*", "[^/]*");
                if let Ok(regex) = regex::Regex::new(&format!("^{}$", regex_pattern)) {
                    if regex.is_match(&relative_path) {
                        return true;
                    }
                }
            } else if relative_path.contains(pattern) {
                return true;
            }
        }
        false
    }

    fn create_tarball(unit: &BuildUnit) -> Result<Vec<u8>> {
        let all_manifests: Vec<_> = unit.source_files.iter()
            .filter(|p| p.ends_with("Cargo.toml"))
            .collect();

        // Find workspace root
        let workspace_root = all_manifests.iter()
            .filter_map(|p| p.parent())
            .min_by_key(|p| p.components().count())
            .ok_or_else(|| anyhow::anyhow!("Could not find workspace root"))?
            .to_path_buf();

        // Find package root by parsing Cargo.toml files
        let package_root = all_manifests.iter()
            .filter_map(|p| {
                let dir = p.parent()?;
                if let Ok(content) = std::fs::read_to_string(p) {
                    if content.contains(&format!("name = \"{}\"", unit.package_name)) {
                        return Some(dir.to_path_buf());
                    }
                }
                None
            })
            .next()
            .ok_or_else(|| anyhow::anyhow!("Could not find package root"))?;

        info!("Creating tarball:");
        info!("Workspace root: {}", workspace_root.display());
        info!("Package root: {}", package_root.display());

        // Read gitignore patterns
        let ignore_patterns = Self::read_gitignore(&workspace_root);

        // Create temporary directory for staging
        let temp_dir = tempfile::tempdir()?;
        let temp_path = temp_dir.path();

        // Copy workspace files
        for entry in walkdir::WalkDir::new(&workspace_root) {
            let entry = entry?;
            let path = entry.path();

            if Self::is_ignored(path, &workspace_root, &ignore_patterns) {
                continue;
            }

            let relative_path = path.strip_prefix(&workspace_root)?;
            let dest_path = temp_path.join(relative_path);

            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&dest_path)?;
            } else {
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(path, &dest_path)?;
                info!("Copied: {} -> {}", relative_path.display(), dest_path.display());
            }
        }

        // List final directory structure
        info!("Final directory structure:");
        for entry in walkdir::WalkDir::new(temp_path) {
            if let Ok(entry) = entry {
                if let Ok(relative) = entry.path().strip_prefix(temp_path) {
                    info!("  {}", relative.display());
                }
            }
        }

        // Create tarball
        let mut tarball = Vec::new();
        let encoder = GzEncoder::new(&mut tarball, Compression::default());
        let mut tar = Builder::new(encoder);
        tar.append_dir_all(".", temp_path)?;
        tar.finish()?;
        drop(tar);

        Ok(tarball)
    }

    fn add_file(path: &Path, tar: &mut Builder<GzEncoder<&mut Vec<u8>>>) -> Result<()> {
        if path.exists() {
            let relative_path = path.strip_prefix(path.parent().unwrap())?;
            info!("Adding to tarball: {}", relative_path.display());
            tar.append_path_with_name(path, relative_path)?;
        }
        Ok(())
    }

    async fn write_artifact_safely(path: &Path, data: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let tmp_path = path.with_extension(format!("{}.tmp", std::process::id()));
        tokio::fs::write(&tmp_path, data).await?;

        #[cfg(windows)]
        {
            use tokio::fs;
            let old_path = path.with_extension(format!("{}.old", std::process::id()));
            
            if path.exists() {
                match fs::rename(path, &old_path).await {
                    Ok(_) => (),
                    Err(e) => {
                        fs::remove_file(&tmp_path).await?;
                        return Err(anyhow::anyhow!(
                            "Could not replace existing file - it may be in use: {}",
                            e
                        ));
                    }
                }
            }

            if let Err(e) = fs::rename(&tmp_path, path).await {
                if old_path.exists() {
                    let _ = fs::rename(&old_path, path).await;
                }
                return Err(anyhow::anyhow!("Failed to move new file into place: {}", e));
            }

            if old_path.exists() {
                let _ = fs::remove_file(&old_path).await;
            }
        }

        #[cfg(not(windows))]
        {
            tokio::fs::rename(&tmp_path, path).await?;
        }

        Ok(())
    }

    async fn handle_build_stream(&self, mut stream: TcpStream, unit: &BuildUnit) -> Result<()> {
        let mut progress = self.progress.lock().await;
        let build_progress = progress
            .entry(unit.package_name.clone())
            .or_insert_with(|| BuildProgress {
                package_bar: self.create_progress_bar(&format!("Building {}", unit.package_name)),
                build_output: Vec::new(),
            });

        loop {
            let mut len_buf = [0u8; 4];
            match stream.read_exact(&mut len_buf).await {
                Ok(_) => (),
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        return Err(anyhow::anyhow!("Server connection closed unexpectedly"));
                    }
                    return Err(e.into());
                }
            }

            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0; len];
            stream.read_exact(&mut buf).await?;

            match bincode::deserialize(&buf)? {
                BuildResponse::BuildOutput { output, is_error, .. } => {
                    let output = if is_error {
                        output.red().to_string()
                    } else {
                        output.green().to_string()
                    };
                    println!("{}", output);
                    build_progress.build_output.push(output);
                }
                BuildResponse::BuildComplete { unit_name, artifacts } => {
                    build_progress.package_bar.set_message(format!("Building {} - Saving artifacts", unit_name));
                    
                    for (path, data) in artifacts {
                        let target_path = if let Some(ref target) = self.target {
                            self.workspace_path
                                .join("target")
                                .join(target)
                                .join(if self.release { "release" } else { "debug" })
                                .join(&path)
                        } else {
                            self.workspace_path
                                .join("target")
                                .join(if self.release { "release" } else { "debug" })
                                .join(&path)
                        };

                        info!("Writing artifact to {}", target_path.display());
                        Self::write_artifact_safely(&target_path, &data).await
                            .with_context(|| format!("Failed to write artifact to {}", target_path.display()))?;
                        info!("Successfully wrote artifact: {}", target_path.display());
                    }
                    
                    build_progress.package_bar.finish_with_message(
                        format!("{} built successfully", unit_name).green().to_string(),
                    );
                    return Ok(());
                }
                BuildResponse::BuildError { unit_name, error } => {
                    build_progress.package_bar.finish_with_message(
                        format!("{} build failed", unit_name).red().to_string(),
                    );
                    return Err(anyhow::anyhow!("Build failed: {}", error));
                }
                _ => {}
            }
        }
    }

    fn discover_build_units(&self) -> Result<Vec<BuildUnit>> {
        let metadata = MetadataCommand::new()
            .current_dir(&self.workspace_path)
            .no_deps()
            .exec()?;

        let mut units = Vec::new();

        for package in metadata.packages {
            let manifest_dir = Path::new(&package.manifest_path).parent().unwrap();
            info!("Processing package {} at {}", package.name, manifest_dir.display());

            let mut source_files = Vec::new();
            source_files.push(package.manifest_path.into());

            let workspace_manifest = Path::new(&metadata.workspace_root).join("Cargo.toml");
            if workspace_manifest.exists() {
                source_files.push(workspace_manifest);
            }

            for target in &package.targets {
                if target.kind.iter().any(|k| k == "lib" || k == "bin") {
                    let src_path = Path::new(&target.src_path);
                    let src_dir = src_path.parent().unwrap();
                    
                    info!("Scanning directory: {}", src_dir.display());
                    
                    for entry in WalkDir::new(src_dir) {
                        match entry {
                            Ok(entry) => {
                                if entry.path().extension().map_or(false, |ext| ext == "rs") {
                                    info!("Found source file: {}", entry.path().display());
                                    source_files.push(entry.path().to_path_buf());
                                }
                            }
                            Err(e) => warn!("Error walking directory: {}", e),
                        }
                    }
                }
            }

            let unit = BuildUnit {
                package_name: package.name.clone(),
                dependencies: package
                    .dependencies
                    .iter()
                    .map(|d| d.name.clone())
                    .collect(),
                source_files,
                artifacts: package
                    .targets
                    .iter()
                    .filter(|t| t.kind.iter().any(|k| k == "lib" || k == "bin"))
                    .map(|t| PathBuf::from(&t.name))
                    .collect(),
            };

            units.push(unit);
        }

        Ok(units)
    }

    async fn build_unit(&self, unit: BuildUnit, attempt: u32) -> Result<()> {
        info!("Building package {} (attempt {})", unit.package_name, attempt);

        let mut stream = TcpStream::connect(&self.server_addr)
            .await
            .context("Failed to connect to build server")?;

        stream.set_nodelay(true)?;

        info!("Creating tarball for {}", unit.package_name);
        let tarball = Self::create_tarball(&unit)
            .context("Failed to create source tarball")?;
        info!("Created tarball of {} bytes", tarball.len());

        let request = BuildRequest::BuildUnit {
            unit: unit.clone(),
            release: self.release,
            target: self.target.clone(),
            tarball_data: tarball,
        };

        info!("Serializing build request");
        let data = bincode::serialize(&request)
            .context("Failed to serialize build request")?;
        info!("Request size: {} bytes", data.len());

        let len = (data.len() as u32).to_be_bytes();
        stream.write_all(&len).await
            .context("Failed to send message length")?;
        stream.write_all(&data).await
            .context("Failed to send build request")?;

        info!("Request sent, waiting for build stream");
        self.handle_build_stream(stream, &unit).await?;

        Ok(())
    }

    pub async fn build(&self) -> Result<()> {
        info!("Discovering build units in workspace...");
        let units = self.discover_build_units()?;
        info!("Found {} build units", units.len());

        for unit in units {
            let mut last_error = None;
            for attempt in 1..=self.retries {
                match self.build_unit(unit.clone(), attempt).await {
                    Ok(_) => {
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        last_error = Some(e);
                        if attempt < self.retries {
                            warn!(
                                "Build attempt {} failed for {}, retrying in 2 seconds...",
                                attempt, unit.package_name
                            );
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
            
            if let Some(e) = last_error {
                return Err(e.context(format!("Failed to build {} after {} attempts", unit.package_name, self.retries)));
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let Cargo::Tesseract(args) = Cargo::parse();

    let log_level = if args.debug { Level::DEBUG } else { Level::INFO };
    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting Tesseract client");
    info!(
        "Server: {}, Release: {}, Target: {:?}",
        args.server, args.release, args.target
    );

    let client = TesseractClient::new(
        args.server,
        args.release,
        args.target,
        args.retries,
    )?;

    if let Err(e) = client.build().await {
        error!("Build failed: {:#}", e);
        std::process::exit(1);
    }

    Ok(())
}