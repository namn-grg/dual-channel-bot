// utils.rs
#![allow(dead_code)]
#![allow(missing_docs)]

use std::time::Duration;

use chrono::Utc;
use ethers::{signers::LocalWallet, types::H160};
use hyperliquid_rust_sdk::{
    ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, ExchangeDataStatus,
    ExchangeResponseStatus,
};
use tokio::time::sleep;
use tracing::{debug, error, info, trace};

/// A small delay before re-opening a position after closing one
pub const SLEEP_BEFORE_OPENING_POSITION: u64 = 3;

// ----------------------------------------
// Existing helpers (rounding, direction, trades, etc.)
// ----------------------------------------

pub fn get_price(price: f64, tick_size: f64) -> f64 {
    (price / tick_size).round() * tick_size
}

pub fn get_size(amount: f64, price: f64) -> f64 {
    let sz_decimals = 2;
    ((amount.abs() / price) * 10f64.powi(sz_decimals as i32)).round() /
        10f64.powi(sz_decimals as i32)
}

/// Utility function to print statistics for closed trades
pub fn print_statistics(closed_trades: &Vec<Trade>) {
    if closed_trades.is_empty() {
        println!("No trades to summarize.");
        return;
    }

    // Initialize counters and accumulators
    let mut total_pnl = 0.0;
    let mut total_profit = 0.0;
    let mut total_loss = 0.0;
    let mut profitable_trades = 0;
    let mut loss_trades = 0;

    println!("--------------- Trade Statistics ---------------");

    // Process each trade
    for (i, trade) in closed_trades.iter().enumerate() {
        let trade_pnl = if let Some(close_price) = trade.close_price {
            match trade.direction {
                Direction::Long => (close_price - trade.entry_price) * trade.size,
                Direction::Short => (trade.entry_price - close_price) * trade.size,
            }
        } else {
            0.0 // Should not occur for closed trades
        };

        // Update statistics
        total_pnl += trade_pnl;

        if trade_pnl > 0.0 {
            profitable_trades += 1;
            total_profit += trade_pnl;
        } else {
            loss_trades += 1;
            total_loss += trade_pnl;
        }

        // Calculate PnL percentage
        let trade_pnl_percentage = if let Some(close_price) = trade.close_price {
            match trade.direction {
                Direction::Long => ((close_price - trade.entry_price) / trade.entry_price) * 100.0,
                Direction::Short => ((trade.entry_price - close_price) / trade.entry_price) * 100.0,
            }
        } else {
            0.0
        };

        // Print individual trade details
        trace!(
            "#{:<2} {:?} | Entry: {:.4}, Close: {:.4?}, Size: {:.2}, PnL%: {:.4}%",
            i + 1,
            trade.direction,
            trade.entry_price,
            trade.close_price.unwrap_or_default(),
            trade.size,
            trade_pnl_percentage
        );
    }

    // Print summary
    info!(
        "Total Trades: {}, Profitable Trades: {}, Loss Trades: {}",
        closed_trades.len(),
        profitable_trades,
        loss_trades
    );
    info!(
        "Total PnL: ${:.4}, Total Profit: ${:.4}, Total Loss: ${:.4}",
        total_pnl, total_profit, total_loss
    );
    println!("-------------------------------------------------");
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy)]
pub struct Trade {
    pub direction: Direction,
    pub entry_price: f64,
    pub entry_time: i64, // store as UTC timestamp
    pub size: f64,
    pub tp_price: f64,
    pub sl_price: f64,
    pub close_price: Option<f64>,
}

pub fn check_should_close(
    trade: &Trade,
    current_price: f64,
    current_ts: i64,
    timeout: u64,
) -> Option<String> {
    let elapsed = (current_ts - trade.entry_time) as u64;

    // First check timeout as it's independent of price
    if elapsed >= timeout {
        return Some(format!("timeout {}s", timeout));
    }

    // Then check price conditions
    match trade.direction {
        Direction::Long => {
            if current_price >= trade.tp_price {
                Some(format!("take-profit at {:.4}", current_price))
            } else if current_price <= trade.sl_price {
                Some(format!("stop-loss at {:.4}", current_price))
            } else {
                None
            }
        }
        Direction::Short => {
            if current_price <= trade.tp_price {
                Some(format!("take-profit at {:.4}", current_price))
            } else if current_price >= trade.sl_price {
                Some(format!("stop-loss at {:.4}", current_price))
            } else {
                None
            }
        }
    }
}

/// Holds parameter values that come from your bot or CLI.
#[derive(Debug, Clone)]
pub struct BotParams {
    pub amount: f64,
    pub leverage: f64,
    pub tp_percent: f64,
    pub sl_percent: f64,
}

/// Account to trade on Hyperliquid
#[derive(Debug)]
pub struct TradingAccount {
    pub wallet: LocalWallet,
    pub exchange_client: ExchangeClient,
    pub user_address: H160,
    pub active_trade: Option<Trade>,
    pub is_long_account: bool,
    pub closed_trades: Vec<Trade>,
}

/// Creates a new `Trade` with the given direction, using your `BotParams`.
pub fn create_trade(is_long: bool, latest_price: f64, params: &BotParams) -> Trade {
    let size = get_size(params.amount * params.leverage, latest_price);

    let tp_price = if is_long {
        latest_price * (1.0 + params.tp_percent)
    } else {
        latest_price * (1.0 - params.tp_percent)
    };

    let sl_price = if is_long {
        latest_price * (1.0 - params.sl_percent)
    } else {
        latest_price * (1.0 + params.sl_percent)
    };

    Trade {
        direction: if is_long { Direction::Long } else { Direction::Short },
        entry_price: latest_price,
        entry_time: Utc::now().timestamp(),
        size,
        tp_price,
        sl_price,
        close_price: None,
    }
}

impl TradingAccount {
    /// Opens a new position on Hyperliquid with the given `Trade` details.
    pub async fn open_position(&mut self, trade: Trade, asset: &str) -> eyre::Result<()> {
        let order = self
            .exchange_client
            .order(
                ClientOrderRequest {
                    asset: asset.to_string(),
                    is_buy: trade.direction == Direction::Long,
                    reduce_only: false,
                    limit_px: trade.entry_price,
                    sz: trade.size.abs(),
                    cloid: None,
                    order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".to_string() }),
                },
                None,
            )
            .await?;
        debug!("Order response: {:?}", order);

        // Not doing inside the fill block to avoid case when its resting and then filled.
        self.active_trade = Some(trade);

        match order {
            ExchangeResponseStatus::Ok(response) => {
                if let Some(data) = response.data {
                    match &data.statuses[0] {
                        ExchangeDataStatus::Filled(_) => {
                            info!(
                                "Opened {} position at {:.3} (TP: {:.3}, SL: {:.3})",
                                if trade.direction == Direction::Long { "LONG" } else { "SHORT" },
                                trade.entry_price,
                                trade.tp_price,
                                trade.sl_price
                            );
                        }
                        ExchangeDataStatus::Error(e) => {
                            error!("Order error: {}", e);
                        }
                        _ => {}
                    }
                }
            }
            ExchangeResponseStatus::Err(e) => {
                error!("Order error: {}", e);
            }
        }

        Ok(())
    }
}

/// Closes the currently active position in `account`, updates PnL, logs info, etc.
///
/// - `current_price`: The market price at which we are closing
/// - `asset`: The symbol/asset to trade (e.g., "HYPE")
/// - `total_pnl`: A mutable reference to your bot's aggregated PnL so far
pub async fn close_position(
    account: &mut TradingAccount,
    current_price: f64,
    asset: &str,
    total_pnl: &mut f64,
) -> eyre::Result<()> {
    if let Some(mut trade) = account.active_trade.take() {
        // Mark trade as closed
        trade.close_price = Some(current_price);

        // The close order size is the opposite of the open order
        let close_size = -trade.size;
        let order = account
            .exchange_client
            .order(
                ClientOrderRequest {
                    asset: asset.to_string(),
                    is_buy: close_size > 0.0,
                    reduce_only: true,
                    limit_px: current_price,
                    sz: close_size.abs(),
                    cloid: None,
                    order_type: ClientOrder::Limit(ClientLimit { tif: "Gtc".to_string() }),
                },
                None,
            )
            .await?;
        debug!("Close Order response: {:?}", order);

        match order {
            ExchangeResponseStatus::Ok(_) => {
                // Calculate PnL for this trade
                let pnl = if trade.size > 0.0 {
                    (current_price - trade.entry_price) / trade.entry_price * trade.size
                } else {
                    (trade.entry_price - current_price) / trade.entry_price * trade.size
                };

                *total_pnl += pnl;
                account.closed_trades.push(trade);

                info!(
                    "Closed {} position - Entry: {:.3}, Exit: {:.3}, PnL: {:.2}",
                    if account.is_long_account { "LONG" } else { "SHORT" },
                    trade.entry_price,
                    current_price,
                    pnl
                );
            }
            ExchangeResponseStatus::Err(e) => {
                error!("Error closing position: {}", e);
            }
        }
    }
    Ok(())
}

/// Checks whether to close an existing trade (due to TP, SL, or timeout).
/// If close occurs, optionally re-opens a new trade after sleeping.
pub async fn check_account_position(
    account: &mut TradingAccount,
    current_price: f64,
    timeout_sec: u64,
    is_long_account: bool,
    total_pnl: &mut f64,
    asset: &str,
    params: &BotParams,
) -> eyre::Result<()> {
    if let Some(trade) = &account.active_trade {
        if let Some(reason) =
            check_should_close(trade, current_price, Utc::now().timestamp(), timeout_sec)
        {
            info!("Closing {} => {}", if is_long_account { "LONG" } else { "SHORT" }, reason);
            // 1) Close
            close_position(account, current_price, asset, total_pnl).await?;

            // 2) Sleep briefly (optional)
            sleep(Duration::from_secs(SLEEP_BEFORE_OPENING_POSITION)).await;

            // 3) Create and open a new trade
            let new_trade = create_trade(is_long_account, current_price, params);
            account.open_position(new_trade, asset).await?;
        }
    }
    Ok(())
}
