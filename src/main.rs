use std::str::FromStr;

use chrono::Utc;
use clap::Parser;
use ethers::{signers::LocalWallet, types::H160};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, ExchangeDataStatus,
    ExchangeResponseStatus, InfoClient, Message, Subscription, UserData,
};
use tokio::sync::mpsc::unbounded_channel;
use tracing::{debug, error, info, trace, warn};

const LEVERAGE: f64 = 3.0;
const TP_PERCENTAGE: f64 = 0.02; // 2%
const SL_PERCENTAGE: f64 = 0.04; // 4%
const MAX_TRADE_DURATION: i64 = 3600; // 1 hour in seconds
const MID_CHECK_DURATION: i64 = 1800; // 30 minutes in seconds

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Use mainnet instead of testnet
    #[arg(long, default_value_t = false)]
    mainnet: bool,

    /// Initial position size per channel
    #[arg(long, default_value_t = 15.0)]
    size: f64,

    /// Trading pair symbol
    #[arg(long, default_value = "HYPE")]
    symbol: String,
}

#[derive(Debug)]
struct Trade {
    entry_price: f64,
    position_size: f64,
    stop_loss: f64,
    take_profit: f64,
    entry_time: i64,
    is_long: bool,
}

#[derive(Debug)]
/// A trading bot that maintains two simultaneous trading channels (long and short)
pub struct DualChannelTradingBot {
    /// The trading asset/coin symbol (e.g. "HYPE")
    asset: String,
    /// The position size to use for each channel (long/short)
    channel_size: f64,
    /// Number of decimal places for price/size rounding
    decimals: u32,
    /// Current total position size across both channels
    current_position: f64,
    /// Latest mid price from the orderbook
    latest_mid_price: f64,
    /// Client for market data and subscriptions
    info_client: InfoClient,
    /// Client for executing trades
    exchange_client: ExchangeClient,
    /// Ethereum address of the trading wallet
    user_address: H160,
    /// Details of the current long trade if one exists
    long_trade: Option<Trade>,
    /// Details of the current short trade if one exists
    short_trade: Option<Trade>,
}

impl DualChannelTradingBot {
    pub async fn new(
        asset: String,
        channel_size: f64,
        decimals: u32,
        wallet: LocalWallet,
        user_address: String,
        network: BaseUrl,
    ) -> DualChannelTradingBot {
        debug!(
            "Initializing bot with: asset={}, size={}, network={:?}",
            asset,
            channel_size,
            match network {
                BaseUrl::Mainnet => "mainnet",
                BaseUrl::Testnet => "testnet",
                BaseUrl::Localhost => "localhost",
            },
        );

        let info_client = InfoClient::new(None, Some(network.clone())).await.unwrap();
        let exchange_client =
            ExchangeClient::new(None, wallet, Some(network.clone()), None, None).await.unwrap();

        DualChannelTradingBot {
            asset,
            channel_size,
            decimals,
            current_position: 0.0,
            latest_mid_price: -1.0,
            info_client,
            exchange_client,
            user_address: H160::from_str(&user_address).unwrap(),
            long_trade: None,
            short_trade: None,
        }
    }

    pub async fn start(&mut self) {
        info!("Starting dual channel bot for {}", self.asset);
        debug!("Initial channel size: {}", self.channel_size);

        let (sender, mut receiver) = unbounded_channel();

        // Subscribe to necessary feeds
        debug!("Subscribing to user events for address: {}", self.user_address);
        self.info_client
            .subscribe(Subscription::UserEvents { user: self.user_address }, sender.clone())
            .await
            .unwrap();

        debug!("Subscribing to market data");
        self.info_client.subscribe(Subscription::AllMids, sender.clone()).await.unwrap();

        // Initial trades
        debug!("Opening initial positions");
        self.open_long_trade().await;
        self.open_short_trade().await;

        info!("Bot running - monitoring trades");
        loop {
            let message = receiver.recv().await.unwrap();
            match message {
                Message::AllMids(all_mids) => {
                    let all_mids = all_mids.data.mids;
                    if let Some(mid) = all_mids.get(&self.asset) {
                        let new_price: f64 = mid.parse().unwrap();
                        debug!("Price update for {}: {}", self.asset, new_price);
                        self.latest_mid_price = new_price;
                        self.check_trades().await;
                    }
                }
                Message::User(user_events) => {
                    if let UserData::Fills(fills) = user_events.data {
                        for fill in fills {
                            let amount: f64 = fill.sz.parse().unwrap();
                            if fill.side.eq("B") {
                                self.current_position += amount;
                            } else {
                                self.current_position -= amount;
                            }
                            info!(
                                "Fill: {} {} {} (Total Position: {})",
                                if fill.side.eq("B") { "bought" } else { "sold" },
                                amount,
                                self.asset,
                                self.current_position
                            );
                        }
                    }
                }
                _ => {
                    debug!("Received unhandled message type");
                }
            }
        }
    }

    async fn check_trades(&mut self) {
        let current_time = Utc::now().timestamp();

        // Check long trade
        if let Some(trade) = &self.long_trade {
            let should_close = self.should_close_trade(trade, current_time).await;
            if should_close {
                debug!(
                    "Closing long trade - Entry: {}, Current: {}, Time Open: {}s",
                    trade.entry_price,
                    self.latest_mid_price,
                    current_time - trade.entry_time
                );
                self.close_trade(true).await;
                self.open_long_trade().await;
            }
        }

        // Check short trade
        if let Some(trade) = &self.short_trade {
            let should_close = self.should_close_trade(trade, current_time).await;
            if should_close {
                debug!(
                    "Closing short trade - Entry: {}, Current: {}, Time Open: {}s",
                    trade.entry_price,
                    self.latest_mid_price,
                    current_time - trade.entry_time
                );
                self.close_trade(false).await;
                self.open_short_trade().await;
            }
        }
    }

    async fn should_close_trade(&self, trade: &Trade, current_time: i64) -> bool {
        let time_open = current_time - trade.entry_time;
        let current_profit = if trade.is_long {
            (self.latest_mid_price - trade.entry_price) / trade.entry_price
        } else {
            (trade.entry_price - self.latest_mid_price) / trade.entry_price
        };

        debug!(
            "{} trade status - P&L: {:.2}%, Time Open: {}s",
            if trade.is_long { "Long" } else { "Short" },
            current_profit * 100.0,
            time_open
        );

        // Check stop loss
        if (trade.is_long && self.latest_mid_price <= trade.stop_loss) ||
            (!trade.is_long && self.latest_mid_price >= trade.stop_loss)
        {
            warn!(
                "{} Stop Loss triggered at {}",
                if trade.is_long { "Long" } else { "Short" },
                self.latest_mid_price
            );
            return true;
        }

        // Check take profit
        if (trade.is_long && self.latest_mid_price >= trade.take_profit) ||
            (!trade.is_long && self.latest_mid_price <= trade.take_profit)
        {
            info!(
                "{} Take Profit reached at {}",
                if trade.is_long { "Long" } else { "Short" },
                self.latest_mid_price
            );
            return true;
        }

        // Check 30-minute profit
        if time_open >= MID_CHECK_DURATION && current_profit > 0.0 {
            info!(
                "{} Mid-check profit taking at {:.2}%",
                if trade.is_long { "Long" } else { "Short" },
                current_profit * 100.0
            );
            return true;
        }

        // Check max duration
        if time_open >= MAX_TRADE_DURATION {
            warn!(
                "{} Max duration reached - Closing at {:.2}% P&L",
                if trade.is_long { "Long" } else { "Short" },
                current_profit * 100.0
            );
            return true;
        }

        false
    }

    async fn open_long_trade(&mut self) {
        let entry_price = self.latest_mid_price;
        let position_size = self.channel_size * LEVERAGE;
        let stop_loss = entry_price * (1.0 - SL_PERCENTAGE);
        let take_profit = entry_price * (1.0 + TP_PERCENTAGE);

        debug!(
            "Opening long trade - Size: {}, Entry: {}, SL: {}, TP: {}",
            position_size, entry_price, stop_loss, take_profit
        );

        self.place_order(position_size, entry_price).await;

        self.long_trade = Some(Trade {
            entry_price,
            position_size,
            stop_loss,
            take_profit,
            entry_time: Utc::now().timestamp(),
            is_long: true,
        });

        info!("Opened long trade at {} with size {}", entry_price, position_size);
    }

    async fn open_short_trade(&mut self) {
        let entry_price = self.latest_mid_price;
        let position_size = -self.channel_size * LEVERAGE;
        let stop_loss = entry_price * (1.0 + SL_PERCENTAGE);
        let take_profit = entry_price * (1.0 - TP_PERCENTAGE);

        debug!(
            "Opening short trade - Size: {}, Entry: {}, SL: {}, TP: {}",
            position_size, entry_price, stop_loss, take_profit
        );

        self.place_order(position_size, entry_price).await;

        self.short_trade = Some(Trade {
            entry_price,
            position_size,
            stop_loss,
            take_profit,
            entry_time: Utc::now().timestamp(),
            is_long: false,
        });

        info!("Opened short trade at {} with size {}", entry_price, position_size);
    }

    async fn close_trade(&mut self, is_long: bool) {
        let trade = if is_long { self.long_trade.take() } else { self.short_trade.take() };

        if let Some(trade) = trade {
            debug!(
                "Closing {} trade - Size: {}, Entry: {}, Exit: {}",
                if is_long { "long" } else { "short" },
                trade.position_size,
                trade.entry_price,
                self.latest_mid_price
            );

            self.place_order(-trade.position_size, self.latest_mid_price).await;
            let pnl = if is_long {
                (self.latest_mid_price - trade.entry_price) / trade.entry_price * 100.0
            } else {
                (trade.entry_price - self.latest_mid_price) / trade.entry_price * 100.0
            };

            info!(
                "Closed {} trade at {} (Entry: {}, P&L: {:.2}%)",
                if is_long { "long" } else { "short" },
                self.latest_mid_price,
                trade.entry_price,
                pnl
            );
        }
    }

    async fn place_order(&self, size: f64, price: f64) {
        let is_buy = size > 0.0;
        debug!(
            "Placing order - Side: {}, Size: {}, Price: {}",
            if is_buy { "Buy" } else { "Sell" },
            size.abs(),
            price
        );

        let order = self
            .exchange_client
            .order(
                ClientOrderRequest {
                    asset: self.asset.clone(),
                    is_buy,
                    reduce_only: false,
                    limit_px: price,
                    sz: size.abs(),
                    cloid: None,
                    order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".to_string() }),
                },
                None,
            )
            .await;

        match order {
            Ok(ExchangeResponseStatus::Ok(order)) => {
                if let Some(order) = order.data {
                    if !order.statuses.is_empty() {
                        match &order.statuses[0] {
                            ExchangeDataStatus::Filled(order) => {
                                info!(
                                    "Order filled: {} {} {} at {}",
                                    if is_buy { "Bought" } else { "Sold" },
                                    size.abs(),
                                    self.asset,
                                    price
                                );
                            }
                            ExchangeDataStatus::Resting(order) => {
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
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Initialize logging with debug level for our crate
    let filter = format!("{}=debug", env!("CARGO_PKG_NAME").replace('-', "_"));
    tracing_subscriber::fmt().with_env_filter(filter).with_file(true).with_line_number(true).init();

    let _ = dotenvy::dotenv();

    let private_key = std::env::var("PRIVATE_KEY")?;
    let user_address = std::env::var("USER_ADDRESS")?;
    let wallet = LocalWallet::from_str(&private_key)?;

    let network = if args.mainnet { BaseUrl::Mainnet } else { BaseUrl::Testnet };
    info!(
        "Starting bot on {} network with {} size",
        if args.mainnet { "mainnet" } else { "testnet" },
        args.size
    );

    let mut bot =
        DualChannelTradingBot::new(args.symbol, args.size, 2, wallet, user_address, network).await;

    bot.start().await;

    Ok(())
}
