use std::time::Duration;

use clap::Parser;
use futures::StreamExt;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::configs::{Opts, SubCommand};

mod configs;

// Categories for logging
const INDEXER: &str = "streamer_indexer";
const TOPIC_NAME: &str = "blocks";

fn main() {
    // We use it to automatically search the for root certificates to perform HTTPS calls
    // (sending telemetry and downloading genesis)
    openssl_probe::init_ssl_cert_env_vars();

    let mut env_filter = EnvFilter::new(
        "tokio_reactor=info,near=info,stats=info,telemetry=info,indexer=info,streamer_indexer=info,aggregated=info",
    );

    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        if !rust_log.is_empty() {
            for directive in rust_log.split(',').filter_map(|s| match s.parse() {
                Ok(directive) => Some(directive),
                Err(err) => {
                    eprintln!("Ignoring directive `{}`: {}", s, err);
                    None
                }
            }) {
                env_filter = env_filter.add_directive(directive);
            }
        }
    }

    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    let opts: Opts = Opts::parse();

    let home_dir = opts.home_dir.unwrap_or_else(near_indexer::get_default_home);

    match opts.subcmd {
        SubCommand::Run(args) => {
            tracing::info!(
                target: crate::INDEXER,
                "NEAR Indexer for Explorer v{} starting...",
                env!("CARGO_PKG_VERSION")
            );

            let system = actix::System::new();
            system.block_on(async move {
                let indexer_config = args.clone().to_indexer_config(home_dir);
                let indexer = near_indexer::Indexer::new(indexer_config);

                // Regular indexer process starts here
                let stream = indexer.streamer();

                listen_blocks(stream, args.concurrency, args.brokers).await;

                actix::System::current().stop();
            });
            system.run().unwrap();
        }
        SubCommand::Init(config) => near_indexer::init_configs(
            &home_dir,
            config.chain_id.as_ref().map(AsRef::as_ref),
            config.account_id.map(|account_id_string| {
                near_indexer::near_primitives::types::AccountId::try_from(account_id_string)
                    .expect("Received accound_id is not valid")
            }),
            config.test_seed.as_ref().map(AsRef::as_ref),
            config.num_shards,
            config.fast,
            config.genesis.as_ref().map(AsRef::as_ref),
            config.download_genesis,
            config.download_genesis_url.as_ref().map(AsRef::as_ref),
            config.download_config,
            config.download_config_url.as_ref().map(AsRef::as_ref),
            config.boot_nodes.as_ref().map(AsRef::as_ref),
            config.max_gas_burnt_view,
        ),
    }
}

async fn listen_blocks(
    stream: tokio::sync::mpsc::Receiver<near_indexer::StreamerMessage>,
    concurrency: std::num::NonZeroU16,
    brokers: String,
) {
    let producer: &FutureProducer = &ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Producer creation error");

    let mut handle_messages = tokio_stream::wrappers::ReceiverStream::new(stream)
        .map(|streamer_message| {
            info!(
                target: crate::INDEXER,
                "Block height {}", &streamer_message.block.header.height
            );
            handle_message(streamer_message, &producer)
        })
        .buffer_unordered(usize::from(concurrency.get()));

    while let Some(_handle_message) = handle_messages.next().await {}
}

async fn handle_message(
    streamer_message: near_indexer::StreamerMessage,
    producer: &FutureProducer,
) {
    let block_height = streamer_message.block.header.height;
    let streamer_message_json = serde_json::to_value(streamer_message).unwrap();

    producer
        .send(
            FutureRecord::to(TOPIC_NAME)
                .payload(&format!("{}", streamer_message_json))
                .key(&format!("{}", block_height)),
            Duration::from_secs(0),
        )
        .await
        .unwrap();
}
