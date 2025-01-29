use std::{
    collections::HashMap,
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use eyre::{eyre, Result};
use hyperliquid_rust_sdk::{BaseUrl, CandleData, InfoClient, Message, Subscription};
use thiserror::Error;
use tokio::{
    select,
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
    time::sleep,
};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::{self, EnvFilter};

use dual_channel_bot::caching::{store_asset_to_cache, store_candle_to_cache, store_tick_to_cache};

#[derive(Error, Debug)]
pub enum MarketDataError {
    #[error("Subscription error: {0}")]
    SubscriptionError(String),
    #[error("Cache storage error: {0}")]
    CacheError(String),
    #[error("Connection error: {0}")]
    ConnectionError(String),
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Comma-separated list of assets to subscribe to (e.g., "ETH,BTC,SOL")
    #[arg(short, long)]
    assets: String,

    /// Maximum messages before reconnection (default: 100,000)
    #[arg(short, long, default_value = "100000")]
    max_messages: usize,

    /// Cache directory for storing market data
    #[arg(short, long, default_value = ".cache")]
    cache_dir: String,
}

struct SubscriptionManager {
    info_client: InfoClient,
    subscription_ids: Vec<u32>,
    sender: UnboundedSender<Message>,
    assets: Vec<String>,
}

impl SubscriptionManager {
    #[instrument(skip(self))]
    async fn subscribe_all(&mut self) -> Result<(), MarketDataError> {
        info!("Subscribing to all assets");
        self.subscription_ids.clear();

        for asset in &self.assets {
            for interval in ["1h", "4h"].iter() {
                let sub_id = self
                    .info_client
                    .subscribe(
                        Subscription::Candle {
                            coin: asset.clone(),
                            interval: interval.to_string(),
                        },
                        self.sender.clone(),
                    )
                    .await
                    .map_err(|e| MarketDataError::SubscriptionError(e.to_string()))?;

                self.subscription_ids.push(sub_id);
                debug!("Subscribed to {asset} {interval} candles with ID: {sub_id}");
            }
            let sub = self
                .info_client
                .subscribe(
                    Subscription::ActiveAssetCtx { coin: asset.clone() },
                    self.sender.clone(),
                )
                .await
                .map_err(|e| MarketDataError::SubscriptionError(e.to_string()))?;
            self.subscription_ids.push(sub);
            debug!("Subscribed to {asset} active asset context with ID: {sub}");
        }

        let all_mids_id = self
            .info_client
            .subscribe(Subscription::AllMids, self.sender.clone())
            .await
            .map_err(|e| MarketDataError::SubscriptionError(e.to_string()))?;

        self.subscription_ids.push(all_mids_id);
        debug!("Subscribed to all mids with ID: {all_mids_id}");

        Ok(())
    }

    #[instrument(skip(self))]
    async fn unsubscribe_all(&mut self) -> Result<(), MarketDataError> {
        info!("Unsubscribing from all subscriptions");

        for id in &self.subscription_ids {
            if let Err(e) = self.info_client.unsubscribe(*id).await {
                warn!("Failed to unsubscribe from {id}: {e}");
            }
        }

        self.subscription_ids.clear();
        Ok(())
    }
}

struct MessageHandler {
    assets: Vec<String>,
    cache_dir: String,
    message_count: usize,
    max_messages: usize,
    last_reconnect: Instant,
    // Batching buffers
    tick_buffer: HashMap<String, Vec<f64>>,
    candle_buffer: HashMap<String, Vec<CandleData>>,
    last_flush: Instant,
    batch_size: usize,
    flush_interval: Duration,
}

impl MessageHandler {
    fn new(assets: Vec<String>, cache_dir: String, max_messages: usize) -> Self {
        Self {
            assets,
            cache_dir,
            message_count: 0,
            max_messages,
            last_reconnect: Instant::now(),
            tick_buffer: HashMap::new(),
            candle_buffer: HashMap::new(),
            last_flush: Instant::now(),
            batch_size: 100,                        // Adjust based on your needs
            flush_interval: Duration::from_secs(5), // Flush every 5 seconds
        }
    }

    async fn flush_buffers(&mut self) -> Result<(), MarketDataError> {
        // Flush tick buffers
        for (asset, ticks) in self.tick_buffer.drain() {
            let tick_cache_path = format!("{}/{}_ticks", self.cache_dir, asset);
            for tick in ticks {
                store_tick_to_cache(&tick_cache_path, tick)
                    .map_err(|e| MarketDataError::CacheError(e.to_string()))?;
            }
        }

        // Flush candle buffers
        for (key, candles) in self.candle_buffer.drain() {
            let candle_cache_path = format!("{}/{}", self.cache_dir, key);
            for candle in candles {
                store_candle_to_cache(&candle_cache_path, &candle)
                    .map_err(|e| MarketDataError::CacheError(e.to_string()))?;
            }
        }

        info!("Flushed buffers");
        self.last_flush = Instant::now();
        Ok(())
    }

    fn handle_message(&mut self, message: Message) -> Result<bool, MarketDataError> {
        self.message_count += 1;
        let should_reconnect = self.message_count >= self.max_messages;

        match message {
            Message::Candle(candle) => {
                let candle_data = candle.data;
                debug!("Candle: {:?}", candle_data);

                let asset = candle_data.coin.clone();
                let timeframe = candle_data.interval.clone();
                let key = format!("{}_{}_candles", asset, timeframe);

                self.candle_buffer.entry(key).or_insert_with(Vec::new).push(candle_data);
            }
            Message::AllMids(all_mids) => {
                for asset in &self.assets {
                    if let Some(mid) = all_mids.data.mids.get(asset) {
                        if let Ok(px) = mid.parse::<f64>() {
                            debug!("Mid {} : {:?}", asset, px);
                            self.tick_buffer.entry(asset.clone()).or_insert_with(Vec::new).push(px);
                        }
                    }
                }
            }
            Message::ActiveAssetCtx(active_asset_ctx) => {
                debug!("Active asset context: {:?}", active_asset_ctx);
                let asset = active_asset_ctx.data.coin.clone();
                let asset_cache_path = format!("{}/{}_asset", self.cache_dir, asset);
                let asset_ctx = active_asset_ctx.data.ctx;
                // Asset context updates are important, write immediately
                store_asset_to_cache(&asset_cache_path, asset_ctx)
                    .map_err(|e| MarketDataError::CacheError(e.to_string()))?;
            }
            _ => {}
        }

        // Check if we need to flush based on buffer size or time
        let should_flush = self.tick_buffer.values().any(|v| v.len() >= self.batch_size) ||
            self.candle_buffer.values().any(|v| v.len() >= self.batch_size) ||
            self.last_flush.elapsed() >= self.flush_interval;

        if should_flush {
            // Take ownership of the buffers and replace them with empty ones
            let tick_buffer = std::mem::take(&mut self.tick_buffer);
            let candle_buffer = std::mem::take(&mut self.candle_buffer);
            let cache_dir = self.cache_dir.clone();

            tokio::spawn(async move {
                // Create a temporary handler just for flushing
                let mut temp_handler = MessageHandler::new(vec![], cache_dir, 0);
                temp_handler.tick_buffer = tick_buffer;
                temp_handler.candle_buffer = candle_buffer;

                if let Err(e) = temp_handler.flush_buffers().await {
                    error!("Error flushing buffers: {}", e);
                }
            });

            self.last_flush = Instant::now();
        }

        if should_reconnect {
            info!(
                "Message limit reached ({}/{}), initiating reconnection",
                self.message_count, self.max_messages
            );
            self.message_count = 0;
            self.last_reconnect = Instant::now();
        }

        Ok(should_reconnect)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_ids(true)
        .try_init();

    let args = Args::parse();

    // Create cache directory if it doesn't exist
    fs::create_dir_all(&args.cache_dir)
        .map_err(|e| eyre!("Failed to create cache directory: {}", e))?;

    let assets: Vec<String> = args.assets.split(',').map(|s| s.trim().to_string()).collect();

    info!("Starting market data service for assets: {:?}", assets);

    let (sender, receiver) = mpsc::unbounded_channel();
    let network = BaseUrl::Mainnet;
    let info_client = InfoClient::new(None, Some(network.clone())).await?;

    let mut sub_manager = SubscriptionManager {
        info_client,
        subscription_ids: Vec::new(),
        sender: sender.clone(),
        assets: assets.clone(),
    };

    let message_handler =
        Arc::new(Mutex::new(MessageHandler::new(assets, args.cache_dir, args.max_messages)));

    // Set up graceful shutdown
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    // let shutdown_tx = Arc::new(Mutex::new(shutdown_tx));

    // Handle Ctrl+C
    // let shutdown_tx_clone = Arc::clone(&shutdown_tx);
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Failed to listen for Ctrl+C: {}", e);
            return;
        }
        info!("Received Ctrl+C, initiating graceful shutdown");
        let _ = shutdown_tx.send(()).await;
    });

    sub_manager.subscribe_all().await?;

    process_messages(receiver, message_handler, &mut sub_manager, &mut shutdown_rx).await?;

    Ok(())
}

async fn process_messages(
    mut receiver: UnboundedReceiver<Message>,
    message_handler: Arc<Mutex<MessageHandler>>,
    sub_manager: &mut SubscriptionManager,
    shutdown_rx: &mut mpsc::Receiver<()>,
) -> Result<()> {
    loop {
        select! {
            // Handle shutdown signal
            _ = shutdown_rx.recv() => {
                info!("Shutdown signal received, cleaning up...");
                sub_manager.unsubscribe_all().await?;
                break Ok(());
            }

            // Handle incoming messages
            Some(message) = receiver.recv() => {
                let mut handler = message_handler.lock().await;
                match handler.handle_message(message) {
                    Ok(should_reconnect) => {
                        if should_reconnect {
                            sub_manager.unsubscribe_all().await?;
                            sleep(Duration::from_secs(3)).await;
                            sub_manager.subscribe_all().await?;
                        }
                    }
                    Err(e) => {
                        error!("Error handling message: {}", e);
                        // Depending on the error type, we might want to reconnect or exit
                        match e {
                            MarketDataError::ConnectionError(_) => {
                                warn!("Connection error, attempting to reconnect...");
                                sub_manager.unsubscribe_all().await?;
                                sleep(Duration::from_secs(5)).await;
                                sub_manager.subscribe_all().await?;
                            }
                            _ => {
                                error!("Fatal error, shutting down: {}", e);
                                break Err(eyre!("Fatal error: {}", e));
                            }
                        }
                    }
                }
            }
        }
    }
}
