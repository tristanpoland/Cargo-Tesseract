use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::Parser;
use colored::*;
use flate2::{write::GzEncoder, Compression};
use serde::{Serialize, Deserialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};
use tracing::{error, info, warn, Level};
use tracing_subscriber::{FmtSubscriber};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// Server address (host:port)
    #[arg(short, long)]
    server: String,

    /// Build in release mode
    #[arg(short, long)]
    release: bool,

    /// Target triple
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
            ProgressStyle::default_bar()
                .template("{msg} {bar:40.cyan/blue} {spinner}")
                .unwrap()
                .progress_chars("=> "),
        );
        pb.set_message(msg.to_string());
        pb
    }

    fn create_tarball(unit: &BuildUnit) -> Result<Vec<u8>> {
        let root_dir = unit.source_files
            .iter()
            .find_map(|path| {
                let mut current = path.parent()?;
                while let Some(parent) = current.parent() {
                    if current.join("Cargo.toml").exists() {
                        return Some(current.to_path_buf());
                    }
                    current = parent;
                }
                None
            })
            .ok_or_else(|| anyhow::anyhow!("Could not find directory containing Cargo.toml"))?;

        let mut tarball = Vec::new();
        let encoder = GzEncoder::new(&mut tarball, Compression::default());
        let mut tar = tar::Builder::new(encoder);

        // Add Cargo.toml and Cargo.lock from root
        let mut add_file = |path: &Path| -> Result<()> {
            if path.exists() {
                let relative_path = path.strip_prefix(&root_dir)?;
                tar.append_path_with_name(path, relative_path)?;
            }
            Ok(())
        };

        add_file(&root_dir.join("Cargo.toml"))?;
        add_file(&root_dir.join("Cargo.lock"))?;

        // Add source files
        for source_path in &unit.source_files {
            add_file(source_path)?;
        }

        // Add workspace files if they exist
        let workspace_root = root_dir.ancestors()
            .find(|dir| dir.join("Cargo.toml").exists())
            .unwrap_or(&root_dir);

        if workspace_root != &root_dir {
            add_file(&workspace_root.join("Cargo.toml"))?;
            add_file(&workspace_root.join("Cargo.lock"))?;
        }

        tar.finish()?;
        drop(tar.into_inner()?);
        Ok(tarball)
    }

    async fn write_artifact_safely(path: &Path, data: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let tmp_path = path.with_extension(format!("{}.tmp", std::process::id()));

        tokio::fs::write(&tmp_path, data).await?;

        #[cfg(windows)]
        {
            let old_path = path.with_extension(format!("{}.old", std::process::id()));
            if path.exists() {
                if let Err(e) = tokio::fs::rename(path, &old_path).await {
                    tokio::fs::remove_file(&tmp_path).await?;
                    return Err(anyhow::anyhow!(
                        "Could not replace existing file - it may be in use: {}",
                        e
                    ));
                }
            }

            if let Err(e) = tokio::fs::rename(&tmp_path, path).await {
                if old_path.exists() {
                    let _ = tokio::fs::rename(&old_path, path).await;
                }
                return Err(anyhow::anyhow!("Failed to move new file into place: {}", e));
            }

            if old_path.exists() {
                let _ = tokio::fs::remove_file(&old_path).await;
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
                        return Ok(());
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
                    for (path, data) in artifacts {
                        let target_path = if let Some(ref target) = self.target {
                            self.workspace_path
                                .join("target")
                                .join(target)
                                .join(if self.release { "release" } else { "debug" })
                                .join(path)
                        } else {
                            self.workspace_path
                                .join("target")
                                .join(if self.release { "release" } else { "debug" })
                                .join(path)
                        };

                        Self::write_artifact_safely(&target_path, &data).await?;
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

            let mut source_files = Vec::new();
            source_files.push(package.manifest_path.into());

            let workspace_manifest = Path::new(metadata.workspace_root.as_str()).join("Cargo.toml");
            if workspace_manifest.exists() {
                source_files.push(workspace_manifest);
            }

            for target in &package.targets {
                if target.kind.iter().any(|k| k == "lib" || k == "bin") {
                    let src_path = Path::new(&target.src_path);
                    let src_dir = src_path.parent().unwrap();

                    for entry in WalkDir::new(src_dir) {
                        if let Ok(entry) = entry {
                            if entry.path().extension().map_or(false, |ext| ext == "rs") {
                                source_files.push(entry.path().to_path_buf());
                            }
                        }
                    }
                }
            }

            let unit = BuildUnit {
                package_name: package.name,
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

        let tarball = Self::create_tarball(&unit)?;

        let request = BuildRequest::BuildUnit {
            unit: unit.clone(),
            release: self.release,
            target: self.target.clone(),
            tarball_data: tarball,
        };

        let data = bincode::serialize(&request)?;
        let len = (data.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&data).await?;

        self.handle_build_stream(stream, &unit).await
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
                return Err(e);
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    // Setup logging
    let log_level = if args.debug { Level::DEBUG } else { Level::INFO };
    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

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