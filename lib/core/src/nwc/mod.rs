use std::{
    collections::{BTreeSet, HashMap},
    str::FromStr as _,
    time::Duration,
};

use crate::{
    event::EventManager,
    model::{Config, Payment},
    persist::Persister,
    utils,
};
use anyhow::Result;
use handler::{BreezRelayMessageHandler, RelayMessageHandler};
use log::{info, warn};
use maybe_sync::{MaybeSend, MaybeSync};
use nostr_sdk::{
    nips::nip44::{decrypt, encrypt, Version},
    nips::nip47::{
        ErrorCode, Method, NIP47Error, NostrWalletConnectURI, Notification, NotificationResult,
        NotificationType, PaymentNotification, Request, RequestParams, Response, ResponseResult,
        TransactionType,
    },
    Client as NostrClient, EventBuilder, Filter, Keys, Kind, RelayPoolNotification, RelayUrl,
    SubscriptionId, Tag, Timestamp,
};
use sdk_common::utils::Arc;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio_with_wasm::alias as tokio;

use crate::model::{NwcEvent, SdkEvent};

pub(crate) mod handler;
mod persist;

#[sdk_macros::async_trait]
pub trait NWCService: MaybeSend + MaybeSync {
    /// Creates a Nostr Wallet Connect connection string for this service.
    ///
    /// Generates a unique connection URI that external applications can use
    /// to connect to this wallet service. The URI includes the wallet's public key,
    /// relay information, and a randomly generated secret for secure communication.
    ///
    /// # Arguments
    /// * `name` - The unique identifier for the connection string
    async fn new_connection_string(&self, name: String) -> Result<String>;

    /// Lists the active Nostr Wallet Connect connections for this service.
    async fn list_connection_strings(&self) -> Result<HashMap<String, String>>;

    /// Removes a Nostr Wallet Connect connection string
    ///
    /// Removes a previously set connection string. Returns error if unset.
    ///
    /// # Arguments
    /// * `name` - The unique identifier for the connection string
    async fn remove_connection_string(&self, name: String) -> Result<()>;

    /// Starts the NWC service event processing loop.
    ///
    /// Establishes connections to Nostr relays and begins listening for incoming
    /// wallet operation requests. The service will:
    /// 1. Connect to configured relays
    /// 2. Broadcast service capability information
    /// 3. Listen for and process incoming requests
    /// 4. Send appropriate responses back through the relays
    ///
    /// The service runs until a shutdown signal is received.
    ///
    /// # Arguments
    /// * `shutdown_receiver` - Channel for receiving shutdown signals
    fn start(&self, shutdown_receiver: watch::Receiver<()>) -> JoinHandle<()>;

    /// Stops the NWC service and performs cleanup.
    ///
    /// Gracefully shuts down the service by:
    /// 1. Disconnecting from all Nostr relays
    /// 2. Aborting the background event processing task
    /// 3. Releasing any held resources
    async fn stop(self);
}

pub struct BreezNWCService<Handler: RelayMessageHandler> {
    keys: Keys,
    config: Config,
    handler: Arc<Handler>,
    event_manager: Arc<EventManager>,
    client: std::sync::Arc<NostrClient>,
    persister: std::sync::Arc<Persister>,
    subscriptions: Mutex<HashMap<String, SubscriptionId>>,
}

impl<Handler: RelayMessageHandler> BreezNWCService<Handler> {
    /// Creates a new BreezNWCService instance.
    ///
    /// Initializes the service with the provided cryptographic keys, handler,
    /// and connects to the specified Nostr relays.
    ///
    /// # Arguments
    /// * `handler` - Handler for processing relay messages
    /// * `relays` - List of relay URLs to connect to
    ///
    /// # Returns
    /// * `Ok(BreezNWCService)` - Successfully initialized service
    /// * `Err(anyhow::Error)` - Error adding relays or initializing
    pub(crate) async fn new(
        handler: Arc<Handler>,
        config: Config,
        persister: std::sync::Arc<Persister>,
        event_manager: Arc<EventManager>,
    ) -> Result<Self> {
        let client = std::sync::Arc::new(NostrClient::default());
        let relays = config.nwc_relays();
        for relay in &relays {
            client.add_relay(relay).await?;
        }

        let secret_key = Self::get_or_create_secret_key(&config, &persister)?;
        let keys = Keys::parse(&secret_key)?;
        Ok(Self {
            client,
            config,
            handler,
            keys,
            persister,
            event_manager,
            subscriptions: Default::default(),
        })
    }

    fn get_or_create_secret_key(config: &Config, p: &Persister) -> Result<String> {
        // If we have a key from the configuration, use it
        if let Some(key) = config
            .nwc_options
            .as_ref()
            .and_then(|opts| opts.secret_key.clone())
        {
            return Ok(key);
        }

        // Otherwise, try restoring it from the previous session
        if let Ok(Some(key)) = p.get_nwc_seckey() {
            return Ok(key);
        }

        // If none exists, generate a new one
        let key = nostr_sdk::key::SecretKey::generate().to_secret_hex();
        p.set_nwc_seckey(key.clone())?;
        Ok(key)
    }

    fn list_clients(p: &Persister) -> Result<HashMap<String, NostrWalletConnectURI>> {
        Ok(p.list_nwc_uris()?
            .into_iter()
            .filter_map(|(name, uri)| {
                NostrWalletConnectURI::from_str(&uri)
                    .map(|uri| (name, uri))
                    .ok()
            })
            .collect())
    }

    async fn subscribe(
        c: &NostrClient,
        s: &mut HashMap<String, SubscriptionId>,
        name: String,
        uri: &NostrWalletConnectURI,
    ) -> Result<()> {
        let sub_id = c
            .subscribe(
                Filter {
                    authors: Some(BTreeSet::from([uri.public_key])),
                    kinds: Some(BTreeSet::from([Kind::WalletConnectRequest])),
                    ..Default::default()
                },
                None,
            )
            .await?;
        s.insert(name, sub_id.val);
        Ok(())
    }
}

impl BreezNWCService<BreezRelayMessageHandler> {
    async fn send_event(
        eb: EventBuilder,
        keys: &Keys,
        client: &NostrClient,
    ) -> Result<(), nostr_sdk::client::Error> {
        let evt = eb.sign_with_keys(keys)?;
        client.send_event(&evt).await?;
        Ok(())
    }

    async fn handle_event(
        n: &RelayPoolNotification,
        clients: &HashMap<String, NostrWalletConnectURI>,
        client: &NostrClient,
        our_keys: &Keys,
        handler: &BreezRelayMessageHandler,
        event_manager: &EventManager,
    ) {
        let RelayPoolNotification::Event { event, .. } = n else {
            return;
        };

        // Verify event pubkey matches expected pubkey
        let Some((_, client_uri)) = clients
            .iter()
            .find(|(_, uri)| uri.public_key == event.pubkey)
        else {
            warn!("Got unrecognized event pubkey {}", event.pubkey);
            return;
        };
        info!("Received NWC notification: {event:?}");

        // Verify the event signature and event id
        if let Err(e) = event.verify() {
            warn!("Event signature verification failed: {e:?}");
            return;
        }

        // Decrypt the event content
        let decrypted_content =
            match decrypt(&client_uri.secret, &our_keys.public_key(), &event.content) {
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
            RequestParams::ListTransactions(req) => match handler.list_transactions(req).await {
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

        let _ = Self::handle_local_notification(event_manager, &result, &error).await;

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
            our_keys.secret_key(),
            &client_uri.public_key,
            &content,
            Version::V2,
        ) {
            Ok(encrypted) => encrypted,
            Err(e) => {
                warn!("Could not encrypt response content: {e:?}");
                return;
            }
        };

        let eb = EventBuilder::new(Kind::WalletConnectResponse, encrypted_content)
            .tags([Tag::event(event.id), Tag::public_key(client_uri.public_key)]);
        if let Err(e) = Self::send_event(eb, our_keys, client).await {
            warn!("Could not send response event to relay pool: {e:?}");
        }
        info!("sent encrypted NWC response");
    }

    async fn handle_local_notification(
        event_manager: &EventManager,
        result: &Option<ResponseResult>,
        error: &Option<NIP47Error>,
    ) -> Result<()> {
        info!("Handling notification: {result:?} {error:?}");
        let event: SdkEvent = match (result, error) {
            (Some(ResponseResult::PayInvoice(response)), None) => SdkEvent::NWC {
                details: NwcEvent::PayInvoice {
                    success: true,
                    preimage: Some(response.preimage.clone()),
                    fees_sat: response.fees_paid.map(|f| f / 1000),
                    error: None,
                },
            },
            (None, Some(error)) => match error.code {
                ErrorCode::PaymentFailed => SdkEvent::NWC {
                    details: NwcEvent::PayInvoice {
                        success: false,
                        preimage: None,
                        fees_sat: None,
                        error: Some(error.message.clone()),
                    },
                },
                _ => {
                    warn!("Unhandled error code: {:?}", error.code);
                    return Ok(());
                }
            },
            (Some(ResponseResult::ListTransactions(_)), None) => SdkEvent::NWC {
                details: NwcEvent::ListTransactions,
            },
            (Some(ResponseResult::GetBalance(_)), None) => SdkEvent::NWC {
                details: NwcEvent::GetBalance,
            },
            _ => {
                warn!("Unexpected combination");
                return Ok(());
            }
        };
        info!("Sending event: {event:?}");
        event_manager.notify(event).await;
        Ok(())
    }

    async fn forward_payment_to_relays(
        p: &Payment,
        client: &NostrClient,
        our_keys: &Keys,
        clients: &HashMap<String, NostrWalletConnectURI>,
    ) {
        let (invoice, description, preimage, payment_hash) = match &p.details {
            crate::model::PaymentDetails::Lightning {
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
            transaction_type: Some(if p.payment_type == crate::model::PaymentType::Send {
                TransactionType::Outgoing
            } else {
                TransactionType::Incoming
            }),
            invoice,
            description: Some(description),
            description_hash: None,
            preimage,
            payment_hash,
            amount: p.amount_sat * 1000,
            fees_paid: p.fees_sat * 1000,
            created_at: Timestamp::from_secs(p.timestamp as u64),
            expires_at: None,
            settled_at: Timestamp::from_secs(p.timestamp as u64),
            metadata: None,
        };

        let notification = if p.payment_type == crate::model::PaymentType::Send {
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
            let encrypted_content = match encrypt(
                our_keys.secret_key(),
                &uri.public_key,
                &notification_content,
                Version::V2,
            ) {
                Ok(encrypted) => encrypted,
                Err(e) => {
                    warn!("Could not encrypt notification content: {e:?}");
                    continue;
                }
            };

            let eb = EventBuilder::new(Kind::Custom(23196), encrypted_content)
                .tags([Tag::public_key(uri.public_key)]);

            if let Err(e) = Self::send_event(eb, our_keys, client).await {
                warn!("Could not send notification event to relay: {e:?}");
            } else {
                info!("Sent payment notification to relay");
            }
        }
    }
}

#[sdk_macros::async_trait]
impl NWCService for BreezNWCService<BreezRelayMessageHandler> {
    async fn new_connection_string(&self, name: String) -> Result<String> {
        let random_secret_key = nostr_sdk::SecretKey::generate();
        let relays = self
            .config
            .nwc_relays()
            .into_iter()
            .filter_map(|r| RelayUrl::from_str(&r).ok())
            .collect();
        let uri = NostrWalletConnectURI::new(self.keys.public_key, relays, random_secret_key, None);
        self.persister.set_nwc_uri(name.clone(), uri.to_string())?;

        let mut subs = self.subscriptions.lock().await;
        Self::subscribe(&self.client, &mut subs, name, &uri).await?;
        Ok(uri.to_string())
    }

    async fn list_connection_strings(&self) -> Result<HashMap<String, String>> {
        self.persister.list_nwc_uris()
    }

    async fn remove_connection_string(&self, name: String) -> Result<()> {
        if let Some(sub_id) = self.subscriptions.lock().await.remove(&name) {
            self.client.unsubscribe(&sub_id).await;
        }
        self.persister.remove_nwc_uri(name)?;
        Ok(())
    }

    fn start(&self, shutdown: watch::Receiver<()>) -> JoinHandle<()> {
        let client = self.client.clone();
        let handler = self.handler.clone();
        let event_manager = self.event_manager.clone();
        let our_keys = self.keys.clone();
        let persister = self.persister.clone();
        let mut sdk_event_listener = self.event_manager.subscribe();

        let nwc_service_future = async move {
            client.connect().await;

            info!("Successfully connected NWC client");

            // Broadcast info event
            let mut content: String = [
                Method::PayInvoice,
                Method::ListTransactions,
                Method::GetBalance,
            ]
            .map(|m| m.to_string())
            .join(" ");
            content.push_str("notifications");

            if let Err(err) = Self::send_event(
                EventBuilder::new(Kind::WalletConnectInfo, content),
                &our_keys,
                &client,
            )
            .await
            {
                warn!("Could not send info event to relay pool: {err:?}");
            }

            // Load the clients from the database and susbcribe to each pubkey
            let clients = match Self::list_clients(&persister) {
                Ok(clients) => clients,
                Err(err) => {
                    warn!("Could not load active NWC clients: {err:?}");
                    return;
                }
            };

            let mut notifications_listener = client.notifications();
            loop {
                tokio::select! {
                    Ok(SdkEvent::PaymentSucceeded { details: payment }) = sdk_event_listener.recv() => Self::forward_payment_to_relays(&payment, &client, &our_keys, &clients).await,
                    Ok(notification) = notifications_listener.recv() => Self::handle_event(&notification, &clients, &client, &our_keys, &handler, &event_manager).await,
                }
            }
        };

        let client = self.client.clone();
        tokio::task::spawn(async move {
            utils::run_with_shutdown_and_cleanup(
                shutdown,
                "Received shutdown signal, exiting NWC service loop",
                nwc_service_future,
                || async move {
                    match tokio::time::timeout(Duration::from_secs(2), client.disconnect()).await {
                        Ok(_) => {
                            info!("Successfully disconnected NWC client");
                        }
                        Err(err) => {
                            warn!("Could not disconnect NWC client within timeout: {err:?}");
                        }
                    }
                },
            )
            .await
        })
    }

    async fn stop(self) {
        self.client.disconnect().await;
    }
}
