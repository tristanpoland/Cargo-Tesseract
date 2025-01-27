use anyhow::{Context, Result};
use cargo_metadata::{MetadataCommand, Package};
use clap::{Parser, Subcommand};
use colored::*;
use flate2::{write::GzEncoder, Compression};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tar::Builder;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
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

        #[arg(short, long)]
        package: Option<String>,
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

struct ProjectFiles {
    root: PathBuf,
    workspace_root: PathBuf,
    files: Vec<PathBuf>,
    ignore_patterns: Vec<String>,
    workspace_packages: HashMap<String, PathBuf>,
}

impl ProjectFiles {
    fn new(start_dir: PathBuf, package_name: Option<&str>) -> Result<Self> {
        let metadata = MetadataCommand::new()
            .no_deps()
            .exec()
            .context("Failed to read Cargo metadata")?;

        let workspace_root = PathBuf::from(&metadata.workspace_root);
        
        // Create a map of all workspace packages
        let workspace_packages: HashMap<String, PathBuf> = metadata
            .packages
            .iter()
            .map(|p| (p.name.clone(), PathBuf::from(p.manifest_path.parent().unwrap())))
            .collect();

        // Handle package selection
        let package = if metadata.workspace_members.len() > 1 {
            match package_name {
                Some(name) => metadata.packages
                    .iter()
                    .find(|p| p.name == name)
                    .context(format!("Package '{}' not found in workspace. Available packages: {}", 
                        name,
                        workspace_packages.keys().cloned().collect::<Vec<_>>().join(", ")))?,
                None => {
                    let packages: Vec<_> = metadata.packages
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect();
                    return Err(anyhow::anyhow!(
                        "Multiple packages found in workspace. Please specify one with --package. Available packages: {}",
                        packages.join(", ")
                    ));
                }
            }
        } else {
            metadata.root_package()
                .context("No package found in workspace")?
        };

        let package_root = package.manifest_path.parent()
            .context("Invalid manifest path")?
            .to_path_buf();

        Ok(Self {
            root: package_root.into(),
            workspace_root: workspace_root.clone(),
            files: Vec::new(),
            ignore_patterns: Self::get_gitignore_patterns(&workspace_root),
            workspace_packages,
        })
    }

    fn get_gitignore_patterns(root: &Path) -> Vec<String> {
        let mut patterns = vec![
            ".git".to_string(),
            "target".to_string(),
            "node_modules".to_string(),
        ];

        // Read workspace root .gitignore
        if let Ok(content) = fs::read_to_string(root.join(".gitignore")) {
            patterns.extend(
                content
                    .lines()
                    .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                    .map(|line| line.trim().to_string())
            );
        }

        patterns
    }

    fn collect_files(&mut self, package: &Package, verbose: bool) -> Result<()> {
        let mut all_files = HashSet::new();
        let mut packages_to_include = HashSet::new();
        
        // Add the main package
        packages_to_include.insert(package.name.clone());
        
        // Add all workspace dependencies recursively
        let mut stack = package.dependencies.iter()
            .filter(|dep| self.workspace_packages.contains_key(&dep.name))
            .map(|dep| dep.name.clone())
            .collect::<Vec<_>>();

        while let Some(dep_name) = stack.pop() {
            if packages_to_include.insert(dep_name.clone()) {
                // Find the package in workspace
                if let Some(dep_pkg) = self.workspace_packages.get(&dep_name) {
                    // Add its workspace dependencies to the stack
                    let metadata = MetadataCommand::new()
                        .manifest_path(dep_pkg.join("Cargo.toml"))
                        .no_deps()
                        .exec()?;
                    if let Some(pkg) = metadata.packages.iter().find(|p| p.name == dep_name) {
                        stack.extend(pkg.dependencies.iter()
                            .filter(|d| self.workspace_packages.contains_key(&d.name))
                            .map(|d| d.name.clone()));
                    }
                }
            }
        }

        if verbose {
            println!("Including packages: {:?}", packages_to_include);
        }

        // Collect files for each package
        for package_name in &packages_to_include {
            let package_path = self.workspace_packages.get(package_name)
                .context(format!("Package path not found for {}", package_name))?;

            for entry in WalkDir::new(package_path)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|entry| entry.path().is_file() && !self.is_ignored(entry.path()))
            {
                all_files.insert(entry.path().to_owned());
            }
        }

        // Include workspace root files
        let workspace_files = vec!["Cargo.toml", "Cargo.lock"];
        for file in workspace_files {
            let file_path = self.workspace_root.join(file);
            if file_path.exists() {
                all_files.insert(file_path);
            }
        }

        self.files = all_files.into_iter().collect();

        if self.files.is_empty() {
            return Err(anyhow::anyhow!("No source files found in package"));
        }

        if verbose {
            println!("Collected {} files", self.files.len());
            for file in &self.files {
                println!("  {}", file.display());
            }
        }

        Ok(())
    }

    fn is_ignored(&self, path: &Path) -> bool {
        let relative_to_workspace = path
            .strip_prefix(&self.workspace_root)
            .map(|p| p.to_string_lossy())
            .unwrap_or_else(|_| path.to_string_lossy());

        self.ignore_patterns.iter().any(|pattern| {
            let pattern = pattern.trim_matches('/');
            
            if pattern.ends_with('/') && path.parent().map_or(false, |p| p.is_dir()) {
                let dir_pattern = pattern.trim_end_matches('/');
                return Self::matches_glob(dir_pattern, &relative_to_workspace);
            }

            Self::matches_glob(pattern, &relative_to_workspace)
        })
    }

    fn matches_glob(pattern: &str, path: &str) -> bool {
        let regex_pattern = pattern
            .replace(".", "\\.")
            .replace("**", ".*")
            .replace("*", "[^/]*")
            .replace("?", ".")
            .replace("[!", "[^")
            .replace("[]", "\\[\\]");

        Regex::new(&format!("(?:^|/){}(?:/|$)", regex_pattern))
            .map(|re| re.is_match(path))
            .unwrap_or(false)
    }

    fn create_tarball(&self, verbose: bool) -> Result<Vec<u8>> {
        let mut tarball = Vec::new();
        let encoder = GzEncoder::new(&mut tarball, Compression::default());
        let mut tar = Builder::new(encoder);
        let mut added_paths = HashSet::new();

        for path in &self.files {
            if !path.exists() {
                eprintln!("Warning: Source file not found: {}", path.display());
                continue;
            }

            let relative_path = path
                .strip_prefix(&self.workspace_root)
                .with_context(|| format!("Failed to get relative path for: {}", path.display()))?;

            if !added_paths.insert(relative_path.to_path_buf()) {
                continue;
            }

            if verbose {
                println!("Adding to tarball: {}", relative_path.display());
            }

            tar.append_path_with_name(path, relative_path)
                .with_context(|| format!("Failed to add to tarball: {}", relative_path.display()))?;
        }

        let encoder = tar.into_inner().context("Failed to finalize tar archive")?;
        encoder.finish().context("Failed to finish compression")?;
        
        Ok(tarball)
    }
}

async fn send_request(
    stream: &mut TcpStream,
    request: &BuildRequest,
    timeout_duration: Duration,
) -> Result<BuildResponse> {
    time::timeout(timeout_duration, async {
        let data = bincode::serialize(request).context("Failed to serialize request")?;
        let len = (data.len() as u32).to_be_bytes();

        stream.write_all(&len).await?;
        stream.write_all(&data).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf);

        let mut buf = vec![0; len as usize];
        stream.read_exact(&mut buf).await?;

        bincode::deserialize(&buf).context("Failed to deserialize response")
    })
    .await
    .context("Request timed out")?
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    match args.command {
        DistributedBuildCommand::Build {
            node,
            release,
            verbose,
            timeout,
            package,
        } => {
            let current_dir = std::env::current_dir()
                .context("Failed to get current directory")?;

            // Initialize project files with optional package name
            let mut project_files = ProjectFiles::new(current_dir, package.as_deref())?;
            
            // Get project metadata
            let metadata = MetadataCommand::new()
                .manifest_path(project_files.root.join("Cargo.toml"))
                .exec()
                .context("Failed to read Cargo.toml")?;

            let package = metadata
                .packages
                .iter()
                .find(|p| p.manifest_path.parent().map_or(false, |path| path == project_files.root))
                .context("Failed to find package in metadata")?;

            // Collect all files including workspace dependencies
            project_files.collect_files(package, verbose)?;

            let multi_progress = MultiProgress::new();
            let overall_pb = multi_progress.add(ProgressBar::new(100));
            overall_pb.set_style(
                ProgressStyle::default_bar()
                    .template("{msg} {spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {percent}%")?
                    .progress_chars("#>-"),
            );

            println!("{}", "🔌 Connecting to build cluster...".green());
            let mut stream = TcpStream::connect(&node)
                .await
                .context("Failed to connect to build node")?;
            stream.set_nodelay(true)?;

            // Initial heartbeat check
            let heartbeat_response = send_request(
                &mut stream,
                &BuildRequest::Heartbeat,
                Duration::from_secs(10),
            )
            .await?;

            if !matches!(heartbeat_response, BuildResponse::HeartbeatAck) {
                return Err(anyhow::anyhow!("Invalid response from build node"));
            }

            // Create build unit
            let build_unit = BuildUnit {
                package_name: package.name.clone(),
                dependencies: package
                    .dependencies
                    .iter()
                    .map(|d| d.name.clone())
                    .collect(),
                source_files: project_files.files.clone(),
                artifacts: package
                    .targets
                    .iter()
                    .filter(|t| t.kind.iter().any(|k| k == "bin" || k == "lib"))
                    .map(|t| PathBuf::from(&t.name))
                    .collect(),
            };

            if verbose {
                println!("Build unit: {:#?}", build_unit);
            }

            // Create and send tarball
            let tarball_data = project_files.create_tarball(verbose)?;

            println!("{}", "🚀 Starting distributed build...".green());
            let build_request = BuildRequest::BuildUnit {
                unit: build_unit,
                release,
                tarball_data,
            };

            let response = time::timeout(
                Duration::from_secs(timeout),
                send_request(&mut stream, &build_request, Duration::from_secs(timeout)),
            )
            .await??;

            match response {
                BuildResponse::BuildComplete { unit_name, artifacts } => {
                    println!("{}", "✅ Build successful".green());
                    println!("{}", "📥 Downloading artifacts...".yellow());

                    let target_base = project_files.root.join("target");
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
                    eprintln!(
                        "{}: {} - {}",
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