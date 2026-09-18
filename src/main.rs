mod api;
mod db;
mod indexer;
mod names;
mod rpc;
mod zmq;
mod zrc20;
mod zrc721;

use anyhow::Result;
use std::env;
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Logging setup
    // Honor RUST_LOG if provided, otherwise fall back to VERBOSE_LOGS
    let max_level = match env::var("RUST_LOG").ok().as_deref() {
        Some("trace") | Some("TRACE") => tracing::Level::TRACE,
        Some("debug") | Some("DEBUG") => tracing::Level::DEBUG,
        Some("info") | Some("INFO") => tracing::Level::INFO,
        Some("warn") | Some("WARN") => tracing::Level::WARN,
        Some("error") | Some("ERROR") => tracing::Level::ERROR,
        _ => {
            let verbose = env::var("VERBOSE_LOGS")
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false);
            if verbose { tracing::Level::DEBUG } else { tracing::Level::INFO }
        }
    };

    let subscriber = FmtSubscriber::builder().with_max_level(max_level).finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Runtime configuration
    let db_path = env::var("DB_PATH").unwrap_or("./data/index".to_string());
    let api_port = env::var("API_PORT")
        .or_else(|_| env::var("PORT"))
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()?;

    // Construct core services
    let reindex = env::var("RE_INDEX")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    let repair = env::var("REPAIR_ZRC721")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    let db = db::Db::new(&db_path, reindex)?;
    let rpc = rpc::ZcashRpcClient::new();

    if let Ok(txid) = env::var("GET_BLOCK_HEIGHT_FOR_TXID") {
        tracing::info!("Looking up block height for txid: {}", txid);
        let tx = rpc.get_raw_transaction(&txid).await?;
        if let Some(hash) = tx.blockhash {
            let block = rpc.get_block(&hash).await?;
            tracing::info!("Transaction {} is in block {}", txid, block.height);
            println!("{}", block.height); // Print to stdout for easy capture
        } else {
            tracing::error!("Transaction {} is not confirmed in a block yet", txid);
        }
        return Ok(());
    }

    let indexer = indexer::Indexer::new(rpc, db.clone());

    if repair {
        indexer.repair_zrc721_mints()?;
    }

    // Indexer runs alongside the HTTP server with automatic retry
    let indexer_handle = tokio::spawn(async move {
        let mut retry_delay = std::time::Duration::from_secs(5);
        let max_retry_delay = std::time::Duration::from_secs(300); // 5 minutes max

        loop {
            match indexer.start().await {
                Ok(_) => {
                    tracing::warn!("Indexer exited normally (unexpected)");
                    break;
                }
                Err(e) => {
                    tracing::error!("Indexer failed: {} - retrying in {:?}", e, retry_delay);
                    tokio::time::sleep(retry_delay).await;

                    // Exponential backoff with max cap
                    retry_delay = std::cmp::min(retry_delay * 2, max_retry_delay);
                }
            }
        }
    });

    // Start the public API
    tracing::info!("Starting API on port {}", api_port);
    api::start_api(db, api_port).await;

    // Keep process alive even if API finishes unexpectedly
    let _ = indexer_handle.await;

    Ok(())
}
