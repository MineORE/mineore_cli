# MineORE Pool CLI

A command line interface for ORE cryptocurrency mining with MineORE pool.

## Install

To install the CLI, use [cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html):

```sh
cargo install mineore-cli
```

## Mining
To start mining, run the following commands
```sh
mineore --address <YOUR_SOLANA_BASE58_ADDRESS> mine --cores <NUMBER_OF_YOUR_CORES>
```

## Roadmap
- Mobile mining
- GPU Support
- Optimized CPU mining
- Improved rewards calculation
- More mining pools in other regions

### Dependencies
If you run into issues during installation, please install the following dependencies for your operating system and try again:

#### Linux
```
sudo apt-get install build-essential pkg-config
```

#### MacOS (using [Homebrew](https://brew.sh/))
```
brew install openssl pkg-config

# If you encounter issues with OpenSSL, you might need to set the following environment variables:
export PATH="/usr/local/opt/openssl/bin:$PATH"
export LDFLAGS="-L/usr/local/opt/openssl/lib"
export CPPFLAGS="-I/usr/local/opt/openssl/include"
```

#### Windows (using [Chocolatey](https://chocolatey.org/))
```
choco install openssl pkgconfiglite
```

## Build

To build the codebase from scratch, checkout the repo and use cargo to build:

```sh
cargo build --release
```

## Help

You can use the `-h` flag on any command to pull up a help menu with documentation:

```sh
mineore -h
```
