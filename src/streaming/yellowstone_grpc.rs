use std::{collections::HashMap, fmt, time::Duration};

use chrono::Local;
use futures::{channel::mpsc, sink::Sink, SinkExt, Stream, StreamExt};
use log::{error, info};
use rustls::crypto::{ring::default_provider, CryptoProvider};
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{EncodedTransactionWithStatusMeta, UiTransactionEncoding};
use tonic::{transport::channel::ClientTlsConfig, Status};
use yellowstone_grpc_client::{GeyserGrpcClient, Interceptor};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions, SubscribeRequestPing, SubscribeUpdate,
    SubscribeUpdateTransaction,
};
use yellowstone_grpc_proto::geyser::{SubscribeRequestFilterBlocksMeta, SubscribeUpdateBlockMeta};

use crate::common::AnyResult;
use crate::streaming::event_parser::core::traits::SDKSystemEventParser;
use crate::streaming::event_parser::{EventParserFactory, Protocol, UnifiedEvent};
use maplit::hashmap;

type TransactionsFilterMap = HashMap<String, SubscribeRequestFilterTransactions>;

const CONNECT_TIMEOUT: u64 = 10;
const REQUEST_TIMEOUT: u64 = 60;
const CHANNEL_SIZE: usize = 1000;
const MAX_DECODING_MESSAGE_SIZE: usize = 1024 * 1024 * 10;

#[derive(Clone)]
pub struct TransactionPretty {
    pub slot: u64,
    pub signature: Signature,
    pub is_vote: bool,
    pub tx: EncodedTransactionWithStatusMeta,
}

impl fmt::Debug for TransactionPretty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct TxWrap<'a>(&'a EncodedTransactionWithStatusMeta);
        impl<'a> fmt::Debug for TxWrap<'a> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let serialized = serde_json::to_string(self.0).expect("failed to serialize");
                fmt::Display::fmt(&serialized, f)
            }
        }

        f.debug_struct("TransactionPretty")
            .field("slot", &self.slot)
            .field("signature", &self.signature)
            .field("is_vote", &self.is_vote)
            .field("tx", &TxWrap(&self.tx))
            .finish()
    }
}

impl From<SubscribeUpdateTransaction> for TransactionPretty {
    fn from(SubscribeUpdateTransaction { transaction, slot }: SubscribeUpdateTransaction) -> Self {
        let tx = transaction.expect("should be defined");
        Self {
            slot,
            signature: Signature::try_from(tx.signature.as_slice()).expect("valid signature"),
            is_vote: tx.is_vote,
            tx: yellowstone_grpc_proto::convert_from::create_tx_with_meta(tx)
                .expect("valid tx with meta")
                .encode(UiTransactionEncoding::Base64, Some(u8::MAX), true)
                .expect("failed to encode"),
        }
    }
}

pub struct YellowstoneGrpc {
    endpoint: String,
    x_token: Option<String>,
}

impl YellowstoneGrpc {
    pub fn new(endpoint: String, x_token: Option<String>) -> AnyResult<Self> {
        if CryptoProvider::get_default().is_none() {
            default_provider()
                .install_default()
                .map_err(|e| anyhow::anyhow!("Failed to install crypto provider: {:?}", e))?;
        }

        Ok(Self { endpoint, x_token })
    }

    pub async fn connect(&self) -> AnyResult<GeyserGrpcClient<impl Interceptor>> {
        let builder = GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
            .x_token(self.x_token.clone())?
            .tls_config(ClientTlsConfig::new().with_native_roots())?
            .max_decoding_message_size(MAX_DECODING_MESSAGE_SIZE)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT));

        Ok(builder.connect().await?)
    }

    pub async fn subscribe_with_request(
        &self,
        transactions: TransactionsFilterMap,
        commitment: Option<CommitmentLevel>,
    ) -> AnyResult<(
        impl Sink<SubscribeRequest, Error = mpsc::SendError>,
        impl Stream<Item = Result<SubscribeUpdate, Status>>,
    )> {
        let subscribe_request = SubscribeRequest {
            transactions,
            blocks_meta: hashmap! {
                "".to_owned() => SubscribeRequestFilterBlocksMeta {
                }
            },
            commitment: if let Some(commitment) = commitment {
                Some(commitment as i32)
            } else {
                Some(CommitmentLevel::Processed.into())
            },
            ..Default::default()
        };

        let mut client = self.connect().await?;
        let (sink, stream) = client
            .subscribe_with_request(Some(subscribe_request))
            .await?;
        Ok((sink, stream))
    }

    pub fn get_subscribe_request_filter(
        &self,
        account_include: Vec<String>,
        account_exclude: Vec<String>,
        account_required: Vec<String>,
    ) -> TransactionsFilterMap {
        let mut transactions = HashMap::new();
        transactions.insert(
            "client".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include,
                account_exclude,
                account_required,
            },
        );
        transactions
    }

    pub async fn handle_stream_message(
        msg: SubscribeUpdate,
        tx: &mut mpsc::Sender<TransactionPretty>,
        block: Option<&mut mpsc::Sender<SubscribeUpdateBlockMeta>>,
        subscribe_tx: &mut (impl Sink<SubscribeRequest, Error = mpsc::SendError> + Unpin),
    ) -> AnyResult<()> {
        match msg.update_oneof {
            Some(UpdateOneof::BlockMeta(sut)) => {
                if let Some(block) = block {
                    block.try_send(sut)?;
                }
            }
            Some(UpdateOneof::Transaction(sut)) => {
                let transaction_pretty = TransactionPretty::from(sut.clone());
                tx.try_send(transaction_pretty)?;
            }
            Some(UpdateOneof::Ping(_)) => {
                subscribe_tx
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    })
                    .await?;
                info!("service is ping: {}", Local::now());
            }
            Some(UpdateOneof::Pong(_)) => {
                info!("service is pong: {}", Local::now());
            }
            _ => {}
        }
        Ok(())
    }

    /// 订阅事件
    pub async fn subscribe_events<F>(
        &self,
        protocols: Vec<Protocol>,
        bot_wallet: Option<Pubkey>,
        account_include: Option<Vec<String>>,
        account_exclude: Option<Vec<String>>,
        account_required: Option<Vec<String>>,
        commitment: Option<CommitmentLevel>,
        callback: F,
    ) -> AnyResult<()>
    where
        F: Fn(Box<dyn UnifiedEvent>) + Send + Sync + 'static,
    {
        // 创建过滤器
        let protocol_accounts = protocols
            .iter()
            .map(|p| p.get_program_id())
            .flatten()
            .map(|p| p.to_string())
            .collect::<Vec<String>>();
        let mut account_include = account_include.unwrap_or_default();
        let account_exclude = account_exclude.unwrap_or_default();
        let account_required = account_required.unwrap_or_default();

        account_include.extend(protocol_accounts.clone());

        let transactions =
            self.get_subscribe_request_filter(account_include, account_exclude, account_required);

        // 订阅事件
        let (mut subscribe_tx, mut stream) = self
            .subscribe_with_request(transactions, commitment)
            .await?;

        // 创建通道
        let (mut tx, mut rx) = mpsc::channel::<TransactionPretty>(CHANNEL_SIZE);
        let (mut block, mut rblock) = mpsc::channel::<SubscribeUpdateBlockMeta>(CHANNEL_SIZE);

        // 创建回调函数，使用 Arc 包装以便在多个任务中共享
        let callback = std::sync::Arc::new(Box::new(callback));

        // 启动处理流的任务
        tokio::spawn(async move {
            while let Some(message) = stream.next().await {
                match message {
                    Ok(msg) => {
                        if let Err(e) = Self::handle_stream_message(
                            msg,
                            &mut tx,
                            Some(&mut block),
                            &mut subscribe_tx,
                        )
                        .await
                        {
                            error!("Error handling message: {:?}", e);
                            break;
                        }
                    }
                    Err(error) => {
                        error!("Stream error: {error:?}");
                        break;
                    }
                }
            }
        });

        // 为交易处理和区块处理克隆 Arc<Box<F>>
        let callback_tx = callback.clone();
        let callback_block = callback;

        // 处理交易
        tokio::spawn(async move {
            while let Some(transaction_pretty) = rx.next().await {
                if let Err(e) = Self::process_event_transaction(
                    transaction_pretty,
                    &**callback_tx,
                    bot_wallet,
                    protocols.clone(),
                )
                .await
                {
                    error!("Error processing transaction: {:?}", e);
                }
            }
        });
        // 处理block
        tokio::spawn(async move {
            while let Some(block) = rblock.next().await {
                if let Err(e) = Self::process_block(block, &**callback_block).await {
                    error!("Error processing block: {:?}", e);
                }
            }
        });
        tokio::signal::ctrl_c().await?;
        Ok(())
    }

    /// 处理事件交易
    async fn process_event_transaction<F>(
        transaction_pretty: TransactionPretty,
        callback: &F,
        bot_wallet: Option<Pubkey>,
        protocols: Vec<Protocol>,
    ) -> AnyResult<()>
    where
        F: Fn(Box<dyn UnifiedEvent>) + Send + Sync,
    {
        let slot = transaction_pretty.slot;
        let signature = transaction_pretty.signature.to_string();
        for protocol in protocols {
            let parser = EventParserFactory::create_parser(protocol);
            let events = parser
                .parse_transaction(
                    transaction_pretty.tx.clone(),
                    &signature,
                    Some(slot),
                    bot_wallet.clone(),
                )
                .await
                .unwrap_or_else(|_e| vec![]);
            for event in events {
                callback(event);
            }
        }

        Ok(())
    }

    /// 处理区块
    async fn process_block<F>(block: SubscribeUpdateBlockMeta, callback: &F) -> AnyResult<()>
    where
        F: Fn(Box<dyn UnifiedEvent>) + Send + Sync,
    {
        let event = SDKSystemEventParser::parse_block(block);
        callback(event);
        Ok(())
    }
}
