# DUAL CHANNEL BOT 🔀

A Rust-based trading bot that operates across dual channels on cryptocurrency markets.

## Features

-   Supports both mainnet and testnet environments
-   Dual channel trading strategy
-   Built in Rust for high performance and reliability

## Setup

1. Clone the repository

```bash
git clone https://github.com/yourusername/dual-channel-bot.git
cd dual-channel-bot
```

2. Configure your environment variables (create a `.env` file):

```bash
PRIVATE_KEY=
USER_ADDRESS=
```

3. Build the bot

```bash
cargo build --release
```

4. Run the bot

```bash
# Run on testnet (default)
cargo run

# Run on mainnet
cargo run -- --mainnet

# Run with custom size
cargo run -- --size 50.0

# Run with different symbol
cargo run -- --symbol ETH

# Run with all options
cargo run -- --mainnet --size 50.0 --symbol ETH
```

## Configuration

The bot can be configured with the following parameters:

-   Network selection (mainnet/testnet)
-   Channel size
-   Asset selection

## License

MIT License

## Disclaimer

Trading cryptocurrencies involves risk. This bot is provided as-is with no guarantees. Use at your own risk.
