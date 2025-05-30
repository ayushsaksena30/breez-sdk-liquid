use std::sync::Arc;
use anyhow::Result;
use nostr::nips::nip47::{
    PayInvoiceRequest,
    PayInvoiceResponse,
    ListTransactionsRequest,
    ListTransactionsResponse,
    Transaction,
    GetBalanceResponse,
    Error,
    ErrorCode,
};
use crate::sdk::LiquidSdk;
use crate::model::{
    PaymentType,
    PaymentState,
    ListPaymentsRequest,
    payment::{PrepareSendRequest, SendPaymentRequest, PayAmount},
};
use crate::nwc::BreezRelayService;

pub trait RelayMessageHandler {
  async fn pay_invoice(&self, params: String) -> Result<PayInvoiceResponse, Error>;
  async fn list_transactions(&self, params: String) -> Result<ListTransactionsResponse, Error>;
  async fn get_balance(&self) -> Result<GetBalanceResponse, Error>;
}

pub struct BreezRelayMessageHandler {
  sdk: Arc<LiquidSdk>,
}

impl BreezRelayMessageHandler {
  pub fn new(sdk: Arc<LiquidSdk>) -> Self {
    Self { sdk }
  }
}

impl RelayMessageHandler for BreezRelayMessageHandler {
  async fn pay_invoice(&self, params: String) -> Result<PayInvoiceResponse, Error> {
    // Parse the JSON params into PayInvoiceRequest
    let request: PayInvoiceRequest = serde_json::from_str(&params)
      .map_err(|e| Error::new(ErrorCode::InvalidRequest, format!("Invalid params: {}", e)))?;
    
    // Create prepare request
    let prepare_req = PrepareSendRequest {
      destination: request.invoice,
      amount: request.amount.map(|a| PayAmount::Bitcoin {
        receiver_amount_sat: a / 1000, // Convert msats to sats
      })
    };

    // Prepare the payment
    let prepare_resp = self.sdk.prepare_send_payment(&prepare_req).await
      .map_err(|e| Error::new(ErrorCode::PaymentFailed, format!("Failed to prepare payment: {}", e)))?;

    // Create send request
    let send_req = SendPaymentRequest {
      prepare_response: prepare_resp,
      use_asset_fees: None
    };

    // Send the payment
    let response = self.sdk.send_payment(&send_req).await
      .map_err(|e| Error::new(ErrorCode::PaymentFailed, format!("Failed to send payment: {}", e)))?;

    // Extract preimage and fees from payment
    let preimage = response.payment.preimage.unwrap_or_default();
    let fees_paid = response.payment.fees_paid.map(|f| f * 1000); // Convert sats to msats

    // Create NIP-47 response
    Ok(PayInvoiceResponse {
      preimage,
      fees_paid, //TODO: this is optional, can't send inside PayInvoiceResponse
    })
  }
  
  async fn list_transactions(&self, params: String) -> Result<ListTransactionsResponse, Error> {
    // Parse the JSON params into ListTransactionsRequest
    let request: ListTransactionsRequest = serde_json::from_str(&params)
      .map_err(|e| Error::new(ErrorCode::InvalidRequest, format!("Invalid params: {}", e)))?;

    // Convert type filter to payment types
    let filters = match request.transaction_type.as_deref() {
      Some("incoming") => Some(vec![PaymentType::Receive]),
      Some("outgoing") => Some(vec![PaymentType::Send]),
      _ => None,
    };

    // Convert unpaid filter to payment states
    let states = request.unpaid.map(|unpaid| {
      if unpaid {
        vec![PaymentState::Pending]
      } else {
        vec![]
      }
    });

    // Get payments from SDK
    let payments = self.sdk.list_payments(&ListPaymentsRequest {
      filters,
      states,
      from_timestamp: request.from,
      to_timestamp: request.until,
      limit: request.limit,
      offset: request.offset,
      details: None,
      sort_ascending: Some(false),
    }).await
    .map_err(|e| Error::new(ErrorCode::Internal, format!("Failed to list payments: {}", e)))?;

    // Convert payments to NIP-47 transactions
    let transactions: Vec<Transaction> = payments.into_iter()
      .map(|payment| {
        Transaction {
          id: payment.payment_hash,
          invoice: payment.bolt11,
          description: payment.description,
          description_hash: None,
          preimage: payment.preimage,
          payment_hash: payment.payment_hash,
          amount: payment.amount_msat,
          fees_paid: payment.fees_paid.map(|f| f * 1000), // Convert sats to msats
          timestamp: payment.timestamp,
          settled_at: payment.settled_at,
          payment_type: match payment.payment_type {
            PaymentType::Send => "outgoing",
            PaymentType::Receive => "incoming",
            _ => "unknown",
          }.to_string(),
          state: match payment.state {
            PaymentState::Pending => "pending",
            PaymentState::Completed => "completed",
            PaymentState::Failed => "failed",
            _ => "unknown",
          }.to_string(),
        }
      })
      .collect();

    // Create NIP-47 response
    Ok(ListTransactionsResponse {
      transactions,
    })
  }

  async fn get_balance(&self) -> Result<GetBalanceResponse, Error> {
    // Get wallet info from SDK
    let info = self.sdk.get_info().await
      .map_err(|e| Error::new(ErrorCode::Internal, format!("Failed to get wallet info: {}", e)))?;

    // Convert balance to msats
    let balance_msats = info.wallet_info.balance_sat * 1000;

    // Create NIP-47 response
    Ok(GetBalanceResponse {
      balance: balance_msats,
    })
  }
}