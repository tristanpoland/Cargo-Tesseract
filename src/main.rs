use clap::{Command, Arg};
use std::process::Command as CMD;
use std::error::Error;
use std::collections::{HashMap, HashSet};
use petgraph::Graph;
use petgraph::algo::toposort;

#[derive(Debug)]
struct BuildNode {
    host: String,
    cores: u32,
    current_jobs: u32,
}

#[derive(Debug)]
struct BuildJob {
    package: String,
    dependencies: HashSet<String>,
    node: Option<String>,
    status: JobStatus,
}

#[derive(Debug, PartialEq)]
enum JobStatus {
    Pending,
    Building,
    Complete,
    Failed,
}

struct BuildCoordinator {
    nodes: Vec<BuildNode>,
    jobs: HashMap<String, BuildJob>,
    dep_graph: Graph<String, ()>,
    shared_cache: String, // Path to shared artifact cache
}

impl BuildCoordinator {
    fn new(nodes: Vec<BuildNode>, shared_cache: String) -> Self {
        Self {
            nodes,
            jobs: HashMap::new(),
            dep_graph: Graph::new(),
            shared_cache,
        }
    }

    fn setup_shared_cache(&self) -> Result<(), Box<dyn Error>> {
        // Create shared artifact cache directory on all nodes
        for node in &self.nodes {
            CMD::new("ssh")
                .args(&[&node.host, &format!("mkdir -p {}", self.shared_cache)])
                .output()?;
        }
        Ok(())
    }

    fn analyze_dependencies(&mut self) -> Result<(), Box<dyn Error>> {
        let output = CMD::new("cargo")
            .args(&["metadata", "--format-version=1"])
            .output()?;

        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        
        // Build dependency graph
        if let Some(packages) = metadata["packages"].as_array() {
            for package in packages {
                let name = package["name"].as_str().unwrap().to_string();
                let node_idx = self.dep_graph.add_node(name.clone());
                
                self.jobs.insert(name.clone(), BuildJob {
                    package: name.clone(),
                    dependencies: HashSet::new(),
                    node: None,
                    status: JobStatus::Pending,
                });

                if let Some(deps) = package["dependencies"].as_array() {
                    for dep in deps {
                        let dep_name = dep["name"].as_str().unwrap().to_string();
                        let dep_idx = self.dep_graph.add_node(dep_name.clone());
                        self.dep_graph.add_edge(node_idx, dep_idx, ());
                        
                        if let Some(job) = self.jobs.get_mut(&name.clone()) {
                            job.dependencies.insert(dep_name);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn assign_jobs(&mut self) -> Vec<String> {
        // Get build order using topological sort
        let sorted = toposort(&self.dep_graph, None)
            .unwrap()
            .into_iter()
            .map(|idx| self.dep_graph[idx].clone())
            .collect::<Vec<_>>();

        // Assign jobs to least loaded nodes
        for package in &sorted {
            if let Some(job) = self.jobs.get_mut(package) {
                let best_node = self.nodes
                    .iter_mut()
                    .min_by_key(|node| node.current_jobs)
                    .unwrap();
                
                job.node = Some(best_node.host.clone());
                best_node.current_jobs += 1;
            }
        }

        sorted
    }

    fn build_package(&self, package: &str, release: bool) -> Result<(), Box<dyn Error>> {
        let job = self.jobs.get(package).unwrap();
        let node = job.node.as_ref().unwrap();

        // Sync dependencies from shared cache
        for dep in &job.dependencies {
            let dep_job = self.jobs.get(dep).unwrap();
            if dep_job.status == JobStatus::Complete {
                CMD::new("rsync")
                    .args(&[
                        "-az",
                        &format!("{}/{}/*", self.shared_cache, dep),
                        &format!("{}:{}/build/{}/", node, self.shared_cache, package),
                    ])
                    .output()?;
            }
        }

        // Build package
        let status = CMD::new("ssh")
            .args(&[
                node,
                &format!(
                    "cd {}/build/{} && CARGO_TARGET_DIR=target cargo build {}",
                    self.shared_cache,
                    package,
                    if release { "--release" } else { "" }
                ),
            ])
            .status()?;

        if !status.success() {
            return Err(format!("Build failed for package {}", package).into());
        }

        // Update shared cache with new artifacts
        CMD::new("rsync")
            .args(&[
                "-az",
                &format!("{}:{}/build/{}/target/", node, self.shared_cache, package),
                &format!("{}/{}/", self.shared_cache, package),
            ])
            .output()?;

        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let matches = Command::new("cargo-tess")
        .bin_name("cargo")
        .subcommand(
            Command::new("tess")
                .about("Distributed build on remote servers")
                .arg(
                    Arg::new("nodes")
                        .short('n')
                        .long("nodes")
                        .value_name("NODES")
                        .help("Comma-separated list of build nodes (host:cores)")
                        .default_value("localhost:8"),
                )
                .arg(
                    Arg::new("cache")
                        .short('c')
                        .long("cache")
                        .value_name("PATH")
                        .help("Path to shared artifact cache")
                        .default_value("/tmp/tess-cache"),
                )
                .arg(
                    Arg::new("release")
                        .long("release")
                        .help("Build with release profile"),
                )
        )
        .get_matches();

    if let Some(matches) = matches.subcommand_matches("tess") {
        let nodes: Vec<BuildNode> = matches.get_one::<String>("nodes").unwrap()
            .split(',')
            .filter_map(|node| {
                let parts: Vec<&str> = node.split(':').collect();
                if parts.len() == 2 {
                    Some(BuildNode {
                        host: parts[0].to_string(),
                        cores: parts[1].parse().unwrap_or(1),
                        current_jobs: 0,
                    })
                } else {
                    None
                }
            })
            .collect();

        let cache_path = matches.get_one::<String>("cache").unwrap().to_string();
        let mut coordinator = BuildCoordinator::new(nodes, cache_path);

        println!("📊 Analyzing dependencies...");
        coordinator.analyze_dependencies()?;
        
        println!("🔧 Setting up build environment...");
        coordinator.setup_shared_cache()?;

        println!("📋 Planning build distribution...");
        let build_order = coordinator.assign_jobs();

        println!("🚀 Starting distributed build...");
        let release = matches.contains_id("release");
        
        for package in build_order {
            println!("Building {}...", package);
            coordinator.build_package(&package, release)?;
        }

        println!("✨ Build complete!");
    }

    Ok(())
}