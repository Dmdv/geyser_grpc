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
use tonic::{metadata::MetadataValue, Request, transport::{Channel, ClientTlsConfig}};
use futures::StreamExt;
use std::time::Duration;
use tokio::time::sleep;

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

async fn create_grpc_client(config: &Config) -> Result<GeyserClient<Channel>> {
    println!("Connecting to GRPC endpoint: {}", config.grpc.endpoint);
    let tls = ClientTlsConfig::new()
        .domain_name(&config.grpc.endpoint);
        
    let channel = Channel::from_shared(format!("https://{}", config.grpc.endpoint))
        .context("Invalid GRPC endpoint")?
        .tls_config(tls)?
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(30))
        .connect()
        .await
        .context("Failed to create GRPC channel")?;
    println!("Successfully created GRPC channel");

    let client = GeyserClient::new(channel);

    Ok(client)
}

async fn subscribe_to_blocks(config: &Config) -> Result<()> {
    let mut retry_count = 0;
    let max_retries = 3;
    let retry_delay = Duration::from_secs(5);

    // Initialize Solana client and keypair outside the loop
    let rpc_client = RpcClient::new("https://api.devnet.solana.com".to_string());
    let keypair = read_keypair_file(&config.solana.keypair_file)
        .map_err(|e| anyhow::anyhow!("Failed to read keypair file: {}", e))?;
    let destination_pubkey = Pubkey::from_str(&config.solana.destination_wallet)
        .context("Invalid destination wallet address")?;

    while retry_count < max_retries {
        let mut client = create_grpc_client(config).await?;

        let mut request = Request::new(SubscribeRequest {
            filters: vec![
                SubscribeRequestFilter {
                    filter: Some(geyser::subscribe_request_filter::Filter::Block(
                        BlockFilter {
                            filter_by_commitment: true,
                        },
                    )),
                },
            ],
            commitment: CommitmentLevel::Confirmed as i32,
        });

        let token = MetadataValue::from_str(&config.grpc.api_key)
            .map_err(|e| anyhow::anyhow!("Failed to create metadata value: {}", e))?;
        request.metadata_mut().insert("x-token", token);

        println!("Created subscription request: {:?}", request.get_ref());
        println!("Attempting to subscribe to GRPC stream...");

        match client.subscribe(request).await {
            Ok(response) => {
                println!("Successfully subscribed to GRPC stream");
                println!("Response metadata: {:?}", response.metadata());
                let mut stream = response.into_inner();

                while let Some(response) = stream.next().await {
                    match response {
                        Ok(msg) => {
                            println!("Received message: {:?}", msg);
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
                            } else {
                                println!("Received message but it's not a block update: {:?}", msg);
                            }
                        }
                        Err(e) => {
                            eprintln!("Error receiving message. Error details: {:?}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to subscribe to GRPC stream. Error details: {:?}", e);
            }
        }

        retry_count += 1;
        if retry_count < max_retries {
            println!("Retrying in {} seconds... (attempt {}/{})", retry_delay.as_secs(), retry_count + 1, max_retries);
            sleep(retry_delay).await;
        }
    }

    if retry_count >= max_retries {
        println!("Max retries reached. Exiting...");
    }

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

    println!("Starting Geyser GRPC subscription...");
    println!("Monitoring for new blocks...");

    subscribe_to_blocks(&config).await
} 