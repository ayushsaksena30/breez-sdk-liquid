use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
use url::Url;
use tokio::sync::mpsc::{self, Sender, Receiver};
use std::sync::Arc;
use tokio::sync::Mutex;
use log;
use nostr::nips::nip47::{
    Error, ErrorCode, Request,
    PayInvoiceRequest,
    PayInvoiceResponse,
    ListTransactionsRequest,
    ListTransactionsResponse,
    GetBalanceResponse,
};

pub struct BreezRelayService {
  relay_url: String,
  pubkey: String,
  secret_key: String,
  shutdown: ShutdownChannel,
}

struct ShutdownChannel {
  pub sender: Sender<()>,
  pub receiver: Mutex<Receiver<()>>,
}

impl ShutdownChannel {
  pub fn new((sender, receiver): (Sender<()>, Receiver<()>)) -> Self {
    Self {
      sender,
      receiver: Mutex::new(receiver),
    }
  }
}

impl RelayService for BreezRelayService {
  fn new(
    relay_url: String, 
    pubkey: String, 
    secret_key: String,
  ) -> Self {
    Self {
      relay_url,
      pubkey,
      secret_key,
      shutdown: ShutdownChannel::new(tokio::sync::mpsc::channel::<()>(10)),
    }
  }

  async fn connect_to_relay(self: Arc<Self>) { //TODO: is this right?
    let cloned = self.clone();
    tokio::spawn(async move {
      let mut shutdown = cloned.shutdown.receiver.lock().await;
      tokio::select! {
        _ = cloned.inner_connect_to_relay() => {
          log::info!("WebSocket connection completed");
        }
        _ = shutdown.recv() => {
          log::info!("Received shutdown signal");
        }
      }
    });
  }

  async fn inner_connect_to_relay(&self) {
    let url = match Url::parse(&self.relay_url) {
      Ok(url) => url,
      Err(e) => {
        log::error!("Failed to parse URL: {e:?}");
        return;
      }
    };
    
    // Connect to the WebSocket server
    let (ws_stream, response) = match connect_async(url).await {
      Ok(stream) => stream,
      Err(e) => {
        log::error!("Failed to connect to relay: {e:?}");
        return;
      }
    };
    
    if !response.status().is_success() {
      log::error!("Failed to connect to relay: {}", response.status());
      return;
    }
    
    log::info!("Successfully connected to relay");
    
    // Split the WebSocket stream into sender and receiver
    let (mut write, mut read) = ws_stream.split();

    match write.send(Message::Ping(vec![])).await {
      Ok(_) => log::debug!("Sent initial ping"),
      Err(e) => {
        log::error!("Failed to send initial ping: {e:?}");
        return;
      }
    }

    // Handle incoming messages
    while let Some(message) = read.next().await {
      match message {
        Ok(Message::Text(text)) => {
          log::debug!("Received text message: {}", text);
          match self.recv_msg(text).await {
            Ok(_) => log::debug!("Processed message successfully"),
            Err(e) => log::warn!("Failed to process message: {e:?}"),
          }
        }
        Ok(Message::Ping(data)) => {
          match write.send(Message::Pong(data)).await {
            Ok(_) => log::debug!("Responded to ping with pong"),
            Err(e) => {
              log::error!("Failed to send pong: {e:?}");
              return;
            }
          }
        }
        Ok(Message::Pong(_)) => {
          log::debug!("Received pong, connection is alive");
        }
        Ok(Message::Close(frame)) => {
          if let Some(reason) = frame.reason {
            log::info!("Connection closed: {}", reason);
          } else {
            log::info!("Connection closed without reason");
          }
          let _ = write.send(Message::Close(None)).await;
          break;
        }
        Err(e) => {
          log::error!("WebSocket error: {e:?}");
          break;
        }
        _ => {}
      }
    }

    log::info!("WebSocket connection closed");
  }

  async fn recv_msg(&self, message: String) {
    // Parse the incoming message into a nostr request
    let request: Request = match serde_json::from_str(&message) {
      Ok(req) => req,
      Err(e) => {
        log::error!("Failed to decode message: {e:?}");
        self.send_msg("error", None, Some(Error::new(ErrorCode::InvalidRequest, format!("Failed to decode message: {}", e)))).await;
        return;
      }
    };

    // Handle the request based on its method
    match request.method.as_str() {
      "pay_invoice" => {
        let params: PayInvoiceRequest = match request.params {
          Some(p) => match serde_json::from_value(p) {
            Ok(p) => p,
            Err(e) => {
              log::error!("Failed to parse pay_invoice params: {e:?}");
              self.send_msg("pay_invoice", None, Some(Error::new(ErrorCode::InvalidRequest, format!("Invalid params: {}", e)))).await;
              return;
            }
          },
          None => {
            log::error!("No params field in pay_invoice request");
            self.send_msg("pay_invoice", None, Some(Error::new(ErrorCode::InvalidRequest, "No params field in request"))).await;
            return;
          }
        };

        match self.handler.pay_invoice(serde_json::to_string(&params).unwrap()).await {
          Ok(result) => {
            if let Err(e) = self.send_msg("pay_invoice", Some(serde_json::to_value(result).unwrap()), None).await {
              log::error!("Failed to send pay_invoice response: {e:?}");
              self.send_msg("pay_invoice", None, Some(Error::new(ErrorCode::Internal, format!("Failed to send response: {}", e)))).await;
            }
          }
          Err(e) => {
            log::error!("Failed to handle pay_invoice: {e:?}");
            self.send_msg("pay_invoice", None, Some(e)).await;
          }
        }
      }

      "list_transactions" => {
        let params: ListTransactionsRequest = match request.params {
          Some(p) => match serde_json::from_value(p) {
            Ok(p) => p,
            Err(e) => {
              log::error!("Failed to parse list_transactions params: {e:?}");
              self.send_msg("list_transactions", None, Some(Error::new(ErrorCode::InvalidRequest, format!("Invalid params: {}", e)))).await;
              return;
            }
          },
          None => {
            log::error!("No params field in list_transactions request");
            self.send_msg("list_transactions", None, Some(Error::new(ErrorCode::InvalidRequest, "No params field in request"))).await;
            return;
          }
        };

        match self.handler.list_transactions(serde_json::to_string(&params).unwrap()).await {
          Ok(result) => {
            if let Err(e) = self.send_msg("list_transactions", Some(serde_json::to_value(result).unwrap()), None).await {
              log::error!("Failed to send list_transactions response: {e:?}");
              self.send_msg("list_transactions", None, Some(Error::new(ErrorCode::Internal, format!("Failed to send response: {}", e)))).await;
            }
          }
          Err(e) => {
            log::error!("Failed to handle list_transactions: {e:?}");
            self.send_msg("list_transactions", None, Some(e)).await;
          }
        }
      }
      
      "get_balance" => {
        match self.handler.get_balance().await {
          Ok(result) => {
            if let Err(e) = self.send_msg("get_balance", Some(serde_json::to_value(result).unwrap()), None).await {
              log::error!("Failed to send get_balance response: {e:?}");
              self.send_msg("get_balance", None, Some(Error::new(ErrorCode::Internal, format!("Failed to send response: {}", e)))).await;
            }
          }
          Err(e) => {
            log::error!("Failed to handle get_balance: {e:?}");
            self.send_msg("get_balance", None, Some(e)).await;
          }
        }
      }
      _ => {
        log::debug!("Unhandled method: {}", request.method);
        self.send_msg("error", None, Some(Error::new(ErrorCode::InvalidRequest, format!("Unhandled method: {}", request.method)))).await;
      }
    }
  }

  async fn send_msg(&self, method: &str, result: Option<serde_json::Value>, error: Option<Error>) {
    let message = match (result, error) {
      (Some(result), None) => serde_json::json!({
        "result_type": method,
        "result": result
      }),
      (None, Some(error)) => serde_json::json!({
        "result_type": method, //TODO: is this right?
        "error": {
          "code": error.code().to_string(),
          "message": error.message()
        }
      })
    };
    
    //TODO: how to send message to relay server ?
  }
}