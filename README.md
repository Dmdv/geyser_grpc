# Solana Geyser GRPC Client

Subscribe to Solana's Geyser GRPC service to monitor new blocks and automatically sends SOL transfers when new blocks are detected.

## Configuration

1. Copy the `config.yaml` file and update it with your settings:
   - Add your wallet's private key
   - Set the destination wallet address
   - Configure the transfer amount (in lamports)
   - Verify the GRPC endpoint and API key

## Building

```bash
cargo build --release
```

## Usage

```bash
./target/release/geyser_grpc --config config.yaml
```

## Features

- Monitors new blocks via Geyser GRPC
- Automatically sends SOL transfers on new block detection
- Configurable transfer amounts and destination
- Secure configuration via YAML file

## Disclaimer and security

The test-wallet is a dummy wallet and should not be used for real transactions.   
The private key is hardcoded for testing purposes only.  
For production use, ensure to implement secure key management practices.