use anyhow::{Context, Result};
use clap::Parser;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer, read_keypair_file},
    system_instruction,
    transaction::Transaction,
    commitment_config::CommitmentLevel,
};
use solana_client::rpc_client::RpcClient;
use std::{str::FromStr, time::Duration};
use serde::Deserialize;
use std::fs;
use tonic::{metadata::MetadataValue, Request, transport::{Channel, ClientTlsConfig}};
use futures::StreamExt;
use tokio::time::sleep;
use tracing::{info, error, warn, Level};
use tracing_subscriber::FmtSubscriber;
use tracing_subscriber::prelude::*;
use thiserror::Error;
use tracing_appender;
use std::collections::HashMap;

mod geyser {
    tonic::include_proto!("geyser");
}

use geyser::{
    geyser_client::GeyserClient,
    CommitmentLevel as GeyserCommitmentLevel,
    SubscribeRequest,
    SubscribeRequestFilterBlocks,
    SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions,
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

impl Config {
    fn new() -> Self {
        Self {
            grpc_url: "https://api.rpcpool.com".to_string(),
            grpc_x_token: "your_token_here".to_string(),
            keypair_file: "keypair.json".to_string(),
            destination_wallet: "".to_string(),
            transfer_amount: 0,
        }
    }
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
    async fn connect(config: Config) -> Result<Self> {
        let channel = Channel::from_shared(config.grpc_url.clone())?
            .tls_config(ClientTlsConfig::new())?
            .connect()
            .await?;

        let client = GeyserClient::new(channel);

        Ok(Self { client, config })
    }

    async fn subscribe(&mut self) -> Result<tonic::Streaming<geyser::SubscribeUpdate>> {
        let mut blocks_map = HashMap::new();
        blocks_map.insert("blocks".to_string(), SubscribeRequestFilterBlocks {
            account_include: vec![],
            include_transactions: true,
            include_accounts: true,
            include_entries: true,
        });

        let mut slots_map = HashMap::new();
        slots_map.insert("slots".to_string(), SubscribeRequestFilterSlots {
            filter_by_commitment: true,
        });

        let mut transactions_map = HashMap::new();
        transactions_map.insert("transactions".to_string(), SubscribeRequestFilterTransactions {
            vote: false,
            failed: false,
            signature: "".into(),
            account_include: vec![],
            account_exclude: vec![],
            account_required: vec![],
        });

        let request = Request::new(SubscribeRequest {
            accounts: HashMap::new(),
            slots: slots_map,
            transactions: transactions_map,
            blocks: blocks_map,
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            commitment: GeyserCommitmentLevel::Confirmed as i32,
            accounts_data_slice: vec![],
            ping: false,
        });

        info!("Creating subscription request with filters: {:?}", request.get_ref());

        let token = MetadataValue::from_str(&self.config.grpc_x_token)
            .map_err(|e| anyhow::anyhow!("Failed to create metadata value: {}", e))?;
        
        let mut request = request;
        request.metadata_mut().insert("x-token", token);

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
                // Log raw message for debugging
                info!("Received raw message: {:?}", msg);
                
                match msg.update {
                    Some(geyser::subscribe_update::Update::Account(account)) => {
                        info!(
                            "Account update: pubkey={}, owner={}, lamports={}, slot={}\nRaw data: {:?}", 
                            account.pubkey, account.owner, account.lamports, account.slot,
                            account.data
                        );
                    }
                    Some(geyser::subscribe_update::Update::Slot(slot)) => {
                        info!(
                            "Slot update: slot={}, parent={}, status={}\nRaw data: {:?}", 
                            slot.slot, slot.parent, slot.status,
                            slot
                        );
                    }
                    Some(geyser::subscribe_update::Update::Transaction(transaction)) => {
                        info!(
                            "Transaction update: signature={}, is_vote={}\nTransaction info: {:?}", 
                            transaction.signature, transaction.is_vote,
                            transaction.transaction
                        );
                        
                        match solana_client.send_transaction().await {
                            Ok(_) => info!("Successfully sent transaction after receiving transaction {}", transaction.signature),
                            Err(e) => error!("Failed to send transaction after receiving transaction {}: {}", transaction.signature, e),
                        }
                    }
                    Some(geyser::subscribe_update::Update::Block(block)) => {
                        info!(
                            "Block update: slot={}, transactions_count={}, accounts_count={}\nRaw block data: {:?}", 
                            block.slot, 
                            block.transactions.len(),
                            block.accounts.len(),
                            block
                        );
                        
                        // Log individual transactions in the block
                        for (i, tx) in block.transactions.iter().enumerate() {
                            info!(
                                "Block transaction {}: slot={}, signature={}, is_vote={}", 
                                i, tx.slot, tx.signature, tx.is_vote
                            );
                        }
                    }
                    Some(geyser::subscribe_update::Update::BlockMeta(meta)) => {
                        info!(
                            "Block meta update: slot={}, blockhash={:?}\nRaw meta: {:?}", 
                            meta.slot, meta.blockhash,
                            meta
                        );
                    }
                    Some(geyser::subscribe_update::Update::Entry(entry)) => {
                        info!(
                            "Entry update: slot={}, index={}, transactions_count={}\nRaw entry: {:?}", 
                            entry.slot, entry.index, entry.transactions.len(),
                            entry
                        );
                    }
                    Some(geyser::subscribe_update::Update::Ping(_)) => {
                        info!("Received ping");
                    }
                    None => {
                        warn!("Received empty update");
                    }
                }
            }
            Err(e) => {
                error!("Error receiving message: {:?}\nError details: {:#?}", e, e);
                return Err(AppError::GrpcError(e));
            }
        }
    }
    
    info!("Stream ended normally");
    Ok(())
}

async fn subscribe_to_blocks(config: &Config) -> Result<(), AppError> {
    let solana_client = SolanaClient::new(config)?;
    let mut retry_count = 0;
    let max_retries = 3;
    let retry_delay = Duration::from_secs(5);

    while retry_count < max_retries {
        info!("Attempt {} of {}", retry_count + 1, max_retries);
        
        let mut client = GrpcClient::connect(config.clone()).await?;
        
        match client.subscribe().await {
            Ok(stream) => {
                info!("Starting stream processing...");
                match process_block_stream(stream, &solana_client).await {
                    Ok(_) => {
                        info!("Stream processing completed successfully");
                        return Ok(());
                    }
                    Err(e) => {
                        error!("Stream processing error: {:?}", e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to subscribe to GRPC stream: {:?}", e);
            }
        }

        retry_count += 1;
        if retry_count < max_retries {
            info!("Waiting {} seconds before retry...", retry_delay.as_secs());
            sleep(retry_delay).await;
        }
    }

    error!("Max retries reached. Exiting...");
    Ok(())
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

    // Run indefinitely with reconnection
    loop {
        match subscribe_to_blocks(&config).await {
            Ok(_) => {
                info!("Stream completed normally, reconnecting in 5 seconds...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => {
                error!("Error in stream processing: {:?}, reconnecting in 5 seconds...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
} 