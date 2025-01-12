use std::{collections::VecDeque, fs, str::FromStr};

use chrono::Utc;
use dotenvy::dotenv;
use ethers::{signers::LocalWallet, types::H160};
use hyperliquid_rust_sdk::{
    BaseUrl, CandleData, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient, Message, Subscription, UserData,
};
use serde::Deserialize;
use tokio::{sync::mpsc::unbounded_channel, time::interval};
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;

use dual_channel_bot::{
    caching::store_candle_to_cache,
    get_price, store_tick_to_cache,
    utils::{print_statistics, Direction, Trade},
};

const MAKER_FEE: f64 = 0.0001; // 0.01%
const TAKER_FEE: f64 = 0.00034; // 0.034%
const STATS_INTERVAL_SECS: u64 = 60; // Print stats every minute

#[derive(Debug, Deserialize)]
struct Config {
    bot: BotConfig,
    risk: RiskConfig,
    vwap: VwapConfig,
}

#[derive(Debug, Deserialize)]
struct BotConfig {
    asset: String,
    capital: f64,
    risk_per_trade: f64,
    leverage: f64,
    decimals: u32,
    test_mode: bool,
}

#[derive(Debug, Deserialize)]
struct RiskConfig {
    stop_loss: f64,
    take_profit: f64,
}

#[derive(Debug, Deserialize)]
struct VwapConfig {
    hourly_periods: usize,
    five_min_periods: usize,
}

#[derive(Debug)]
/// Simple struct to hold order book updates: bids and asks
struct OrderBookUpdate {
    bids: Vec<(f64, f64)>, // (price, size)
    asks: Vec<(f64, f64)>, // (price, size)
}

#[derive(Debug)]
/// A basic VWAP struct that aggregates sum of price*volume over total volume
struct VWAP {
    sum_pv: f64,
    sum_v: f64,
}

impl VWAP {
    fn new() -> Self {
        VWAP { sum_pv: 0.0, sum_v: 0.0 }
    }

    fn update(&mut self, price: f64, volume: f64) {
        self.sum_pv += price * volume;
        self.sum_v += volume;
    }

    fn value(&self) -> f64 {
        if self.sum_v > 0.0 {
            self.sum_pv / self.sum_v
        } else {
            0.0
        }
    }
}

/// Main bot struct
pub struct OrderFlowTradingBot {
    config: Config,
    capital: f64,         // Current capital after PnL
    initial_capital: f64, // Starting capital

    current_position: f64,
    latest_mid_price: f64,
    current_trade: Option<Trade>,
    closed_trades: Vec<Trade>,

    // Candle buffers
    hourly_candles: VecDeque<CandleData>,
    five_min_candles: VecDeque<CandleData>,
    last_hourly_candle_ts: i64, // Timestamp of last processed hourly candle
    last_five_min_candle_ts: i64, // Timestamp of last processed 5min candle

    // VWAP trackers
    hourly_vwap: VWAP,
    five_min_vwap: VWAP,

    // Clients & user address
    info_client: InfoClient,
    exchange_client: Option<ExchangeClient>,
    user_address: Option<H160>,
}

impl OrderFlowTradingBot {
    /// Constructor
    pub async fn new(
        config_path: &str,
        wallet: Option<LocalWallet>,
        user_address: Option<H160>,
    ) -> eyre::Result<Self> {
        debug!("Creating new OrderFlowTradingBot instance");

        // Load and parse config
        let config_str = fs::read_to_string(config_path)?;
        let config: Config = toml::from_str(&config_str)?;

        // Build InfoClient and ExchangeClient
        let info_client = InfoClient::new(None, Some(BaseUrl::Mainnet))
            .await
            .map_err(|e| eyre::eyre!("Failed to create InfoClient: {}", e))?;

        let exchange_client = if !config.bot.test_mode {
            if let (Some(wallet), Some(_addr)) = (wallet, user_address) {
                Some(
                    ExchangeClient::new(None, wallet, Some(BaseUrl::Mainnet), None, None)
                        .await
                        .map_err(|e| eyre::eyre!("Failed to create ExchangeClient: {}", e))?,
                )
            } else {
                return Err(eyre::eyre!("Wallet and user address required for live trading"));
            }
        } else {
            None
        };

        Ok(Self {
            initial_capital: config.bot.capital,
            capital: config.bot.capital,
            config,

            current_position: 0.0,
            latest_mid_price: -1.0,
            current_trade: None,
            closed_trades: Vec::new(),

            hourly_candles: VecDeque::with_capacity(24),
            five_min_candles: VecDeque::with_capacity(12),
            last_hourly_candle_ts: 0,
            last_five_min_candle_ts: 0,

            hourly_vwap: VWAP::new(),
            five_min_vwap: VWAP::new(),

            info_client,
            exchange_client,
            user_address,
        })
    }

    /// Calculate position size based on risk and capital
    fn calculate_position_size(&self, entry_price: f64) -> f64 {
        let risk_amount = self.capital * self.config.bot.risk_per_trade;
        let position_value = risk_amount * self.config.bot.leverage;
        position_value / entry_price
    }

    /// Place an order - either real or simulated
    async fn place_order(&self, size: f64, price: f64) -> eyre::Result<()> {
        let is_buy = size > 0.0;
        let fees = if is_buy { TAKER_FEE } else { MAKER_FEE };
        let fee_amount = price * size.abs() * fees;

        if self.config.bot.test_mode {
            info!(
                "[TEST] {} order: size={}, price={}, fees={}",
                if is_buy { "Buy" } else { "Sell" },
                size.abs(),
                price,
                fee_amount
            );
            Ok(())
        } else if let Some(exchange_client) = &self.exchange_client {
            let order_request = ClientOrderRequest {
                asset: self.config.bot.asset.clone(),
                is_buy,
                reduce_only: false,
                limit_px: price,
                sz: size.abs(),
                cloid: None,
                order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".to_string() }),
            };

            match exchange_client.order(order_request, None).await {
                Ok(ExchangeResponseStatus::Ok(order_response)) => {
                    if let Some(order_data) = order_response.data {
                        match &order_data.statuses[0] {
                            ExchangeDataStatus::Filled(filled_order) => {
                                info!(
                                    "Order filled: {} {} {} at {}",
                                    if is_buy { "Bought" } else { "Sold" },
                                    size.abs(),
                                    self.config.bot.asset,
                                    price
                                );
                            }
                            ExchangeDataStatus::Resting(_) => {
                                info!(
                                    "Order resting: {} {} {} at {}",
                                    if is_buy { "Buy" } else { "Sell" },
                                    size.abs(),
                                    self.config.bot.asset,
                                    price
                                );
                            }
                            ExchangeDataStatus::Error(e) => {
                                error!("Error placing order: {}", e);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(ExchangeResponseStatus::Err(e)) => {
                    error!("Error placing order: {}", e);
                }
                Err(e) => {
                    error!("Error placing order: {}", e);
                }
            }
            Ok(())
        } else {
            Err(eyre::eyre!("Exchange client not initialized"))
        }
    }

    /// Enter a trade if risk/reward looks decent
    async fn enter_trade(&mut self, signal: f64) {
        debug!("Entering trade with signal: {}", signal);
        if self.latest_mid_price <= 0.0 {
            warn!("No valid mid price available to enter trade.");
            return;
        }

        let entry_price = self.latest_mid_price;
        let position_size = self.calculate_position_size(entry_price) * signal.signum();
        let direction = if signal > 0.0 { Direction::Long } else { Direction::Short };

        // Calculate stop loss and take profit based on risk config
        let stop_loss = if signal > 0.0 {
            entry_price * (1.0 - self.config.risk.stop_loss)
        } else {
            entry_price * (1.0 + self.config.risk.stop_loss)
        };

        let take_profit = if signal > 0.0 {
            entry_price * (1.0 + self.config.risk.take_profit)
        } else {
            entry_price * (1.0 - self.config.risk.take_profit)
        };

        // Format logs with decimals
        let formatted_entry_price =
            format!("{:.*}", self.config.bot.decimals as usize, entry_price);
        let formatted_position_size =
            format!("{:.*}", self.config.bot.decimals as usize, position_size.abs());

        info!(
            "Entering trade: {} at price: {}, position size: {}",
            if signal > 0.0 { "Long" } else { "Short" },
            formatted_entry_price,
            formatted_position_size
        );

        // Place the order
        if let Err(e) = self.place_order(position_size, entry_price).await {
            error!("Failed to place opening order: {}", e);
            return;
        }

        // Store the trade
        self.current_trade = Some(Trade {
            direction,
            entry_price,
            entry_time: Utc::now().timestamp(),
            size: position_size,
            tp_price: take_profit,
            sl_price: stop_loss,
            close_price: None,
        });

        info!(
            "Trade placed: {} at price: {}, position size: {}, SL: {}, TP: {}",
            if signal > 0.0 { "Long" } else { "Short" },
            formatted_entry_price,
            formatted_position_size,
            stop_loss,
            take_profit
        );
    }

    /// Exit trade and update PnL
    async fn exit_trade(&mut self, exit_price: f64) {
        debug!("Exiting trade at price: {}", exit_price);
        if let Some(mut trade) = self.current_trade.take() {
            let offset_size = -trade.size;

            // Calculate PnL
            let price_diff = exit_price - trade.entry_price;
            let pnl = if trade.direction == Direction::Long {
                price_diff * trade.size
            } else {
                -price_diff * trade.size
            };

            // Calculate fees
            let entry_fees = trade.entry_price * trade.size.abs() * TAKER_FEE;
            let exit_fees = exit_price * trade.size.abs() * MAKER_FEE;
            let total_fees = entry_fees + exit_fees;

            // Update capital
            self.capital += pnl - total_fees;

            // Update trade with close price
            trade.close_price = Some(exit_price);

            // Place exit order
            if let Err(e) = self.place_order(offset_size, exit_price).await {
                error!("Failed to place exit order: {}", e);
                return;
            }

            self.closed_trades.push(trade);

            info!(
                "Trade closed - PnL: ${:.2}, Fees: ${:.2}, Current Capital: ${:.2}",
                pnl, total_fees, self.capital
            );
        }
    }

    /// Start the bot: subscribe to channels and process messages in a loop
    pub async fn start(&mut self) -> eyre::Result<()> {
        debug!("Starting OrderFlowTradingBot");
        // Create the channel for receiving subscription messages
        let (sender, mut receiver) = unbounded_channel();

        // Setup subscriptions
        self.subscribe_all(sender.clone()).await?;

        let mut stats_interval = interval(std::time::Duration::from_secs(STATS_INTERVAL_SECS));
        let (tick_cache_path, candle_cache_path) = self.get_cache_paths();

        // Create cache directory if it doesn't exist
        if let Some(cache_dir) = std::path::Path::new(&tick_cache_path).parent() {
            fs::create_dir_all(cache_dir)?;
        }

        // Main message loop
        loop {
            tokio::select! {
                Some(message) = receiver.recv() => {
                    match message {
                        Message::AllMids(all_mids) => {
                            if let Some(mid) = all_mids.data.mids.get(&self.config.bot.asset) {
                                match mid.parse::<f64>() {
                                    Ok(px) => {
                                        self.latest_mid_price = get_price(px, 0.1);
                                        self.check_stop_loss_take_profit().await;

                                        if let Err(e) = store_tick_to_cache(&tick_cache_path, px) {
                                            error!("Failed to store tick: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to parse mid price: {}", e);
                                    }
                                }
                            }
                        }
                        Message::User(user_events) => {
                            self.handle_user_event(user_events.data).await;
                        }
                        Message::L2Book(order_book) => {
                            let update = OrderBookUpdate {
                                bids: order_book.data.levels[0]
                                    .iter()
                                    .filter_map(|level| {
                                        let px = level.px.parse().ok()?;
                                        let sz = level.sz.parse().ok()?;
                                        Some((px, sz))
                                    })
                                    .collect(),
                                asks: order_book.data.levels[1]
                                    .iter()
                                    .filter_map(|level| {
                                        let px = level.px.parse().ok()?;
                                        let sz = level.sz.parse().ok()?;
                                        Some((px, sz))
                                    })
                                    .collect(),
                            };
                            self.process_order_book_update(update).await;
                        }
                        Message::Candle(candle) => {
                            let candle_data = candle.data.clone();

                            // Only process if it's a new candle
                            let candle_ts = candle_data.time_close as i64;
                            let should_process = match candle_data.interval.as_str() {
                                "1h" => candle_ts > self.last_hourly_candle_ts,
                                "5m" => candle_ts > self.last_five_min_candle_ts,
                                _ => false,
                            };

                            if should_process {
                                self.process_candle(candle_data.clone()).await;
                                if let Err(e) = store_candle_to_cache(&candle_cache_path, &candle_data) {
                                    error!("Failed to store candle: {}", e);
                                }
                            }
                        }
                        _ => {
                            // Unhandled message type - you might want to log or ignore
                        }
                    }
                }
                _ = stats_interval.tick() => {
                    print_statistics(&self.closed_trades);
                    info!(
                        "Capital: ${:.2}, Current Position: {:?}",
                        self.capital,
                        self.current_trade.unwrap_or(Trade::default())
                    );
                }
            }
        }
    }

    /// Subscribe to all needed streams.
    async fn subscribe_all(
        &mut self,
        sender: tokio::sync::mpsc::UnboundedSender<Message>,
    ) -> eyre::Result<()> {
        debug!("Subscribing to all streams");

        if self.user_address.is_some() {
            // 1) Fills / user events
            self.info_client
                .subscribe(
                    Subscription::UserEvents { user: self.user_address.unwrap() },
                    sender.clone(),
                )
                .await
                .map_err(|e| eyre::eyre!("Failed to subscribe to user events: {}", e))?;
        }

        // 2) OrderBook (L2)
        self.info_client
            .subscribe(Subscription::L2Book { coin: self.config.bot.asset.clone() }, sender.clone())
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to L2Book: {}", e))?;

        // 3) Mids
        self.info_client
            .subscribe(Subscription::AllMids, sender.clone())
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to AllMids: {}", e))?;

        // 4) 1H candles
        self.info_client
            .subscribe(
                Subscription::Candle {
                    coin: self.config.bot.asset.clone(),
                    interval: "1h".to_string(),
                },
                sender.clone(),
            )
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to 1h candles: {}", e))?;

        // 5) 5m candles
        self.info_client
            .subscribe(
                Subscription::Candle {
                    coin: self.config.bot.asset.clone(),
                    interval: "5m".to_string(),
                },
                sender,
            )
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to 5m candles: {}", e))?;

        Ok(())
    }

    /// Handle user fill events
    async fn handle_user_event(&mut self, event_data: UserData) {
        debug!("Handling user event: {:?}", event_data);
        if let UserData::Fills(fills) = event_data {
            for fill in fills {
                // Attempt to parse the fill size
                let amount: f64 = match fill.sz.parse() {
                    Ok(a) => a,
                    Err(e) => {
                        error!("Failed to parse fill size: {}", e);
                        continue;
                    }
                };

                // Adjust current_position
                if fill.side.eq("B") {
                    self.current_position += amount;
                    info!("Fill: bought {} {}", amount, self.config.bot.asset);
                } else {
                    self.current_position -= amount;
                    info!("Fill: sold {} {}", amount, self.config.bot.asset);
                }
            }
        }
    }

    /// Process an L2 order book update
    async fn process_order_book_update(&mut self, update: OrderBookUpdate) {
        // debug!("Processing order book update: {:?}", update);
        // For demonstration, we compute an order flow imbalance
        let imbalance = self.calculate_order_flow_imbalance(&update);
        trace!("Order book imbalance calculated: {}", imbalance);

        // You could incorporate imbalance logic in `generate_trading_signal` or separate
        // step to refine your strategy.
    }

    /// Calculate a simple order-flow imbalance metric
    fn calculate_order_flow_imbalance(&self, update: &OrderBookUpdate) -> f64 {
        let bid_volume: f64 = update.bids.iter().map(|(_, size)| size).sum();
        let ask_volume: f64 = update.asks.iter().map(|(_, size)| size).sum();
        if (bid_volume + ask_volume).abs() < f64::EPSILON {
            return 0.0;
        }
        (bid_volume - ask_volume) / (bid_volume + ask_volume)
    }

    /// Process a candle message
    async fn process_candle(&mut self, candle: CandleData) {
        let candle_ts = candle.time_close as i64;

        debug!("Processing new candle data: {:?}", candle);
        let price: f64 = match candle.close.parse() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to parse candle close price: {}", e);
                return;
            }
        };

        let volume: f64 = candle.volume.parse().unwrap();

        // Update appropriate candle buffer and VWAP
        if candle.interval == "1h" {
            self.hourly_candles.push_back(candle);
            if self.hourly_candles.len() > self.config.vwap.hourly_periods * 2 {
                self.hourly_candles.pop_front();
            }
            self.hourly_vwap.update(price, volume);
            self.last_hourly_candle_ts = candle_ts;
        } else if candle.interval == "5m" {
            self.five_min_candles.push_back(candle);
            if self.five_min_candles.len() > self.config.vwap.five_min_periods * 2 {
                self.five_min_candles.pop_front();
            }
            self.five_min_vwap.update(price, volume);
            self.last_five_min_candle_ts = candle_ts;
        }

        self.generate_trading_signal().await;
    }

    /// Generate trading signals based on VWAPs and current position
    async fn generate_trading_signal(&mut self) {
        // Wait until we have enough data
        if self.hourly_candles.len() < self.config.vwap.hourly_periods ||
            self.five_min_candles.len() < self.config.vwap.five_min_periods
        {
            debug!(
                "Insufficient data for trading signal generation 24h: {}, 5m: {}",
                self.hourly_candles.len(),
                self.five_min_candles.len()
            );
            return;
        }

        let hourly_vwap = self.hourly_vwap.value();
        let five_min_vwap = self.five_min_vwap.value();

        // Basic cross-over style: short-term VWAP vs. long-term VWAP
        let signal = if five_min_vwap > hourly_vwap {
            1.0 // Bullish signal
        } else if five_min_vwap < hourly_vwap {
            -1.0 // Bearish signal
        } else {
            0.0 // Neutral
        };

        debug!(
            "Trading signal generated: {} Hourly VWAP: {}, 5m VWAP: {}",
            signal, hourly_vwap, five_min_vwap
        );

        // If a signal appears and there's no open trade, attempt to open one
        if signal != 0.0 && self.current_trade.is_none() {
            self.enter_trade(signal).await;
        }
    }

    /// Check if SL/TP conditions are met
    async fn check_stop_loss_take_profit(&mut self) {
        if let Some(trade) = &self.current_trade {
            // Stop Loss
            if (trade.direction == Direction::Long && self.latest_mid_price <= trade.sl_price) ||
                (trade.direction == Direction::Short && self.latest_mid_price >= trade.sl_price)
            {
                debug!("Stop Loss hit at price: {}", self.latest_mid_price);
                self.exit_trade(self.latest_mid_price).await;
            }
            // Take Profit
            else if (trade.direction == Direction::Long &&
                self.latest_mid_price >= trade.tp_price) ||
                (trade.direction == Direction::Short && self.latest_mid_price <= trade.tp_price)
            {
                debug!("Take Profit hit at price: {}", self.latest_mid_price);
                self.exit_trade(self.latest_mid_price).await;
            }
        }
    }

    fn get_cache_paths(&self) -> (String, String) {
        let tick_cache_path = format!(".cache/{}", self.config.bot.asset);
        let candle_cache_path = format!(".cache/{}_candles", self.config.bot.asset);
        (tick_cache_path, candle_cache_path)
    }
}

/// Simple entry point that creates and runs the bot
#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    // Load environment variables if .env exists
    let _ = dotenv(); // Ignore error if .env not found

    info!("Starting main function");

    // Load wallet and address only if not in test mode
    let config_str = fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&config_str)?;

    let (wallet, user_address) = if !config.bot.test_mode {
        let private_key = std::env::var("PRIVATE_KEY_LONG")
            .map_err(|_| eyre::eyre!("Missing PRIVATE_KEY in .env"))?;
        let wallet = LocalWallet::from_str(&private_key)
            .map_err(|e| eyre::eyre!("Invalid PRIVATE_KEY format: {}", e))?;
        let user_address = std::env::var("USER_ADDRESS_LONG")
            .map_err(|_| eyre::eyre!("Missing USER_ADDRESS_LONG in .env"))?
            .parse()
            .map_err(|e| eyre::eyre!("Invalid USER_ADDRESS format: {}", e))?;
        (Some(wallet), Some(user_address))
    } else {
        (None, None)
    };

    // Create and start the bot
    let mut bot = OrderFlowTradingBot::new("config.toml", wallet, user_address).await?;
    bot.start().await?;

    Ok(())
}
