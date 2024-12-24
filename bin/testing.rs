use chrono::Utc;
use clap::{Parser, ValueEnum};
use hyperliquid_rust_sdk::{BaseUrl, InfoClient, Message, Subscription};
use tokio::{select, signal, sync::mpsc::unbounded_channel, time::interval};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use dual_channel_bot::caching::{load_ticks_from_cache, store_tick_to_cache};

// =============================================================
//  CLI + Enums
// =============================================================
#[derive(ValueEnum, Clone, Debug)]
enum RunMode {
    Live,
    Cached,
}

const PRINT_STATS_INTERVAL: u64 = 60; // in seconds
const CACHE_DIR: &str = ".cache";

/// Our CLI arguments
#[derive(Parser, Debug)]
struct Args {
    /// Run in live or cached mode
    #[arg(long, value_enum, default_value_t = RunMode::Live)]
    mode: RunMode,

    #[arg(long, default_value_t = 10.0)]
    amount: f64,

    #[arg(long, default_value_t = 3.0)]
    leverage: f64,

    #[arg(long, default_value_t = 0.01)] // e.g., 1%
    tp_percent: f64,

    #[arg(long, default_value_t = 0.04)] // e.g., 4%
    sl_percent: f64,

    #[arg(long, default_value_t = 900)] // e.g., 15 minutes in seconds
    timeout_sec: u64,

    #[arg(long, default_value_t = 0.000432)] // 0.0432%
    fees: f64,
    // In real usage, you'd have `--mainnet` or other arguments from your main code
    /// Name of the asset to trade (eg: HYPE)
    #[arg(long, default_value = "HYPE")]
    asset: String,
}

// =============================================================
//  Data Structures for Price Caching & Simulation
// =============================================================

/// If you want to store more info (like best bid/ask, volume, etc.),
/// extend this struct with additional fields. For simplicity, we only store “price”.

#[derive(Debug, Clone)]
pub struct SimulationParams {
    pub amount: f64,
    pub leverage: f64,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub timeout_sec: u64,
    pub fees: f64,
}

/// Direction of a trade
#[derive(Debug, Clone, Copy)]
enum Direction {
    Long,
    Short,
}

/// Represents one trade’s lifecycle in the simulation
#[derive(Debug, Clone)]
struct Trade {
    direction: Direction,
    open_price: f64,
    open_time: i64, // store as UTC timestamp
    close_price: Option<f64>,
    close_time: Option<i64>,
    notional: f64,
    tp_price: f64,
    sl_price: f64,
}

/// Our test-simulation framework
pub struct TestTradingFramework {
    params: SimulationParams,
    long_trade: Option<Trade>,
    short_trade: Option<Trade>,
    closed_trades: Vec<Trade>,
    total_pnl_usd: f64,
}

impl TestTradingFramework {
    /// Create a new trading framework with the given simulation parameters.
    pub fn new(params: SimulationParams) -> Self {
        Self {
            params,
            long_trade: None,
            short_trade: None,
            closed_trades: Vec::new(),
            total_pnl_usd: 0.0,
        }
    }

    /// Open both trades (long & short) initially at the same price.
    pub fn open_initial_trades(&mut self, price: f64) {
        let now_ts = Utc::now().timestamp();
        let notional = self.params.amount * self.params.leverage;

        let long_tp = price * (1.0 + self.params.tp_percent);
        let long_sl = price * (1.0 - self.params.sl_percent);
        let short_tp = price * (1.0 - self.params.tp_percent);
        let short_sl = price * (1.0 + self.params.sl_percent);

        self.long_trade = Some(Trade {
            direction: Direction::Long,
            open_price: price,
            open_time: now_ts,
            close_price: None,
            close_time: None,
            notional,
            tp_price: long_tp,
            sl_price: long_sl,
        });

        self.short_trade = Some(Trade {
            direction: Direction::Short,
            open_price: price,
            open_time: now_ts,
            close_price: None,
            close_time: None,
            notional,
            tp_price: short_tp,
            sl_price: short_sl,
        });

        info!(
            "Opened LONG at {:.4} (TP={:.4}, SL={:.4}) and SHORT at {:.4} (TP={:.4}, SL={:.4})",
            price, long_tp, long_sl, price, short_tp, short_sl
        );
    }

    /// Process each incoming price tick:  
    ///  - Print current PnL for open positions
    ///  - Close trades if conditions are met
    ///  - Re-open them immediately (so there is always one long and one short open)
    pub fn on_price_update(&mut self, price: f64) {
        let now_ts = Utc::now().timestamp();

        // Print current PnL for each trade
        self.print_current_pnl(price);

        // Check and process long trade
        if let Some(reason) = self.check_should_close(&self.long_trade, price, now_ts) {
            info!("Closing LONG => {reason}");
            if let Some(trade) = self.long_trade.take() {
                self.close_trade(trade, price);
            }
            self.open_new_trade(Direction::Long, price);
        }

        // Check and process short trade
        if let Some(reason) = self.check_should_close(&self.short_trade, price, now_ts) {
            info!("Closing SHORT => {reason}");
            if let Some(trade) = self.short_trade.take() {
                self.close_trade(trade, price);
            }
            self.open_new_trade(Direction::Short, price);
        }
    }

    /// Check if an existing trade should be closed due to either:
    ///  - A timeout (exceeding `params.timeout_sec`)
    ///  - Price hitting the trade's take-profit or stop-loss
    fn check_should_close(
        &self,
        maybe_trade: &Option<Trade>,
        current_price: f64,
        now_ts: i64,
    ) -> Option<String> {
        let trade = maybe_trade.as_ref()?;
        let elapsed = (now_ts - trade.open_time) as u64;

        // First check timeout as it's independent of price
        if elapsed >= self.params.timeout_sec {
            return Some(format!("timeout {}s", self.params.timeout_sec));
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

    /// Close the specified `trade` at the given `exit_price`.
    fn close_trade(&mut self, trade: Trade, exit_price: f64) {
        // same logic as before...
        let direction_str = match trade.direction {
            Direction::Long => "Long",
            Direction::Short => "Short",
        };

        // Calculate PnL, fees, etc.
        let pnl = match trade.direction {
            Direction::Long => (exit_price - trade.open_price) / trade.open_price * trade.notional,
            Direction::Short => (trade.open_price - exit_price) / trade.open_price * trade.notional,
        };

        let fee_cost = trade.notional * self.params.fees * 2.0;
        let net_pnl = pnl - fee_cost;
        self.total_pnl_usd += net_pnl;

        // Create closed trade record
        let closed_trade = Trade {
            close_price: Some(exit_price),
            close_time: Some(Utc::now().timestamp()),
            ..trade
        };
        self.closed_trades.push(closed_trade);

        info!(
            "Closed {direction_str}: Entry={:.4}, Exit={:.4}, PnL=${:.4} ({:.2}%), Fees=${:.4}",
            trade.open_price,
            exit_price,
            net_pnl,
            (pnl / trade.notional) * 100.0,
            fee_cost
        );
    }

    /// Re-open a new trade in the specified `direction` immediately.
    fn open_new_trade(&mut self, direction: Direction, price: f64) {
        let now_ts = Utc::now().timestamp();
        let notional = self.params.amount * self.params.leverage;

        let (tp_price, sl_price) = match direction {
            Direction::Long => {
                (price * (1.0 + self.params.tp_percent), price * (1.0 - self.params.sl_percent))
            }
            Direction::Short => {
                (price * (1.0 - self.params.tp_percent), price * (1.0 + self.params.sl_percent))
            }
        };

        let new_trade = Trade {
            direction,
            open_price: price,
            open_time: now_ts,
            close_price: None,
            close_time: None,
            notional,
            tp_price,
            sl_price,
        };

        let direction_str = match direction {
            Direction::Long => {
                self.long_trade = Some(new_trade);
                "Long"
            }
            Direction::Short => {
                self.short_trade = Some(new_trade);
                "Short"
            }
        };

        info!(
            "[RE-OPEN] {direction_str} at {:.4}, TP={:.4}, SL={:.4}, Size=${:.2}",
            price, tp_price, sl_price, notional
        );
    }

    /// Prints the *unrealized* PnL for any open trades at the current `price`.
    fn print_current_pnl(&self, price: f64) {
        if let Some(t) = &self.long_trade {
            let pct = (price - t.open_price) / t.open_price * 100.0;
            let pnl = pct * t.notional / 100.0;
            debug!(
                "LONG: Entry={:.4}, Current={:.4}, PnL=${:.2} ({:.2}%)",
                t.open_price, price, pnl, pct
            );
        }
        if let Some(t) = &self.short_trade {
            let pct = (t.open_price - price) / t.open_price * 100.0;
            let pnl = pct * t.notional / 100.0;
            debug!(
                "SHORT: Entry={:.4}, Current={:.4}, PnL=${:.2} ({:.2}%)",
                t.open_price, price, pnl, pct
            );
        }
    }

    /// Utility method to print a summary of all closed trades and total PnL.
    pub fn print_summary(&self) {
        info!("--------------- Trade Summary ---------------");

        // Counters for profitable and non-profitable trades
        let mut profitable_trades = 0;
        let mut loss_trades = 0;

        for (i, trade) in self.closed_trades.iter().enumerate() {
            // Calculate PnL for each trade
            let trade_pnl = if let Some(close_price) = trade.close_price {
                match trade.direction {
                    Direction::Long => {
                        (close_price - trade.open_price) * trade.notional / trade.open_price
                    }
                    Direction::Short => {
                        (trade.open_price - close_price) * trade.notional / trade.open_price
                    }
                }
            } else {
                0.0 // Should not occur for closed trades
            };

            // Increment counters based on PnL
            if trade_pnl > 0.0 {
                profitable_trades += 1;
            } else {
                loss_trades += 1;
            }

            // Print details of each trade
            info!(
                "#{:<2} {:?} Open={:.4}, Close={:.4?}, Notional={:.2}, PnL=${:.4}",
                i + 1,
                trade.direction,
                trade.open_price,
                trade.close_price.unwrap_or_default(),
                trade.notional,
                trade_pnl
            );
        }

        // Print the summary
        info!(
            "[STATS] Total PnL = ${:.4}, Total Profitable Trades = {}, Total Loss Trades = {}",
            self.total_pnl_usd, profitable_trades, loss_trades
        );
    }
}

// =============================================================
//  MAIN
// =============================================================

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();

    // Initialize logging/tracing
    // Initialize logging with `RUST_LOG` support
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()) // Reads `RUST_LOG`
        .init();

    // Build simulation params from CLI
    let sim_params = SimulationParams {
        amount: args.amount,
        leverage: args.leverage,
        tp_percent: args.tp_percent,
        sl_percent: args.sl_percent,
        timeout_sec: args.timeout_sec,
        fees: args.fees,
    };
    let cache_path = format!("{}/{}.json", CACHE_DIR, args.asset);

    // Create our test framework
    let mut framework = TestTradingFramework::new(sim_params);

    match args.mode {
        RunMode::Live => {
            // 1) Connect to Hyperliquid, subscribe to live price feed
            let (sender, mut receiver) = unbounded_channel();

            let network = BaseUrl::Mainnet; // or from other CLI arguments
            let mut info_client = InfoClient::new(None, Some(network.clone())).await?;
            info_client.subscribe(Subscription::AllMids, sender).await?;

            info!("Running in LIVE mode. Subscribing to real-time prices...");

            // We’ll wait for the first real price...
            let initial_price: f64;
            loop {
                // Blocks until we get a new message
                let msg = receiver.recv().await.expect("Channel closed");
                if let Message::AllMids(all_mids) = msg {
                    if let Some(mid) = all_mids.data.mids.get("HYPE") {
                        initial_price = mid.parse().unwrap();
                        break;
                    }
                }
            }

            framework.open_initial_trades(initial_price);

            let mut interval = interval(std::time::Duration::from_secs(PRINT_STATS_INTERVAL));

            // Repeatedly handle new ticks
            loop {
                select! {
                    Some(msg) = receiver.recv() => {
                        if let Message::AllMids(all_mids) = msg {
                            if let Some(mid) = all_mids.data.mids.get("HYPE") {
                                let price = mid.parse::<f64>().unwrap();
                                // 2) forward to test framework
                                framework.on_price_update(price);
                                // 3) store this tick in the cache
                                store_tick_to_cache(&cache_path, price)?;
                            }
                        }
                    },
                    _ = interval.tick() => {
                        framework.print_summary();
                    },
                    _ = signal::ctrl_c() => {
                        warn!("Ctrl+C received. Shutting down.");
                        break;
                    }
                }
            }

            info!(
                "Real-time simulation done. Final realized PnL = ${:.2}",
                framework.total_pnl_usd
            );
        }
        RunMode::Cached => {
            // 1) Load all PriceTicks from the cache file
            info!("Running in CACHED mode. Reading from {}", cache_path);
            let price_ticks = load_ticks_from_cache(&cache_path)?;

            // 2) Use the first tick to open initial trades
            if price_ticks.is_empty() {
                error!("Cache file has no data. Exiting.");
                return Ok(());
            }
            framework.open_initial_trades(price_ticks[0].price);

            // 3) Replay each tick in sequence
            for tick in price_ticks.iter().skip(1) {
                framework.on_price_update(tick.price);
                // optionally, add a small delay to mimic real-time replay
                // tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            info!("Replay done. Final realized PnL = ${:.2}", framework.total_pnl_usd);
        }
    }

    Ok(())
}
