use chrono::{DateTime, Utc};
use ethers::{
    signers::{LocalWallet, Signer},
    types::H160,
};
use log::{error, info};
use std::collections::VecDeque;
use std::str::FromStr;
use tokio::sync::mpsc::unbounded_channel;

use hyperliquid_rust_sdk::{
    bps_diff, truncate_float, BaseUrl, CandleData, ClientCancelRequest, ClientLimit, ClientOrder,
    ClientOrderRequest, ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
    Message, Subscription, UserData,
};

const LEVERAGE: f64 = 3.0;
const TP_PERCENTAGE: f64 = 0.02; // 2%
const SL_PERCENTAGE: f64 = 0.04; // 4%
const MAX_TRADE_DURATION: i64 = 3600; // 1 hour in seconds
const MID_CHECK_DURATION: i64 = 1800; // 30 minutes in seconds

struct Trade {
    entry_price: f64,
    position_size: f64,
    stop_loss: f64,
    take_profit: f64,
    entry_time: i64,
    is_long: bool,
}

pub struct DualChannelTradingBot {
    asset: String,
    channel_size: f64, // size per channel (long/short)
    decimals: u32,
    current_position: f64,
    latest_mid_price: f64,
    info_client: InfoClient,
    exchange_client: ExchangeClient,
    user_address: H160,
    long_trade: Option<Trade>,
    short_trade: Option<Trade>,
}

impl DualChannelTradingBot {
    pub async fn new(
        asset: String,
        channel_size: f64,
        decimals: u32,
        wallet: LocalWallet,
    ) -> DualChannelTradingBot {
        let user_address = wallet.address();

        let info_client = InfoClient::new(None, Some(BaseUrl::Testnet)).await.unwrap();
        let exchange_client = ExchangeClient::new(None, wallet, Some(BaseUrl::Testnet), None, None)
            .await
            .unwrap();

        DualChannelTradingBot {
            asset,
            channel_size,
            decimals,
            current_position: 0.0,
            latest_mid_price: -1.0,
            info_client,
            exchange_client,
            user_address,
            long_trade: None,
            short_trade: None,
        }
    }

    pub async fn start(&mut self) {
        let (sender, mut receiver) = unbounded_channel();

        // Subscribe to necessary feeds
        self.info_client
            .subscribe(
                Subscription::UserEvents {
                    user: self.user_address,
                },
                sender.clone(),
            )
            .await
            .unwrap();

        self.info_client
            .subscribe(Subscription::AllMids, sender.clone())
            .await
            .unwrap();

        // Initial trades
        self.open_long_trade().await;
        self.open_short_trade().await;

        loop {
            let message = receiver.recv().await.unwrap();
            match message {
                Message::AllMids(all_mids) => {
                    let all_mids = all_mids.data.mids;
                    if let Some(mid) = all_mids.get(&self.asset) {
                        self.latest_mid_price = mid.parse().unwrap();
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
                                "Fill: {} {} {}",
                                if fill.side.eq("B") { "bought" } else { "sold" },
                                amount,
                                self.asset
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    async fn check_trades(&mut self) {
        let current_time = Utc::now().timestamp();

        // Check long trade
        if let Some(trade) = &self.long_trade {
            let should_close = self.should_close_trade(trade, current_time).await;
            if should_close {
                self.close_trade(true).await;
                self.open_long_trade().await;
            }
        }

        // Check short trade
        if let Some(trade) = &self.short_trade {
            let should_close = self.should_close_trade(trade, current_time).await;
            if should_close {
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

        // Check stop loss
        if (trade.is_long && self.latest_mid_price <= trade.stop_loss)
            || (!trade.is_long && self.latest_mid_price >= trade.stop_loss)
        {
            return true;
        }

        // Check take profit
        if (trade.is_long && self.latest_mid_price >= trade.take_profit)
            || (!trade.is_long && self.latest_mid_price <= trade.take_profit)
        {
            return true;
        }

        // Check 30-minute profit
        if time_open >= MID_CHECK_DURATION && current_profit > 0.0 {
            return true;
        }

        // Check max duration
        if time_open >= MAX_TRADE_DURATION {
            return true;
        }

        false
    }

    async fn open_long_trade(&mut self) {
        let entry_price = self.latest_mid_price;
        let position_size = self.channel_size * LEVERAGE;
        let stop_loss = entry_price * (1.0 - SL_PERCENTAGE);
        let take_profit = entry_price * (1.0 + TP_PERCENTAGE);

        self.place_order(position_size, entry_price).await;

        self.long_trade = Some(Trade {
            entry_price,
            position_size,
            stop_loss,
            take_profit,
            entry_time: Utc::now().timestamp(),
            is_long: true,
        });

        info!(
            "Opened long trade at {} with size {}",
            entry_price, position_size
        );
    }

    async fn open_short_trade(&mut self) {
        let entry_price = self.latest_mid_price;
        let position_size = -self.channel_size * LEVERAGE;
        let stop_loss = entry_price * (1.0 + SL_PERCENTAGE);
        let take_profit = entry_price * (1.0 - TP_PERCENTAGE);

        self.place_order(position_size, entry_price).await;

        self.short_trade = Some(Trade {
            entry_price,
            position_size,
            stop_loss,
            take_profit,
            entry_time: Utc::now().timestamp(),
            is_long: false,
        });

        info!(
            "Opened short trade at {} with size {}",
            entry_price, position_size
        );
    }

    async fn close_trade(&mut self, is_long: bool) {
        let trade = if is_long {
            self.long_trade.take()
        } else {
            self.short_trade.take()
        };

        if let Some(trade) = trade {
            self.place_order(-trade.position_size, self.latest_mid_price)
                .await;
            info!(
                "Closed {} trade at {} (Entry: {})",
                if is_long { "long" } else { "short" },
                self.latest_mid_price,
                trade.entry_price
            );
        }
    }

    async fn place_order(&self, size: f64, price: f64) {
        let is_buy = size > 0.0;
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
                    order_type: ClientOrder::Limit(ClientLimit {
                        tif: "Gtc".to_string(),
                    }),
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
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv()?;
    let private_key = std::env::var("PRIVATE_KEY")?;
    let wallet = LocalWallet::from_str(&private_key)?;

    let mut bot = DualChannelTradingBot::new("HYPE".to_string(), 100.0, 2, wallet).await;
    bot.start().await;

    Ok(())
}
