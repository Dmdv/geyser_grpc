use anyhow::{Context, Result, anyhow};
use clap::Parser;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Keypair,
    system_instruction,
    transaction::Transaction,
    signature::Signer,
    signature::read_keypair_file,
};
use solana_client::rpc_client::RpcClient;
use std::str::FromStr;
use serde::Deserialize;
use std::fs;
use tonic::{metadata::MetadataValue, Request, transport::Channel};
use futures::StreamExt;

mod geyser {
    tonic::include_proto!("geyser");
}

use geyser::{
    geyser_client::GeyserClient,
    SubscribeRequest,
    SubscribeRequestFilter,
    BlockFilter,
    CommitmentLevel,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    grpc: GrpcConfig,
    solana: SolanaConfig,
}

#[derive(Debug, Deserialize)]
struct GrpcConfig {
    endpoint: String,
    api_key: String,
}

#[derive(Debug, Deserialize)]
struct SolanaConfig {
    keypair_file: String,
    destination_wallet: String,
    transfer_amount: u64,
}

async fn send_sol_transaction(
    rpc_client: &RpcClient,
    keypair: &Keypair,
    destination: &Pubkey,
    amount: u64,
) -> Result<()> {
    let recent_blockhash = rpc_client
        .get_latest_blockhash()
        .context("Failed to get recent blockhash")?;

    let transaction = Transaction::new_signed_with_payer(
        &[system_instruction::transfer(
            &keypair.pubkey(),
            destination,
            amount,
        )],
        Some(&keypair.pubkey()),
        &[keypair],
        recent_blockhash,
    );

    let signature = rpc_client
        .send_and_confirm_transaction(&transaction)
        .context("Failed to send transaction")?;

    println!("Transaction sent! Signature: {}", signature);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    // Load configuration
    let config_content = fs::read_to_string(&args.config)
        .context("Failed to read config file")?;
    let config: Config = serde_yaml::from_str(&config_content)
        .context("Failed to parse config file")?;

    // Initialize Solana client
    let rpc_client = RpcClient::new(
        "https://api.devnet.solana.com".to_string(),
    );

    // Create keypair from file
    let keypair = read_keypair_file(&config.solana.keypair_file)
        .map_err(|e| anyhow!("Failed to read keypair file: {}", e))?;
    
    // Parse destination wallet
    let destination_pubkey = Pubkey::from_str(&config.solana.destination_wallet)
        .context("Invalid destination wallet address")?;

    println!("Starting Geyser GRPC subscription...");
    println!("Monitoring for new blocks...");

    // Create GRPC client with API key in headers
    let mut client = {
        let channel = Channel::from_shared(config.grpc.endpoint)
            .context("Invalid GRPC endpoint")?
            .connect()
            .await
            .context("Failed to create GRPC channel")?;

        GeyserClient::with_interceptor(channel, move |mut req: Request<()>| {
            let token = MetadataValue::from_str(&config.grpc.api_key)
                .map_err(|e| tonic::Status::invalid_argument(e.to_string()))?;
            req.metadata_mut().insert("x-token", token);
            Ok(req)
        })
    };

    // Create subscription request
    let subscribe_request = SubscribeRequest {
        filters: vec![SubscribeRequestFilter {
            filter: Some(geyser::subscribe_request_filter::Filter::Block(
                BlockFilter {
                    filter_by_commitment: true,
                },
            )),
        }],
        commitment: CommitmentLevel::Confirmed as i32,
    };

    // Start subscription
    let mut stream = client
        .subscribe(subscribe_request)
        .await
        .context("Failed to subscribe")?
        .into_inner();

    // Process incoming messages
    while let Some(response) = stream.next().await {
        match response {
            Ok(msg) => {
                if let Some(geyser::subscribe_response::Response::Block(block)) = msg.response {
                    println!("New block detected! Slot: {}", block.slot);
                    
                    // Send SOL transfer
                    if let Err(e) = send_sol_transaction(
                        &rpc_client,
                        &keypair,
                        &destination_pubkey,
                        config.solana.transfer_amount,
                    ).await {
                        eprintln!("Failed to send transaction: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("Error receiving message: {}", e);
                break;
            }
        }
    }

    Ok(())
} 