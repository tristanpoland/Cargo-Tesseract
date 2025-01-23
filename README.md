# 🚀 cargo-tess

Remote Rust builds on powerful cloud hardware. Accelerate your development by offloading compilation to high-performance build servers.

[![Crates.io](https://img.shields.io/crates/v/cargo-tess.svg)](https://crates.io/crates/cargo-tess)
[![Documentation](https://docs.rs/cargo-tess/badge.svg)](https://docs.rs/cargo-tess)
[![Build Status](https://github.com/username/cargo-tess/workflows/CI/badge.svg)](https://github.com/username/cargo-tess/actions)

## ✨ Features

- 🖥️ Build on powerful remote hardware instead of your local machine
- 🔄 Seamless integration with Cargo workflow
- 📦 Smart dependency caching
- 🔒 Secure artifact transfer
- 📊 Real-time build progress visualization
- 🚀 Support for release and debug builds
- 🔜 Distributed builds across multiple nodes (coming soon!)

## 🚀 Quick Start

Install cargo-tess:

```bash
cargo install cargo-tess
```

Run a remote build:

```bash
cargo tess --node build.example.com:9876
```

For release builds:

```bash
cargo tess --node build.example.com:9876 --release
```

## 🔒 Security

- Secure TCP connections
- Sanitized path handling
- Size-limited artifact transfers
- Heartbeat monitoring

## 🤝 Contributing

1. Fork the repository
2. Create your feature branch
3. Submit a Pull Request

## 📝 License

This project is licensed under [MIT License](LICENSE)

---
Made with ❤️ for the Rust community