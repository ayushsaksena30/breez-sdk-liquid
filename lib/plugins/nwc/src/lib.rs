pub mod config;
pub mod handler;
mod persist;
mod relay;
mod nwc;

pub use crate::handler::*;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    str::FromStr as _,
};

use breez_sdk_liquid::{
    sdk::LiquidSdk,
    event::EventManager,
    model::Payment,
    persist::Persister,
    plugin::{Plugin, PluginStorage},
};
use anyhow::Result;
use config::NwcConfig;
use handler::RelayMessageHandler;
use log::{info, warn, debug};
use maybe_sync::{MaybeSend, MaybeSync};
use nostr_sdk::{
    nips::nip44::{decrypt, encrypt, Version},
    nips::nip47::{
        ErrorCode, Method, NIP47Error, NostrWalletConnectURI, Notification, NotificationResult,
        NotificationType, PaymentNotification, Request, RequestParams, Response, ResponseResult,
        TransactionType,
    },
    Alphabet, Client as NostrClient, EventBuilder, Filter, Keys, Kind, RelayPoolNotification,
    RelayUrl, SingleLetterTag, Tag, Timestamp,
};
use sdk_common::utils::Arc;
use tokio::sync::{mpsc, watch, Mutex, OnceCell};
use tokio_with_wasm::alias as tokio;

use breez_sdk_liquid::model::{NwcEvent, SdkEvent};

#[sdk_macros::async_trait]
pub trait NwcService: MaybeSend + MaybeSync {
    /// Creates a Nostr Wallet Connect connection string for this service.
    ///
    /// Generates a unique connection URI that external applications can use
    /// to connect to this wallet service. The URI includes the wallet's public key,
    /// relay information, and a randomly generated secret for secure communication.
    ///
    /// # Arguments
    /// * `name` - The unique identifier for the connection string
    async fn add_connection_string(&self, name: String) -> Result<String>;

    /// Lists the active Nostr Wallet Connect connections for this service.
    async fn list_connection_strings(&self) -> Result<HashMap<String, String>>;

    /// Removes a Nostr Wallet Connect connection string
    ///
    /// Removes a previously set connection string. Returns error if unset.
    ///
    /// # Arguments
    /// * `name` - The unique identifier for the connection string
    async fn remove_connection_string(&self, name: String) -> Result<()>;
}

pub struct SdkNwcService {
    keys: Keys,
    config: NwcConfig,
    client: NostrClient,
    event_manager: Arc<EventManager>,
    persister: std::sync::Arc<Persister>,
    resubscription_trigger: Mutex<Option<mpsc::Sender<()>>>,
    task_handle: OnceCell<tokio::task::JoinHandle<()>>,
}

impl SdkNwcService {
    /// Creates a new SdkNwcService instance.
    ///
    /// Initializes the service with the provided cryptographic keys
    /// and connects to the specified Nostr relays.
    ///
    /// # Arguments
    /// * `config` - NWC configuration containing relay URLs and secret key
    /// * `persister` - Persister for storing NWC data
    /// * `event_manager` - Event manager for notifications
    ///
    /// # Returns
    /// * `Arc<SdkNwcService>` - Successfully initialized service
    /// * `Err(anyhow::Error)` - Error adding relays or initializing
    pub(crate) async fn new(
        config: NwcConfig,
        persister: std::sync::Arc<Persister>,
        event_manager: Arc<EventManager>,
    ) -> Result<Arc<Self>> {
        let client = NostrClient::default();
        let relays = config.relays();
        for relay in &relays {
            client.add_relay(relay).await?;
        }

        let secret_key = Self::get_or_create_secret_key(&config, &persister)?;
        let keys = Keys::parse(&secret_key)?;
        Ok(Arc::new(Self {
            client,
            config,
            keys,
            persister,
            event_manager,
            resubscription_trigger: Default::default(),
            task_handle: OnceCell::new(),
        }))
    }

    fn get_or_create_secret_key(config: &NwcConfig, persister: &Persister) -> Result<String> {
        // If we have a key from the configuration, use it
        if let Some(key) = config.secret_key.clone() {
            return Ok(key);
        }

        // Otherwise, try restoring it from the previous session
        if let Ok(Some(key)) = persister.get_nwc_seckey() {
            return Ok(key);
        }

        // If none exists, generate a new one
        let key = nostr_sdk::key::SecretKey::generate().to_secret_hex();
        persister.set_nwc_seckey(key.clone())?;
        Ok(key)
    }

    fn list_clients(&self) -> Result<HashMap<String, NostrWalletConnectURI>> {
        Ok(self
            .persister
            .list_nwc_uris()?
            .into_iter()
            .filter_map(|(name, uri)| {
                NostrWalletConnectURI::from_str(&uri)
                    .map(|uri| (name, uri))
                    .ok()
            })
            .collect())
    }

    async fn resubscribe(&self, clients: &HashMap<String, NostrWalletConnectURI>) -> Result<()> {
        let pubkeys = clients
            .values()
            .map(|uri| uri.public_key.to_string())
            .collect();
        self.client
            .subscribe(
                Filter {
                    generic_tags: BTreeMap::from([(
                        SingleLetterTag {
                            character: Alphabet::P,
                            uppercase: false,
                        },
                        pubkeys,
                    )]),
                    kinds: Some(BTreeSet::from([Kind::WalletConnectRequest])),
                    ..Default::default()
                },
                None,
            )
            .await?;
        info!("Successfully subscribed to events");
        Ok(())
    }

    async fn send_event(&self, event_builder: EventBuilder) -> Result<(), nostr_sdk::client::Error> {
        let event = event_builder.sign_with_keys(&self.keys)?;
        self.client.send_event(&event).await?;
        Ok(())
    }

    async fn handle_event(&self, notification: &RelayPoolNotification, handler: &dyn RelayMessageHandler) {
        let RelayPoolNotification::Event { event, .. } = notification else {
            return;
        };
        info!("Received NWC event: {event:?}");

        let client_pubkey = event.pubkey;

        // Verify the event has not expired
        if event
            .tags
            .expiration()
            .is_some_and(|t| *t > Timestamp::now())
        {
            warn!("Event {} has expired. Skipping.", event.id);
            return;
        }

        // Verify the event signature and event id
        if let Err(e) = event.verify() {
            warn!("Event signature verification failed: {e:?}");
            return;
        }

        // Decrypt the event content
        let decrypted_content =
            match decrypt(self.keys.secret_key(), &client_pubkey, &event.content) {
                Ok(content) => content,
                Err(e) => {
                    warn!("Failed to decrypt event content: {e:?}");
                    return;
                }
            };

        info!("Decrypted NWC notification: {decrypted_content}");

        let req = match serde_json::from_str::<Request>(&decrypted_content) {
            Ok(r) => r,
            Err(e) => {
                warn!("Received unexpected request from relay pool: {decrypted_content} err {e:?}");
                return;
            }
        };

        let (result, error) = match req.params {
            RequestParams::PayInvoice(req) => match handler.pay_invoice(req).await {
                Ok(res) => (Some(ResponseResult::PayInvoice(res)), None),
                Err(e) => (None, Some(e)),
            },
            RequestParams::ListTransactions(req) => match handler.list_transactions(req).await
            {
                Ok(res) => (Some(ResponseResult::ListTransactions(res)), None),
                Err(e) => (None, Some(e)),
            },
            RequestParams::GetBalance => match handler.get_balance().await {
                Ok(res) => (Some(ResponseResult::GetBalance(res)), None),
                Err(e) => (None, Some(e)),
            },
            _ => {
                info!("Received unhandled request: {req:?}");
                return;
            }
        };

        self.handle_local_notification(&result, &error, &event.id.to_string())
            .await;

        let content = match serde_json::to_string(&Response {
            result_type: req.method,
            result,
            error,
        }) {
            Ok(c) => c,
            Err(e) => {
                warn!("Could not serialize Nostr response: {e:?}");
                return;
            }
        };
        info!("NWC Response content: {content}");
        info!("encrypting NWC response");
        let encrypted_content = match encrypt(
            self.keys.secret_key(),
            &client_pubkey,
            &content,
            Version::V2,
        ) {
            Ok(encrypted) => encrypted,
            Err(e) => {
                warn!("Could not encrypt response content: {e:?}");
                return;
            }
        };

        let event_builder = EventBuilder::new(Kind::WalletConnectResponse, encrypted_content)
            .tags([Tag::event(event.id), Tag::public_key(client_pubkey)]);
        if let Err(e) = self.send_event(event_builder).await {
            warn!("Could not send response event to relay pool: {e:?}");
        }
        info!("sent encrypted NWC response");
    }

    async fn handle_local_notification(
        &self,
        result: &Option<ResponseResult>,
        error: &Option<NIP47Error>,
        event_id: &str,
    ) {
        debug!("Handling notification: {result:?} {error:?}");
        let event: SdkEvent = match (result, error) {
            (Some(ResponseResult::PayInvoice(response)), None) => SdkEvent::NWC {
                details: NwcEvent::PayInvoiceHandled {
                    success: true,
                    preimage: Some(response.preimage.clone()),
                    fees_sat: response.fees_paid.map(|f| f / 1000),
                    error: None,
                },
                event_id: event_id.to_string(),
            },
            (None, Some(error)) => match error.code {
                ErrorCode::PaymentFailed => SdkEvent::NWC {
                    details: NwcEvent::PayInvoiceHandled {
                        success: false,
                        preimage: None,
                        fees_sat: None,
                        error: Some(error.message.clone()),
                    },
                    event_id: event_id.to_string(),
                },
                _ => {
                    warn!("Unhandled error code: {:?}", error.code);
                    return;
                }
            },
            (Some(ResponseResult::ListTransactions(_)), None) => SdkEvent::NWC {
                details: NwcEvent::ListTransactionsHandled,
                event_id: event_id.to_string(),
            },
            (Some(ResponseResult::GetBalance(_)), None) => SdkEvent::NWC {
                details: NwcEvent::GetBalanceHandled,
                event_id: event_id.to_string(),
            },
            _ => {
                warn!("Unexpected combination");
                return;
            }
        };
        info!("Sending event: {event:?}");
        self.event_manager.notify(event).await;
        
    }

    async fn forward_payment_to_clients(
        &self,
        payment: &Payment,
        clients: &HashMap<String, NostrWalletConnectURI>,
    ) {
        let (invoice, description, preimage, payment_hash) = match &payment.details {
            breez_sdk_liquid::model::PaymentDetails::Lightning {
                invoice,
                description,
                preimage,
                payment_hash,
                ..
            } => (
                invoice.clone().unwrap_or_default(),
                description.clone(),
                preimage.clone().unwrap_or_default(),
                payment_hash.clone().unwrap_or_default(),
            ),
            _ => {
                return;
            }
        };

        let payment_notification = PaymentNotification {
            transaction_type: Some(if payment.payment_type == crate::model::PaymentType::Send {
                TransactionType::Outgoing
            } else {
                TransactionType::Incoming
            }),
            invoice,
            description: Some(description),
            description_hash: None,
            preimage,
            payment_hash,
            amount: payment.amount_sat * 1000,
            fees_paid: payment.fees_sat * 1000,
            created_at: Timestamp::from_secs(payment.timestamp as u64),
            expires_at: None,
            settled_at: Timestamp::from_secs(payment.timestamp as u64),
            metadata: None,
        };

        let notification = if payment.payment_type == breez_sdk_liquid::model::PaymentType::Send {
            Notification {
                notification_type: NotificationType::PaymentSent,
                notification: NotificationResult::PaymentSent(payment_notification),
            }
        } else {
            Notification {
                notification_type: NotificationType::PaymentReceived,
                notification: NotificationResult::PaymentReceived(payment_notification),
            }
        };

        let notification_content = match serde_json::to_string(&notification) {
            Ok(content) => content,
            Err(e) => {
                warn!("Could not serialize notification: {e:?}");
                return;
            }
        };

        for uri in clients.values() {
            let nwc_client_keypair = Keys::new(uri.secret.clone());
            let encrypted_content = match encrypt(
                self.keys.secret_key(),
                &nwc_client_keypair.public_key,
                &notification_content,
                Version::V2,
            ) {
                Ok(encrypted) => encrypted,
                Err(e) => {
                    warn!("Could not encrypt notification content: {e:?}");
                    continue;
                }
            };

            let event_builder = EventBuilder::new(Kind::Custom(23196), encrypted_content)
                .tags([Tag::public_key(uri.public_key)]);

            if let Err(e) = self.send_event(event_builder).await {
                warn!("Could not send notification event to relay: {e:?}");
            } else {
                info!("Sent payment notification to relay");
            }
        }
    }

    async fn trigger_resubscription(&self) {
        if let Some(ref trigger) = *self.resubscription_trigger.lock().await {
            let _ = trigger.send(()).await;
        }
    }

}

#[sdk_macros::async_trait]
impl NwcService for SdkNwcService {
    async fn add_connection_string(&self, name: String) -> Result<String> {
        let random_secret_key = nostr_sdk::SecretKey::generate();
        let relays = self
            .config
            .relays()
            .into_iter()
            .filter_map(|r| RelayUrl::from_str(&r).ok())
            .collect();
        let uri = NostrWalletConnectURI::new(self.keys.public_key, relays, random_secret_key, None);
        self.persister.set_nwc_uri(name.clone(), uri.to_string())?;
        self.trigger_resubscription().await;
        Ok(uri.to_string())
    }

    async fn list_connection_strings(&self) -> Result<HashMap<String, String>> {
        self.persister.list_nwc_uris()
    }

    async fn remove_connection_string(&self, name: String) -> Result<()> {
        self.persister.remove_nwc_uri(name)?;
        self.trigger_resubscription().await;
        Ok(())
    }
}

impl Plugin for SdkNwcService {
    fn id(&self) -> String {
        "nwc".to_string()
    }

    async fn on_start(&self, sdk: Arc<LiquidSdk>, storage: PluginStorage) {
        let s = Arc::new(self.clone());
        let s_cleanup = s.clone();
        let mut sdk_event_listener = self.event_manager.subscribe();

        let nwc_service_future = async move {
            s.client.connect().await;

            info!("Successfully connected NWC client");

            let _ = s.event_manager.notify(SdkEvent::NWC {
                details: NwcEvent::ConnectedHandled,
                event_id: "service_start".to_string(),
            }).await;

            // Broadcast info event
            let mut content: String = [
                Method::PayInvoice,
                Method::ListTransactions,
                Method::GetBalance,
            ]
            .map(|m| m.to_string())
            .join(" ");
            content.push_str("notifications");

            if let Err(err) = s
                .send_event(
                    EventBuilder::new(Kind::WalletConnectInfo, content)
                        .tag(Tag::custom("encryption".into(), ["nip44_v2".to_string()])),
                )
                .await
            {
                warn!("Could not send info event to relay pool: {err:?}");
            }

            let (resub_tx, mut resub_rx) = mpsc::channel::<()>(10);
            *s.resubscription_trigger.lock().await = Some(resub_tx);
            loop {
                let clients = match s.list_clients() {
                    Ok(clients) => clients,
                    Err(err) => {
                        warn!("Could not retreive active clients from database: {err:?}");
                        return;
                    }
                };
                if let Err(err) = s.resubscribe(&clients).await {
                    warn!("Could not resubscribe to events: {err:?}");
                    return;
                };
                let mut notifications_listener = s.client.notifications();
                loop {
                    tokio::select! {
                        Ok(SdkEvent::PaymentSucceeded { details: payment }) = sdk_event_listener.recv() => s.forward_payment_to_clients(&payment, &clients).await,
                        Ok(notification) = notifications_listener.recv() => {
                            let handler = handler::SdkRelayMessageHandler::new(sdk.clone());
                            s.handle_event(&notification, &handler).await;
                        },
                        Some(_) = resub_rx.recv() => {
                            info!("Resubscribing to notifications.");
                            break;
                        }
                    }
                }
            }
        };

        let handle = tokio::task::spawn(nwc_service_future);
        if self.task_handle.set(handle).is_err() {
            warn!("NWC service task_handle already set; not overriding");
        }
    }

    async fn on_stop(&self) {
        *self.resubscription_trigger.lock().await = None;
        if let Some(handle) = self.task_handle.get() {
            handle.abort();
        }
        let _ = self.event_manager.notify(SdkEvent::NWC {
            details: NwcEvent::DisconnectedHandled,
            event_id: "service_stop".to_string(),
        }).await;

        self.client.disconnect().await;
    }
}
