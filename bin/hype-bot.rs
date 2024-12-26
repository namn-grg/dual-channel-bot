use std::str::FromStr;

use clap::Parser;
use dotenvy::dotenv;
use ethers::{signers::LocalWallet, types::H160};
use eyre::Ok;
use hyperliquid_rust_sdk::{
    BaseUrl, ExchangeClient, InfoClient, Message, Subscription, TradeInfo, UserData,
};
use tokio::{select, signal, sync::mpsc::unbounded_channel, time::interval};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use dual_channel_bot::{
    get_price,
    utils::{check_account_position, create_trade, print_statistics, BotParams, TradingAccount},
};

/// Print stats every 5 minutes
const STATS_INTERVAL_SECS: u64 = 60;

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
        user_address_long: String,
        user_address_short: String,
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
            user_address: H160::from_str(&user_address_long)?,
            active_trade: None,
            is_long_account: true,
            closed_trades: vec![],
        };

        let short_account = TradingAccount {
            wallet: short_wallet,
            exchange_client: short_exchange,
            user_address: H160::from_str(&user_address_short)?,
            active_trade: None,
            is_long_account: false,
            closed_trades: vec![],
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

    /// Helper function to handle reconnection
    async fn handle_reconnection(&mut self, network: &BaseUrl) -> eyre::Result<()> {
        error!("Attempting to reconnect...");
        self.reconnect_clients(network).await?;
        Ok(())
    }

    async fn start(&mut self, network: BaseUrl) -> eyre::Result<()> {
        let (sender, mut receiver) = unbounded_channel();

        // Subscribe to market data
        self.info_client.subscribe(Subscription::AllMids, sender.clone()).await?;

        // Subscribe to user events for both accounts
        // self.info_client
        //     .subscribe(
        //         Subscription::UserEvents { user: self.long_account.user_address },
        //         sender.clone(),
        //     )
        //     .await?;

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
                    self.latest_price = get_price(mid.parse()?, 0.001);
                    if self.latest_price > 0.0 {
                        info!("Initial price received: {}", self.latest_price);
                        break;
                    }
                }
            }
        }

        // Open initial positions
        let long_trade = create_trade(true, self.latest_price, &BotParams::from(&self.params));
        self.long_account.open_position(long_trade, &self.asset).await?;

        let short_trade = create_trade(false, self.latest_price, &BotParams::from(&self.params));
        self.short_account.open_position(short_trade, &self.asset).await?;

        let mut stats_interval = interval(std::time::Duration::from_secs(STATS_INTERVAL_SECS));

        loop {
            let result = async {
                select! {
                    Some(msg) = receiver.recv() => {
                        match msg {
                            Message::AllMids(all_mids) => {
                                if let Some(mid) = all_mids.data.mids.get(&self.asset) {
                                    self.latest_price = get_price(mid.parse()?, 0.001);
                                    self.print_current_pnl();

                                    let long_is_long_account = self.long_account.is_long_account;
                                    check_account_position(
                                        &mut self.long_account,
                                        self.latest_price,
                                        self.params.timeout_sec,
                                        long_is_long_account,
                                        &mut self.total_pnl,
                                        &self.asset,
                                        &BotParams::from(&self.params),
                                    ).await?;

                                    let short_is_long_account = self.short_account.is_long_account;
                                    check_account_position(
                                        &mut self.short_account,
                                        self.latest_price,
                                        self.params.timeout_sec,
                                        short_is_long_account,
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
                        print_statistics(&self.long_account.closed_trades);
                        print_statistics(&self.short_account.closed_trades);
                    }
                    _ = signal::ctrl_c() => {
                        info!("Shutting down...");
                        return Err(eyre::eyre!("Shutting down"));
                    }
                }
                Ok(())
            }
            .await;

            if let Err(e) = result {
                error!("Error occurred: {}", e);
                break Ok(());
                // self.handle_reconnection(&network).await?;
            }
        }
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

    /// Reconnect the InfoClient and ExchangeClients in case of disconnection
    async fn reconnect_clients(&mut self, network: &BaseUrl) -> eyre::Result<()> {
        info!("Reconnecting InfoClient and ExchangeClients...");

        // Attempt to recreate InfoClient
        self.info_client = InfoClient::new(None, Some(network.clone())).await?;

        // Attempt to recreate ExchangeClient for long account
        self.long_account.exchange_client = ExchangeClient::new(
            None,
            self.long_account.wallet.clone(),
            Some(network.clone()),
            None,
            None,
        )
        .await?;

        // Attempt to recreate ExchangeClient for short account
        self.short_account.exchange_client = ExchangeClient::new(
            None,
            self.short_account.wallet.clone(),
            Some(network.clone()),
            None,
            None,
        )
        .await?;

        info!("Reconnection successful.");

        Ok(())
    }

    /// Print the current PnL of the long and short accounts.
    /// We calculate unrealized PnL as:
    ///     PnL = (current_price - entry_price) / entry_price * 100  (for Long positions)
    ///     PnL = (entry_price - current_price) / entry_price * 100  (for Short positions)
    fn print_current_pnl(&self) {
        let current_price = self.latest_price;

        // Print LONG account PnL
        if let Some(trade) = &self.long_account.active_trade {
            let pnl = (current_price - trade.entry_price) / trade.entry_price * 100.0;
            debug!(
                "LONG Account: Entry Price={:.4}, Current Price={:.4}, PnL={:.4}%",
                trade.entry_price, current_price, pnl
            );
        } else {
            debug!("LONG Account: No active trade.");
        }

        // Print SHORT account PnL
        if let Some(trade) = &self.short_account.active_trade {
            let pnl = (trade.entry_price - current_price) / trade.entry_price * 100.0;
            debug!(
                "SHORT Account: Entry Price={:.4}, Current Price={:.4}, PnL={:.4}",
                trade.entry_price, current_price, pnl
            );
        } else {
            debug!("SHORT Account: No active trade.");
        }
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
    let private_key_long = std::env::var("PRIVATE_KEY_LONG")?;
    let private_key_short = std::env::var("PRIVATE_KEY_SHORT")?;
    let user_address_long = std::env::var("USER_ADDRESS_LONG")?;
    let user_address_short = std::env::var("USER_ADDRESS_SHORT")?;

    let long_wallet = LocalWallet::from_str(&private_key_long)?;
    let short_wallet = LocalWallet::from_str(&private_key_short)?;

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
        user_address_long,
        user_address_short,
        network,
    )
    .await?;

    // Start the main run-loop
    bot.start(network).await?;

    Ok(())
}
