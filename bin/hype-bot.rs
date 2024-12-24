use std::str::FromStr;

use clap::Parser;
use dotenvy::dotenv;
use ethers::{signers::LocalWallet, types::H160};
use eyre::Ok;
use hyperliquid_rust_sdk::{
    BaseUrl, ExchangeClient, InfoClient, Message, Subscription, TradeInfo, UserData,
};
use tokio::{select, signal, sync::mpsc::unbounded_channel, time::interval};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use dual_channel_bot::utils::{check_account_position, create_trade, BotParams, TradingAccount};

/// We'll print stats every 5 minutes
const STATS_INTERVAL_SECS: u64 = 300;

/// CLI arguments
#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value_t = false)]
    mainnet: bool,

    #[arg(long, default_value_t = 10.0)]
    amount: f64,

    #[arg(long, default_value_t = 3.0)]
    leverage: f64,

    #[arg(long, default_value_t = 0.01)]
    tp_percent: f64,

    #[arg(long, default_value_t = 0.04)]
    sl_percent: f64,

    #[arg(long, default_value_t = 900)]
    timeout_sec: u64,

    #[arg(long, default_value = "HYPE")]
    asset: String,
}

#[derive(Debug)]
struct DualAccountBot {
    asset: String,
    params: SimParams,
    long_account: TradingAccount,
    short_account: TradingAccount,
    info_client: InfoClient,
    latest_price: f64,
    total_pnl: f64,
}

/// Minimal struct to hold our simulation parameters
#[derive(Debug, Clone)]
struct SimParams {
    amount: f64,
    leverage: f64,
    tp_percent: f64,
    sl_percent: f64,
    timeout_sec: u64,
}

/// Convert our local `SimParams` into the `BotParams` used by `utils`
/// (Optional convenience function.)
impl From<&SimParams> for BotParams {
    fn from(sp: &SimParams) -> Self {
        Self {
            amount: sp.amount,
            leverage: sp.leverage,
            tp_percent: sp.tp_percent,
            sl_percent: sp.sl_percent,
        }
    }
}

impl DualAccountBot {
    /// Construct a new DualAccountBot
    async fn new(
        asset: String,
        params: SimParams,
        long_wallet: LocalWallet,
        short_wallet: LocalWallet,
        long_address: String,
        short_address: String,
        network: BaseUrl,
    ) -> eyre::Result<Self> {
        let info_client = InfoClient::new(None, Some(network.clone())).await?;
        let long_exchange =
            ExchangeClient::new(None, long_wallet.clone(), Some(network.clone()), None, None)
                .await?;
        let short_exchange =
            ExchangeClient::new(None, short_wallet.clone(), Some(network.clone()), None, None)
                .await?;

        let long_account = TradingAccount {
            wallet: long_wallet,
            exchange_client: long_exchange,
            user_address: H160::from_str(&long_address)?,
            active_trade: None,
            is_long_account: true,
        };

        let short_account = TradingAccount {
            wallet: short_wallet,
            exchange_client: short_exchange,
            user_address: H160::from_str(&short_address)?,
            active_trade: None,
            is_long_account: false,
        };

        Ok(Self {
            asset,
            params,
            long_account,
            short_account,
            info_client,
            latest_price: 0.0,
            total_pnl: 0.0,
        })
    }

    /// The main run loop
    async fn start(&mut self) -> eyre::Result<()> {
        let (sender, mut receiver) = unbounded_channel();

        // Subscribe to market data
        self.info_client.subscribe(Subscription::AllMids, sender.clone()).await?;

        // Subscribe to user events for both accounts
        self.info_client
            .subscribe(
                Subscription::UserEvents { user: self.long_account.user_address },
                sender.clone(),
            )
            .await?;

        self.info_client
            .subscribe(
                Subscription::UserEvents { user: self.short_account.user_address },
                sender.clone(),
            )
            .await?;

        // Wait for initial price
        info!("Waiting for initial price data...");
        loop {
            let message = receiver.recv().await.unwrap();
            if let Message::AllMids(all_mids) = message {
                if let Some(mid) = all_mids.data.mids.get(&self.asset) {
                    self.latest_price = mid.parse()?;
                    if self.latest_price > 0.0 {
                        info!("Initial price received: {}", self.latest_price);
                        break;
                    }
                }
            }
        }

        // --- Open initial positions ---
        // Build a trade for the long account
        let long_trade = create_trade(true, self.latest_price, &BotParams::from(&self.params));
        self.long_account.open_position(long_trade, &self.asset).await?;

        // Build a trade for the short account
        let short_trade = create_trade(false, self.latest_price, &BotParams::from(&self.params));
        self.short_account.open_position(short_trade, &self.asset).await?;

        // We'll print stats every STATS_INTERVAL_SECS
        let mut stats_interval = interval(std::time::Duration::from_secs(STATS_INTERVAL_SECS));

        loop {
            select! {
                Some(msg) = receiver.recv() => {
                    match msg {
                        Message::AllMids(all_mids) => {
                            if let Some(mid) = all_mids.data.mids.get(&self.asset) {
                                self.latest_price = mid.parse()?;
                                // Instead of `self.check_account_position(...)`,
                                // we call the free function from `utils`.
                                // Process the long account
                                let long_is_long_account = self.long_account.is_long_account; // Extract field
                                check_account_position(
                                    &mut self.long_account,
                                    self.latest_price,
                                    self.params.timeout_sec,
                                    long_is_long_account, // Use local variable
                                    &mut self.total_pnl,
                                    &self.asset,
                                    &BotParams::from(&self.params),
                                ).await?;

                                // Process the short account
                                let short_is_long_account = self.short_account.is_long_account; // Extract field
                                check_account_position(
                                    &mut self.short_account,
                                    self.latest_price,
                                    self.params.timeout_sec,
                                    short_is_long_account, // Use local variable
                                    &mut self.total_pnl,
                                    &self.asset,
                                    &BotParams::from(&self.params),
                                ).await?;
                            }
                        }
                        Message::User(user_events) => {
                            if let UserData::Fills(fills) = user_events.data {
                                self.handle_fills(fills).await?;
                            }
                        }
                        _ => {}
                    }
                }
                _ = stats_interval.tick() => {
                    self.print_statistics();
                }
                _ = signal::ctrl_c() => {
                    info!("Shutting down...");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle fill events (optional / example usage)
    async fn handle_fills(&mut self, fills: Vec<TradeInfo>) -> eyre::Result<()> {
        for fill in fills {
            let amount: f64 = fill.sz.parse()?;
            let price: f64 = fill.px.parse()?;

            debug!(
                "Fill received: {} {} at {}",
                if fill.side == "B" { "Buy" } else { "Sell" },
                amount,
                price
            );
        }
        Ok(())
    }

    /// Print periodic stats
    fn print_statistics(&self) {
        info!(
            "\n===== Trading Statistics =====\n\
             Total PnL: ${:.2}\n\
             Current Price: {:.3}\n\
             Long Position: {:?}\n\
             Short Position: {:?}\n",
            self.total_pnl,
            self.latest_price,
            self.long_account.active_trade,
            self.short_account.active_trade
        );
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Parse CLI args
    let args = Args::parse();

    // Initialize logging
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    // Load environment variables
    dotenv()?;

    // Load credentials for both accounts
    let long_key = std::env::var("LONG_PRIVATE_KEY")?;
    let short_key = std::env::var("SHORT_PRIVATE_KEY")?;
    let long_address = std::env::var("LONG_ADDRESS")?;
    let short_address = std::env::var("SHORT_ADDRESS")?;

    let long_wallet = LocalWallet::from_str(&long_key)?;
    let short_wallet = LocalWallet::from_str(&short_key)?;

    // Decide the network
    let network = if args.mainnet { BaseUrl::Mainnet } else { BaseUrl::Testnet };

    // Our simulation parameters
    let params = SimParams {
        amount: args.amount,
        leverage: args.leverage,
        tp_percent: args.tp_percent,
        sl_percent: args.sl_percent,
        timeout_sec: args.timeout_sec,
    };

    info!(
        "Starting dual-account bot on {} for {}",
        if args.mainnet { "mainnet" } else { "testnet" },
        args.asset
    );

    // Create the bot
    let mut bot = DualAccountBot::new(
        args.asset,
        params,
        long_wallet,
        short_wallet,
        long_address,
        short_address,
        network,
    )
    .await?;

    // Start the main run-loop
    bot.start().await?;

    Ok(())
}
