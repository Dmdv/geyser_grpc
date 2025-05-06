use anyhow::Result;
use clap::Parser;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer, read_keypair_file},
    system_instruction,
    transaction::Transaction,
};
use solana_client::rpc_client::RpcClient;
use std::{str::FromStr, time::Duration};
use serde::Deserialize;
use std::fs;
use tonic::transport::{Channel, ClientTlsConfig};
use futures::StreamExt;
use tokio::time::sleep;
use tracing::{info, error, warn};
use tracing_subscriber::prelude::*;
use thiserror::Error;
use tracing_appender;
use std::collections::HashMap;

mod geyser {
    tonic::include_proto!("geyser");
}

use geyser::{
    geyser_client::GeyserClient,
    SubscribeRequest,
    SubscribeRequestFilterBlocks,
    SubscribeRequestFilterSlots,
};

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("GRPC error: {0}")]
    GrpcError(#[from] tonic::Status),
    #[error("Transport error: {0}")]
    TransportError(#[from] tonic::transport::Error),
    #[error("Solana error: {0}")]
    SolanaError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Anyhow error: {0}")]
    AnyhowError(#[from] anyhow::Error),
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    grpc_url: String,
    grpc_x_token: String,
    keypair_file: String,
    destination_wallet: String,
    transfer_amount: u64,
}

struct SolanaClient {
    rpc_client: RpcClient,
    keypair: Keypair,
    destination_pubkey: Pubkey,
    transfer_amount: u64,
}

impl SolanaClient {
    fn new(config: &Config) -> Result<Self, AppError> {
        let rpc_client = RpcClient::new("https://api.devnet.solana.com".to_string());
        let keypair = read_keypair_file(&config.keypair_file)
            .map_err(|e| AppError::ConfigError(format!("Failed to read keypair file: {}", e)))?;
        let destination_pubkey = Pubkey::from_str(&config.destination_wallet)
            .map_err(|e| AppError::ConfigError(format!("Invalid destination wallet address: {}", e)))?;

        Ok(Self {
            rpc_client,
            keypair,
            destination_pubkey,
            transfer_amount: config.transfer_amount,
        })
    }

    async fn send_transaction(&self) -> Result<(), AppError> {
        let recent_blockhash = self.rpc_client
            .get_latest_blockhash()
            .map_err(|e| AppError::SolanaError(format!("Failed to get recent blockhash: {}", e)))?;

        let transaction = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &self.keypair.pubkey(),
                &self.destination_pubkey,
                self.transfer_amount,
            )],
            Some(&self.keypair.pubkey()),
            &[&self.keypair],
            recent_blockhash,
        );

        let signature = self.rpc_client
            .send_and_confirm_transaction(&transaction)
            .map_err(|e| AppError::SolanaError(format!("Failed to send transaction: {}", e)))?;

        info!("Transaction sent! Signature: {}", signature);
        Ok(())
    }
}

struct GrpcClient {
    client: GeyserClient<Channel>,
    config: Config,
}

impl GrpcClient {
    async fn connect(config: Config) -> Result<Self, AppError> {
        let channel = Channel::from_shared(config.grpc_url.clone())
            .map_err(|e| anyhow::anyhow!("Invalid URI: {}", e))?
            .tls_config(ClientTlsConfig::new())?
            .connect()
            .await?;

        Ok(Self {
            client: GeyserClient::new(channel),
            config,
        })
    }

    async fn subscribe(&mut self) -> Result<tonic::Streaming<geyser::SubscribeUpdate>, AppError> {
        let mut slots_map = HashMap::new();
        slots_map.insert(
            "slots".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: true,
            },
        );

        let mut blocks_map = HashMap::new();
        blocks_map.insert(
            "blocks".to_string(),
            SubscribeRequestFilterBlocks {
                account_include: vec![],
                include_transactions: true,
                include_accounts: true,
                include_entries: true,
            },
        );

        let request = SubscribeRequest {
            accounts: HashMap::new(),
            slots: slots_map,
            transactions: HashMap::new(),
            blocks: blocks_map,
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            commitment: 1,
            accounts_data_slice: vec![],
            ping: true,
        };

        info!("Creating subscription request with filters: {:?}", request);

        let mut request = tonic::Request::new(request);
        request.metadata_mut().insert(
            "x-token",
            self.config.grpc_x_token.parse().unwrap(),
        );

        info!("Created subscription request with metadata");

        let response = self.client.subscribe(request).await?;
        info!("Successfully subscribed to GRPC stream");

        Ok(response.into_inner())
    }
}

async fn process_block_stream(
    mut stream: tonic::Streaming<geyser::SubscribeUpdate>,
    solana_client: &SolanaClient,
) -> Result<(), AppError> {
    info!("Starting to process stream...");
    
    while let Some(response) = stream.next().await {
        match response {
            Ok(msg) => {
                let update_type = msg.update.as_ref().map_or("None".to_string(), |u| match u {
                    geyser::subscribe_update::Update::Account(_) => "Account".to_string(),
                    geyser::subscribe_update::Update::Slot(_) => "Slot".to_string(),
                    geyser::subscribe_update::Update::Block(_) => "Block".to_string(),
                    geyser::subscribe_update::Update::BlockMeta(_) => "BlockMeta".to_string(),
                    geyser::subscribe_update::Update::Entry(_) => "Entry".to_string(),
                    geyser::subscribe_update::Update::Ping(_) => "Ping".to_string(),
                    geyser::subscribe_update::Update::Transaction(_) => "Transaction".to_string(),
                });
                info!("Received message type: {:?}", update_type);
                
                match msg.update {
                    Some(geyser::subscribe_update::Update::Block(block)) => {
                        info!(
                            "Block update: slot={}, transactions_count={}, accounts_count={}", 
                            block.slot, 
                            block.transactions.len(),
                            block.accounts.len(),
                        );
                        
                        // Send transaction when new block is received
                        match solana_client.send_transaction().await {
                            Ok(_) => info!("Successfully sent transaction for block {}", block.slot),
                            Err(e) => error!("Failed to send transaction for block {}: {:?}", block.slot, e),
                        }
                    }
                    Some(geyser::subscribe_update::Update::Ping(_)) => {
                        info!("Received ping response");
                    }
                    _ => {
                        // Log other updates but don't process them
                        info!("Received update of type: {}", update_type);
                    }
                }
            }
            Err(e) => {
                error!("Error receiving message: {:?}", e);
                warn!("Continuing to listen for messages after error");
            }
        }
    }
    
    info!("Stream ended");
    Ok(())
}

async fn subscribe_to_blocks(config: &Config) -> Result<(), AppError> {
    let solana_client = SolanaClient::new(config)?;
    let retry_delay = Duration::from_secs(5);

    loop {
        info!("Attempting to connect to GRPC server at {}", config.grpc_url);
        
        match GrpcClient::connect(config.clone()).await {
            Ok(mut client) => {
                info!("Successfully connected to GRPC server. Creating subscription...");
                
                match client.subscribe().await {
                    Ok(stream) => {
                        info!("Successfully created subscription stream");
                        match process_block_stream(stream, &solana_client).await {
                            Ok(_) => {
                                info!("Stream processing completed normally");
                                info!("Waiting {} seconds before reconnecting...", retry_delay.as_secs());
                                sleep(retry_delay).await;
                            }
                            Err(e) => {
                                error!("Stream processing error: {:?}", e);
                                info!("Waiting {} seconds before reconnecting...", retry_delay.as_secs());
                                sleep(retry_delay).await;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to subscribe to GRPC stream: {:?}", e);
                        info!("Waiting {} seconds before reconnecting...", retry_delay.as_secs());
                        sleep(retry_delay).await;
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to GRPC server: {:?}", e);
                info!("Waiting {} seconds before reconnecting...", retry_delay.as_secs());
                sleep(retry_delay).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let args = Args::parse();
    
    let config_content = fs::read_to_string(&args.config)
        .map_err(|e| AppError::ConfigError(format!("Failed to read config file: {}", e)))?;
    
    let config: Config = serde_yaml::from_str(&config_content)
        .map_err(|e| AppError::ConfigError(format!("Failed to parse config file: {}", e)))?;

    // Initialize logging with both console and file output
    let file_appender = tracing_appender::rolling::RollingFileAppender::new(
        tracing_appender::rolling::Rotation::DAILY,
        "logs",
        "geyser_grpc.log",
    );

    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(true)
        .with_level(true)
        .with_thread_names(true)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339());

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_thread_ids(true)
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .with_level(true)
        .with_thread_names(true)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339());

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(file_layer)
        .with(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()))
        .init();

    info!("Starting geyser-grpc client...");
    info!("Using config file: {}", args.config);
    info!("Config: {:?}", config);

    subscribe_to_blocks(&config).await
} 