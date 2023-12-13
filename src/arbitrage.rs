use super::constants::{ASSET_HOLDINGS, USD_BUDGET, MAX_SYMBOLS_WATCH, MIN_ARB_THRESH, UPDATE_SYMBOLS_SECONDS};
use super::bellmanford::Edge;
use crate::exchanges::binance::Binance;
use super::helpers;
use super::models::{ArbData, BookType, Direction, SmartError};
use super::traits::{ApiCalls, BellmanFordEx, ExchangeData};

use csv::Writer;
use futures::future::join_all;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::fs::OpenOptions;
use std::collections::HashSet;

/// Calculate Weighted Average Price
/// Calculates the depth of the orderbook to get a real rate
fn calculate_weighted_average_price(
    orderbook: &Vec<(f64, f64)>,
    budget: f64,
    direction: &Direction,
) -> Option<(f64, f64, f64)> {
    let mut total_cost = 0.0;
    let mut total_quantity = 0.0;

    for &(price, quantity) in orderbook.iter() {

        // Effective quantity is the amount of the quote asset you receive (or spend in reverse)
        let effective_quantity = match direction {
            Direction::Reverse => quantity,
            Direction::Forward => quantity * price,
        };

        // Cost is the amount of the base asset you spend (or receive in forward)
        let cost = match direction {
            Direction::Reverse => quantity * price,
            Direction::Forward => quantity,
        };

        // Check if adding this order exceeds the budget
        if total_cost + cost > budget {
            let remaining_budget = budget - total_cost;

            // Adjust the remaining quantity based on the direction of the trade
            let remaining_quantity = match direction {
                Direction::Reverse => remaining_budget / price,
                Direction::Forward => remaining_budget * price, // In forward, get quote asset amount
            };

            total_cost += remaining_budget;
            total_quantity += remaining_quantity;
            break;

        } else {
            total_cost += cost;
            total_quantity += effective_quantity;
        }

        if total_cost >= budget {
            break;
        }
    }

    if total_quantity == 0.0 {
        return None;
    }

    // Weighted average price calculation
    let weighted_average_price = match direction {
        Direction::Reverse => total_cost / total_quantity,
        Direction::Forward => total_quantity / total_cost,
    };
  
    Some((weighted_average_price, total_cost, total_quantity))
}

/// Calculate Arbitrage
/// Calculates arbitrage given relevant inputs and orderbooks
fn calculate_arbitrage<T>(
    orderbooks: &Vec<Vec<(f64, f64)>>,
    symbols: &Vec<String>,
    directions: &Vec<Direction>,
    budget: f64,
    exchange: &T,
) -> Option<(f64, Vec<f64>)> 
where T: BellmanFordEx + ExchangeData + ApiCalls {

    // Initialize
    let mut real_rate = 1.0;
    let mut quantities_input = vec![];
    let mut amount_in = budget;

    // Perform arbitrage calculation
    for i in 0..symbols.len() {
        let symbol = &symbols[i];
        let direction = &directions[i];
        let orderbook = &orderbooks[i];

        // Guard: Validate quantity
        let symbol_info = exchange.symbols().get(symbol.as_str())?;
        let price = exchange.prices().get(symbol.as_str())?;
        let quantity = match helpers::validate_quantity(symbol_info, amount_in, *price) {
            Ok(quantity) => quantity,
            Err(_e) => {
                // eprintln!("Failed to validate quantity: {:?}", _e);
                return None;
            }
        };

        // Add quantity
        quantities_input.push(quantity);

        // Calculate Average Price and quantity out - first pass
        let trade_res: Option<(f64, f64, f64)> = calculate_weighted_average_price(orderbook, amount_in, &direction);
        let (weighted_price, total_qty) = match trade_res {
            Some((wp, _, qty)) => (wp, qty),
            None => {
                // eprintln!("Error calculating weighted price...");
                return None;
            }
        };

        // Update budget amount in
        amount_in = total_qty;

        // Calculate Real Rate
        match direction {
            Direction::Forward => real_rate *= weighted_price,
            Direction::Reverse => real_rate *= 1.0 / weighted_price,
        }
    }

    // Return results
    Some((real_rate, quantities_input))
}


/// Validate Arbitrage Cycle
/// Validates arbitrage cycle has enough depth
pub async fn validate_arbitrage_cycle<T: BellmanFordEx>(cycle: &Vec<Edge>, exchange: &T) -> Option<(f64, Vec<f64>, Vec<String>)> 
where T: BellmanFordEx + ExchangeData + ApiCalls {

    // Guard: Ensure cycle
    if cycle.len() == 0 { return None };

    // Guard: Ensure asset holding
    let from = cycle[0].from.as_str();
    if !ASSET_HOLDINGS.contains(&from) {
        // eprintln!("Asset not in holding: {}", from);
        return None
    }

    // Get starting budget
    let budget = match from {
        "BTC" => USD_BUDGET / exchange.prices().get("BTCUSDT").expect("Expected price for BTCUSDT").to_owned(),
        "ETH" => USD_BUDGET / exchange.prices().get("ETHUSDT").expect("Expected price for ETHUSDT").to_owned(),
        "BNB" => USD_BUDGET / exchange.prices().get("BNBUSDT").expect("Expected price for BNBUSDT").to_owned(),
        "LINK" => USD_BUDGET / exchange.prices().get("LINKUSDT").expect("Expected price for LINKUSDT").to_owned(),
        "USDT" => USD_BUDGET,
        "BUSD" => USD_BUDGET,
        "USDC" => USD_BUDGET,
        _ => {
            eprintln!("{} not recognised as meaningful starting point", from);
            return None
        }
    };

    // Initialize
    let mut symbols: Vec<String> = vec![];
    let mut directions: Vec<Direction> = vec![];
    let mut book_types: Vec<BookType> = vec![];
    let mut orderbooks: Vec<Vec<(f64, f64)>> = vec![];

    // Extract info for parallel async orderbook fetching
    for leg in cycle {
        let symbol_1 = format!("{}{}", leg.to, leg.from);
        let symbol_2 = format!("{}{}", leg.from, leg.to);
        let symbol = if exchange.symbols().contains_key(symbol_1.as_str()) { symbol_1 } else { symbol_2 };
        let book_type = if symbol.starts_with(leg.from.as_str()) { BookType::Asks } else { BookType::Bids };
        let direction = if symbol.starts_with(leg.from.as_str()) { Direction::Forward } else { Direction::Reverse };

        symbols.push(symbol);
        directions.push(direction);
        book_types.push(book_type);
    }

    // Build futures for orderbook asyncronous extraction
    let futures: Vec<_> = symbols.iter().zip(book_types.iter())
        .map(|(symbol, book_type)| exchange.get_orderbook_depth(symbol.as_str(), book_type.clone()))
        .collect();

    // Call api for orderbooks
    let results: Vec<Result<Vec<(f64, f64)>, SmartError>> = join_all(futures).await;

    // Guard: Ensure orderbook results
    for result in results {
        match result {
            Ok(book) => orderbooks.push(book),
            Err(e) => {
                eprintln!("Error fetching order book: {:?}", e);
                return None
            },
        }
    }

    // Calculate Arbitrage
    let Some((real_rate, quantities)) = calculate_arbitrage::<T>(&orderbooks, &symbols, &directions, budget, exchange) else { return None };

    // Return result
    Some((real_rate, quantities, symbols))
}

/// Store Arb
/// Stores Arb found in table for later analysis
pub fn store_arb_cycle(cycle: &Vec<Edge>, arb_rate: f64, arb_surface: f64) -> Result<(), SmartError> {

    // Get unique assets
    let mut assets_hs: HashSet<String> = HashSet::new();
    for leg in cycle {
        assets_hs.insert(leg.from.clone());
        assets_hs.insert(leg.to.clone());
    }
    
    let timestamp: u64 = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let arb_length = cycle.len();
    let arb_assets: Vec<&String> = assets_hs.iter().collect();
    
    let asset_0 = if arb_assets.len() > 0 { Some(arb_assets[0].to_owned()) } else { None };
    let asset_1 = if arb_assets.len() > 1 { Some(arb_assets[1].to_owned()) } else { None };
    let asset_2 = if arb_assets.len() > 2 { Some(arb_assets[2].to_owned()) } else { None };
    let asset_3 = if arb_assets.len() > 3 { Some(arb_assets[3].to_owned()) } else { None };
    let asset_4 = if arb_assets.len() > 4 { Some(arb_assets[4].to_owned()) } else { None };
    let asset_5 = if arb_assets.len() > 5 { Some(arb_assets[5].to_owned()) } else { None };
    let asset_6 = if arb_assets.len() > 6 { Some(arb_assets[6].to_owned()) } else { None };
    let asset_7 = if arb_assets.len() > 7 { Some(arb_assets[7].to_owned()) } else { None };

    // Create an ArbData instance
    let data: ArbData = ArbData {
        timestamp,
        arb_length,
        arb_rate,
        arb_surface,
        asset_0,
        asset_1,
        asset_2,
        asset_3,
        asset_4,
        asset_5,
        asset_6,
        asset_7
    };

    // Create or append to a CSV file
    let file: std::fs::File = OpenOptions::new()
        .write(true)
        .append(true)
        .create(true)
        .open("/Users/shaun/Code/DEVELOPMENT/hft/bellman_ford_pegasus/arbitrage_data.csv")?;

    // Write the data to the CSV file
    let mut wtr = Writer::from_writer(file);
    wtr.serialize(data)?;

    // Ensure all data is flushed to the file
    wtr.flush()?;

    Ok(())
}

/// Calculate Arbitrage Surface Rate
/// Calculates the surface rate of an arbitrage opportunity
fn calculate_arbitrage_surface_rate(cycle: &Vec<Edge>) -> f64 {
    cycle.iter().fold(1.0, |acc, edge| acc * f64::exp(-edge.weight)) - 1.0
}

/// Best Symbols
/// Finds the assets that appear most often within the required arb threshold
pub async fn best_symbols_thread(best_symbols: Arc<Mutex<Vec<String>>>) -> Result<(), SmartError> {

    // Initialize
    println!("thread: best symbols running...");
    let ignore_list = ["BTC", "USDT"];

    let mut symbols_hs: HashSet<String> = HashSet::new();
    let mut timestamp: u64 = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut save_timestamp: u64 = timestamp + UPDATE_SYMBOLS_SECONDS;

    loop {
        std::thread::sleep(Duration::from_millis(100));
        timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        let exch_binance = Binance::new().await;
        let cycles = exch_binance.run_bellman_ford_multi();
        for cycle in cycles {
            let arb_opt = validate_arbitrage_cycle(&cycle, &exch_binance).await;
            if let Some((arb_rate, _, _)) = arb_opt {

                // // Use if wanting to store and track arbitrage opportunities
                // let _arb_surface = calculate_arbitrage_surface_rate(&cycle) + 1.0;
                // let _: () = arbitrage::store_arb_cycle(&cycle, arb_rate, arb_surface).unwrap();

                if arb_rate >= MIN_ARB_THRESH {
                    for leg in cycle {
                        if symbols_hs.len() < MAX_SYMBOLS_WATCH && !ignore_list.contains(&leg.from.as_str()) { symbols_hs.insert(leg.from); }
                        if symbols_hs.len() < MAX_SYMBOLS_WATCH && !ignore_list.contains(&leg.to.as_str()) { symbols_hs.insert(leg.to); }
                    }
                }
            }
        }

        // Update best symbols
        if timestamp >= save_timestamp && symbols_hs.len() == MAX_SYMBOLS_WATCH {
            dbg!("updating best symbols");
            let sym_list: Vec<String> = symbols_hs.iter().map(|s| s.clone()).collect();
            let mut new_best_symbols: Vec<String> = vec![];
            for i in 0..sym_list.len() {
                let sym_1 = format!("{}USDT", sym_list[i]);
                let sym_2 = format!("{}BTC", sym_list[i]);
                new_best_symbols.push(sym_1);
                new_best_symbols.push(sym_2);
            }

            // Update shared symbols state
            let mut shared_symbols = best_symbols.lock().unwrap();
            shared_symbols.clear();
            shared_symbols.extend(new_best_symbols);

            // Clear set
            symbols_hs.clear();

            // Update next save timestamp
            save_timestamp = timestamp + UPDATE_SYMBOLS_SECONDS;
        }
    }
 }
 