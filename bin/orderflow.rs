use std::{collections::VecDeque, str::FromStr};

use dotenvy::dotenv;
use ethers::{
    signers::{LocalWallet, Signer},
    types::H160,
};
use tokio::{sync::mpsc::unbounded_channel, time::interval};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{field::debug, EnvFilter};
use hyperliquid_rust_sdk::{
    BaseUrl, CandleData, ClientLimit, ClientOrder,
    ClientOrderRequest, ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
    Message, Subscription, UserData,
};

const MAKER_FEE: f64 = 0.0001; // 0.01%
const TAKER_FEE: f64 = 0.00035; // 0.035%
/// Print stats every 5 minutes
const STATS_INTERVAL_SECS: u64 = 60;

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

#[derive(Debug)]
/// A struct to keep track of a current open trade
struct Trade {
    entry_price: f64,
    position_size: f64,
    stop_loss: f64,
    take_profit: f64,
}

/// Main bot struct
pub struct OrderFlowTradingBot {
    asset: String,
    max_position_size: f64,
    risk_factor: f64,
    order_size: f64,
    decimals: u32,

    current_position: f64,
    latest_mid_price: f64,
    current_trade: Option<Trade>,
    closed_trades: Vec<Trade>,

    // Candle buffers
    hourly_candles: VecDeque<CandleData>,
    five_min_candles: VecDeque<CandleData>,

    // VWAP trackers
    hourly_vwap: VWAP,
    five_min_vwap: VWAP,

    // Clients & user address
    info_client: InfoClient,
    exchange_client: ExchangeClient,
    user_address: H160,
}

impl OrderFlowTradingBot {
    /// Constructor
    pub async fn new(
        asset: String,
        max_position_size: f64,
        risk_factor: f64,
        order_size: f64,
        decimals: u32,
        wallet: LocalWallet,
    ) -> eyre::Result<Self> {
        debug!("Creating new OrderFlowTradingBot instance");
        let user_address = wallet.address();

        // Build InfoClient and ExchangeClient
        let info_client = InfoClient::new(None, Some(BaseUrl::Mainnet))
            .await
            .map_err(|e| eyre::eyre!("Failed to create InfoClient: {}", e))?;

        let exchange_client = ExchangeClient::new(None, wallet, Some(BaseUrl::Testnet), None, None)
            .await
            .map_err(|e| eyre::eyre!("Failed to create ExchangeClient: {}", e))?;

        Ok(Self {
            asset,
            max_position_size,
            risk_factor,
            order_size,
            decimals,

            current_position: 0.0,
            latest_mid_price: -1.0,
            current_trade: None,
            closed_trades: Vec::new(),

            hourly_candles: VecDeque::new(),
            five_min_candles: VecDeque::new(),
            hourly_vwap: VWAP::new(),
            five_min_vwap: VWAP::new(),

            info_client,
            exchange_client,
            user_address,
        })
    }

    /// Start the bot: subscribe to channels and process messages in a loop
    pub async fn start(&mut self) -> eyre::Result<()> {
        debug!("Starting OrderFlowTradingBot");
        // Create the channel for receiving subscription messages
        let (sender, mut receiver) = unbounded_channel();

        // Setup subscriptions
        self.subscribe_all(sender.clone()).await?;

        // let mut stats_interval = interval(std::time::Duration::from_secs(STATS_INTERVAL_SECS));
        // let current_position = self.current_position;
        // let closed_trades_len = self.closed_trades.len();
        // tokio::spawn(async move {
        //     loop {
        //         stats_interval.tick().await;
        //         info!("Current position: {}, closed trades: {}", current_position, closed_trades_len);
        //     }
        // });

        // Main message loop
        while let Some(message) = receiver.recv().await {
            match message {
                Message::AllMids(all_mids) => {
                    if let Some(mid) = all_mids.data.mids.get(&self.asset) {
                        match mid.parse::<f64>() {
                            Ok(px) => {
                                self.latest_mid_price = px;
                                self.check_stop_loss_take_profit().await;
                                debug!("Received mid price: {}", px);
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
                    self.process_candle(candle.data).await;
                }
                _ => {
                    // Unhandled message type - you might want to log or ignore
                }
            }
        }
        Ok(())
    }

    /// Subscribe to all needed streams.
    async fn subscribe_all(
        &mut self,
        sender: tokio::sync::mpsc::UnboundedSender<Message>,
    ) -> eyre::Result<()> {
        debug!("Subscribing to all streams");
        // 1) Fills / user events
        self.info_client
            .subscribe(Subscription::UserEvents { user: self.user_address }, sender.clone())
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to user events: {}", e))?;

        // 2) OrderBook (L2)
        self.info_client
            .subscribe(Subscription::L2Book { coin: self.asset.clone() }, sender.clone())
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
                Subscription::Candle { coin: self.asset.clone(), interval: "1h".to_string() },
                sender.clone(),
            )
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to 1h candles: {}", e))?;

        // 5) 5m candles
        self.info_client
            .subscribe(
                Subscription::Candle { coin: self.asset.clone(), interval: "5m".to_string() },
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
                    info!("Fill: bought {} {}", amount, self.asset);
                } else {
                    self.current_position -= amount;
                    info!("Fill: sold {} {}", amount, self.asset);
                }
            }
        }
    }

    /// Process an L2 order book update
    async fn process_order_book_update(&mut self, update: OrderBookUpdate) {
        debug!("Processing order book update: {:?}", update);
        // For demonstration, we compute an order flow imbalance
        let imbalance = self.calculate_order_flow_imbalance(&update);
        info!("Order book imbalance calculated: {}", imbalance);

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
        debug!("Processing candle data: {:?}", candle);
        let price: f64 = match candle.close.parse() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to parse candle close price: {}", e);
                return;
            }
        };

        let volume: f64 = match candle.volume.parse() {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to parse candle volume: {}", e);
                return;
            }
        };

        // Update appropriate candle buffer and VWAP
        if candle.interval == "1h" {
            self.hourly_candles.push_back(candle);
            if self.hourly_candles.len() > 24 {
                self.hourly_candles.pop_front();
            }
            self.hourly_vwap.update(price, volume);
        } else if candle.interval == "5m" {
            self.five_min_candles.push_back(candle);
            if self.five_min_candles.len() > 12 {
                self.five_min_candles.pop_front();
            }
            self.five_min_vwap.update(price, volume);
        }

        self.generate_trading_signal().await;
    }

    /// Generate trading signals based on VWAPs and current position
    async fn generate_trading_signal(&mut self) {
        debug!("Generating trading signal");
        // Wait until we have enough data
        if self.hourly_candles.len() < 24 || self.five_min_candles.len() < 12 {
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

        // If a signal appears and there's no open trade, attempt to open one
        if signal != 0.0 && self.current_trade.is_none() {
            self.enter_trade(signal).await;
        }
    }

    /// Enter a trade if risk/reward looks decent
    async fn enter_trade(&mut self, signal: f64) {
        debug!("Entering trade with signal: {}", signal);
        // Ensure we have a valid mid price
        if self.latest_mid_price <= 0.0 {
            warn!("No valid mid price available to enter trade.");
            return;
        }

        let entry_price = self.latest_mid_price;

        // Position size: min of (requested size) and (max * risk_factor)
        let position_size =
            (self.order_size * signal).min(self.max_position_size * self.risk_factor);

        // Format logs with decimals
        let formatted_entry_price = format!("{:.*}", self.decimals as usize, entry_price);
        let formatted_position_size = format!("{:.*}", self.decimals as usize, position_size.abs());

        // Calculate a naive SL/TP
        let stop_loss = if signal > 0.0 {
            entry_price * (1.0 - self.risk_factor)
        } else {
            entry_price * (1.0 + self.risk_factor)
        };
        let take_profit = if signal > 0.0 {
            entry_price * (1.0 + self.risk_factor * 2.0)
        } else {
            entry_price * (1.0 - self.risk_factor * 2.0)
        };

        info!(
            "Entering trade: {} at price: {}, position size: {}",
            if signal > 0.0 { "Long" } else { "Short" },
            formatted_entry_price,
            formatted_position_size
        );

        // Check basic R:R with fees
        let risk = (entry_price - stop_loss).abs() + entry_price * TAKER_FEE;
        let reward = (take_profit - entry_price).abs() - take_profit * MAKER_FEE;

        if reward > risk {
            // Place the opening order
            if let Err(e) = self.place_order(position_size, entry_price).await {
                error!("Failed to place opening order: {}", e);
                return;
            }

            // Store the trade
            self.current_trade = Some(Trade { entry_price, position_size, stop_loss, take_profit });

            info!("Trade placed with stop loss: {:.4}, take profit: {:.4}", stop_loss, take_profit);
        } else {
            info!("Trade not placed due to unfavorable risk-reward ratio.");
        }
    }

    /// Check if SL/TP conditions are met
    async fn check_stop_loss_take_profit(&mut self) {
        debug!("Checking stop loss and take profit conditions");
        if let Some(trade) = &self.current_trade {
            // Stop Loss
            if (trade.position_size > 0.0 && self.latest_mid_price <= trade.stop_loss) ||
                (trade.position_size < 0.0 && self.latest_mid_price >= trade.stop_loss)
            {
                self.exit_trade(self.latest_mid_price).await;
            }
            // Take Profit
            else if (trade.position_size > 0.0 && self.latest_mid_price >= trade.take_profit) ||
                (trade.position_size < 0.0 && self.latest_mid_price <= trade.take_profit)
            {
                self.exit_trade(self.latest_mid_price).await;
            }
        }
    }

    /// Exit trade by placing an offsetting order
    async fn exit_trade(&mut self, exit_price: f64) {
        debug!("Exiting trade at price: {}", exit_price);
        if let Some(trade) = &self.current_trade {
            let offset_size = -trade.position_size;
            if let Err(e) = self.place_order(offset_size, exit_price).await {
                error!("Failed to place exit order: {}", e);
                return;
            }
        }

        self.closed_trades.push(self.current_trade.take().unwrap());
        self.current_trade = None;
    }

    /// Helper: place an order using the ExchangeClient
    async fn place_order(&self, size: f64, price: f64) -> eyre::Result<()> {
        debug!("Placing order: size = {}, price = {}", size, price);
        let is_buy = size > 0.0;
        let order_request = ClientOrderRequest {
            asset: self.asset.clone(),
            is_buy,
            reduce_only: false,
            limit_px: price,
            sz: size.abs(),
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".to_string() }),
        };

        debug!("Placing order: {:?}", order_request);

        match self.exchange_client.order(order_request, None).await {
            Ok(ExchangeResponseStatus::Ok(order_response)) => {
                if let Some(order_data) = order_response.data {
                    if !order_data.statuses.is_empty() {
                        match &order_data.statuses[0] {
                            ExchangeDataStatus::Filled(filled_order) => {
                                info!(
                                    "Order filled: {} {} {} at {}",
                                    if is_buy { "Bought" } else { "Sold" },
                                    size.abs(),
                                    self.asset,
                                    price
                                );
                            }
                            ExchangeDataStatus::Resting(_) => {
                                info!(
                                    "Order resting: {} {} {} at {}",
                                    if is_buy { "Buy" } else { "Sell" },
                                    size.abs(),
                                    self.asset,
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
            }
            Ok(ExchangeResponseStatus::Err(e)) => {
                error!("Error placing order: {}", e);
            }
            Err(e) => {
                error!("Error placing order: {}", e);
            }
        }

        Ok(())
    }
}

/// Simple entry point that creates and runs the bot
#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    // Load environment variables
    dotenv()?;

    debug!("Starting main function");

    let private_key =
        std::env::var("PRIVATE_KEY").map_err(|_| eyre::eyre!("Missing PRIVATE_KEY in .env"))?;
    let wallet = LocalWallet::from_str(&private_key)
        .map_err(|e| eyre::eyre!("Invalid PRIVATE_KEY format: {}", e))?;

    // Build and start the bot
    // Create a new instance of the OrderFlowTradingBot
    let mut bot = OrderFlowTradingBot::new(
        "ETH".to_string(), // asset
        1.0,               // max_position_size
        0.01,              // risk_factor
        0.1,               // order_size
        2,                 // decimals
        wallet             // wallet
    ).await?;
    
    bot.start().await?;

    Ok(())
}
