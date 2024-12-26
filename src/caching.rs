#![allow(missing_docs)]
use chrono::Utc;
use hyperliquid_rust_sdk::CandleData;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
};

/// A single price tick (price + timestamp).
/// If you want to store more info (like best bid/ask, volume, etc.),
/// extend this struct with additional fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceTick {
    pub timestamp: i64,
    pub price: f64,
}

/// Store a tick to the cache efficiently
pub fn store_tick_to_cache(path: &str, price: f64) -> eyre::Result<()> {
    // Create a new PriceTick
    let tick = PriceTick { timestamp: Utc::now().timestamp(), price };

    // Open the file in append mode and write the tick as a compact JSON object
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = BufWriter::new(file);

    // Write the tick as a compact JSON object, followed by a newline
    serde_json::to_writer(&mut writer, &tick)?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    Ok(())
}

/// Load all ticks from the cache
pub fn load_ticks_from_cache(path: &str) -> eyre::Result<Vec<PriceTick>> {
    // Open the file for reading
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    // Read the file line by line, parsing each line as a JSON object
    let mut ticks = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let tick: PriceTick = serde_json::from_str(&line)?;
        ticks.push(tick);
    }

    Ok(ticks)
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SerializableCandleData {
    pub close: String,
    pub volume: String,
    pub interval: String,
}

impl From<&CandleData> for SerializableCandleData {
    fn from(candle: &CandleData) -> Self {
        Self {
            close: candle.close.clone(),
            volume: candle.volume.clone(),
            interval: candle.interval.clone(),
        }
    }
}

/// CandleDataRecord wraps CandleData + a timestamp for your convenience
#[derive(Serialize, Deserialize, Debug)]
pub struct CandleDataRecord {
    pub timestamp: i64,
    pub candle: SerializableCandleData,
}

/// A single candle (OHLCV + timestamp).
pub fn store_candle_to_cache(path: &str, candle: &CandleData) -> eyre::Result<()> {
    let record = CandleDataRecord {
        timestamp: chrono::Utc::now().timestamp(),
        candle: SerializableCandleData::from(candle),
    };
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = std::io::BufWriter::new(file);

    serde_json::to_writer(&mut writer, &record)?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    Ok(())
}

/// Load all candles from the cache
pub fn load_candles_from_cache(path: &str) -> eyre::Result<Vec<CandleDataRecord>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: CandleDataRecord = serde_json::from_str(&line)?;
        records.push(rec);
    }
    Ok(records)
}
