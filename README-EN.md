# dst-admin-rust
> dst-admin-rust management web

[English](README-EN.md)/[中文](README.md)

**Now supports both Windows and Linux platforms**

For Windows, download the `-windows.zip` release and follow the [Windows setup guide](docs/WINDOWS.md).

## About

DST Admin Rust is the Rust 2024 migration of the web-based management panel for "Don't Starve Together" dedicated servers. Target binary: `dst-admin-rust`. Key features include:

- 🚀 **Easy Deployment**: Single executable binary, no complex configuration required
- 💾 **Low Resource Usage**: Built with Rust, minimal memory footprint and high performance
- 🎨 **Modern UI**: Clean and intuitive web interface
- ⚙️ **Feature-Rich**:
  - Visual configuration for game rooms and world settings
  - Online mod management and configuration
  - Multi-cluster and multi-world support
  - Game save backup and snapshot restoration
  - Player management (whitelist, blacklist, administrators)
  - Real-time log viewing and game console access
  - Automatic game server update detection

## Preview

![首页效果](docs/image/dashboard.png)
![首页效果](docs/image/panel.png)
![首页效果](docs/image/toomanyitemplus.png)
![首页效果](docs/image/player.png)
![房间效果](docs/image/home.png)
![世界效果](docs/image/level.png)
![世界效果](docs/image/selectormod.png)
![模组效果](docs/image/mod1.png)
![模组效果](docs/image/mod3.png)
![模组效果](docs/image/mod2.png)
![日志效果](docs/image/playerlog.png)
![大厅效果](docs/image/lobby.png)



## Run

**Edit config.yml**
```yaml
# Bind address
bindAddress: ""
# Port
port: 8082
# Data directory prefix
dataDir: "./data"
# Windows helper CLI port
dstCliPort: 8102
# Database
database: dst-db
```

Run
```bash
cargo run --bin dst-admin-rust
```

## Build

### Build for Linux

```bash
./tools/release/build-linux.sh
# Output: dst-admin-rust (Linux amd64 binary)
```

When cross-compiling for Linux, install the target and provide a linker:

```bash
rustup target add x86_64-unknown-linux-gnu
LINUX_LINKER=x86_64-linux-gnu-gcc ./tools/release/build-linux.sh
```

### Build for Windows

```bash
./tools/release/build-windows.sh
# Output: dst-admin-rust.exe (Windows amd64 binary)
```

Windows GNU builds require the Rust target and MinGW linker:

```bash
rustup target add x86_64-pc-windows-gnu
x86_64-w64-mingw32-gcc --version
./tools/release/build-windows.sh
```

### Build for the current platform

```bash
cargo build --release --bin dst-admin-rust
```

## QQ Group
![QQ 群](docs/image/饥荒开服面板交流issue群聊二维码.png)
