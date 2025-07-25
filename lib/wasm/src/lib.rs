mod error;
mod event;
mod logger;
pub mod model;
mod platform;
mod signer;

use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;

use crate::event::{EventListener, WasmEventListener};
use crate::model::*;

use anyhow::anyhow;
use breez_sdk_liquid::elements::hex::ToHex;
use breez_sdk_liquid::persist::Persister;
use breez_sdk_liquid::sdk::{LiquidSdk, LiquidSdkBuilder};
use breez_sdk_liquid::signer::SdkLwkSigner;
use breez_sdk_liquid::PRODUCTION_BREEZSERVER_URL;
use log::LevelFilter;
use logger::{Logger, WasmLogger};
use signer::{Signer, WasmSigner};
use wasm_bindgen::prelude::*;

pub const BREEZ_WASM_SYNC_SERVICE_URL: &str = "https://datasync.breez.technology:442";

#[wasm_bindgen]
pub struct BindingLiquidSdk {
    sdk: Rc<LiquidSdk>,
}

#[wasm_bindgen(js_name = "connect")]
pub async fn connect(req: ConnectRequest) -> WasmResult<BindingLiquidSdk> {
    let signer = Box::new(LiquidSdk::default_signer(&req.clone().into())?);
    connect_inner(req.config, signer).await
}

#[wasm_bindgen(js_name = "connectWithSigner")]
pub async fn connect_with_signer(
    req: ConnectWithSignerRequest,
    signer: Signer,
) -> WasmResult<BindingLiquidSdk> {
    let signer: Box<dyn breez_sdk_liquid::model::Signer> = Box::new(WasmSigner { signer });
    connect_inner(req.config, signer).await
}

async fn connect_inner(
    config: Config,
    signer: Box<dyn breez_sdk_liquid::model::Signer>,
) -> WasmResult<BindingLiquidSdk> {
    let config: breez_sdk_liquid::model::Config = config.into();
    let signer = Rc::new(signer);

    let mut sdk_builder = LiquidSdkBuilder::new(
        config.clone(),
        PRODUCTION_BREEZSERVER_URL.to_string(),
        Rc::clone(&signer),
    )?;

    let sdk_lwk_signer = SdkLwkSigner::new(Rc::clone(&signer))?;
    let fingerprint = sdk_lwk_signer.fingerprint()?;
    let fingerprint = fingerprint.to_hex();

    let wallet_dir = PathBuf::from_str(&config.get_wallet_dir(&config.working_dir, &fingerprint)?)
        .map_err(|e| anyhow!(e.to_string()))?;

    let maybe_db_backup_persister = platform::create_db_backup_persister(&wallet_dir).ok();
    let maybe_backup_bytes = match &maybe_db_backup_persister {
        Some(p) => p.load_backup().await.unwrap_or_else(|e| {
            log::error!("Failed to load db backup: {:?}", e);
            None
        }),
        None => None,
    };

    let persister = Arc::new(Persister::new_in_memory(
        &config.working_dir,
        config.network,
        config.sync_enabled(),
        config.asset_metadata.clone(),
        maybe_backup_bytes,
    )?);

    sdk_builder.persister(persister.clone());

    let sdk = sdk_builder.build().await?;
    sdk.start().await?;

    if let Some(db_backup_persister) = maybe_db_backup_persister {
        let (sender, receiver) = tokio::sync::mpsc::channel(20);
        let listener = platform::db_backup_common::ForwardingEventListener::new(sender);
        sdk.add_event_listener(Box::new(listener)).await?;

        db_backup_persister.start_backup_task(persister, receiver);
    }

    Ok(BindingLiquidSdk { sdk })
}

#[wasm_bindgen(js_name = "defaultConfig")]
pub fn default_config(network: LiquidNetwork, breez_api_key: Option<String>) -> WasmResult<Config> {
    let mut config = match network {
        LiquidNetwork::Mainnet => breez_sdk_liquid::model::Config::mainnet_esplora(breez_api_key),
        LiquidNetwork::Testnet => breez_sdk_liquid::model::Config::testnet_esplora(breez_api_key),
        LiquidNetwork::Regtest => breez_sdk_liquid::model::Config::regtest_esplora(),
    };
    config.sync_service_url = Some(BREEZ_WASM_SYNC_SERVICE_URL.to_string());
    Ok(config.into())
}

#[wasm_bindgen(js_name = "parseInvoice")]
pub fn parse_invoice(input: String) -> WasmResult<LNInvoice> {
    Ok(LiquidSdk::parse_invoice(&input)?.into())
}

#[wasm_bindgen(js_name = "setLogger")]
pub fn set_logger(logger: Logger) -> WasmResult<()> {
    crate::logger::WASM_LOGGER.set(Some(logger));

    let wasm_logger = WasmLogger {};
    Ok(log::set_boxed_logger(Box::new(wasm_logger))
        .map(|_| log::set_max_level(LevelFilter::Trace))
        .map_err(|_| anyhow!("Logger already created"))?)
}

#[wasm_bindgen]
impl BindingLiquidSdk {
    #[wasm_bindgen(js_name = "getInfo")]
    pub async fn get_info(&self) -> WasmResult<GetInfoResponse> {
        Ok(self.sdk.get_info().await?.into())
    }

    #[wasm_bindgen(js_name = "signMessage")]
    pub fn sign_message(&self, req: SignMessageRequest) -> WasmResult<SignMessageResponse> {
        Ok(self.sdk.sign_message(&req.into())?.into())
    }

    #[wasm_bindgen(js_name = "checkMessage")]
    pub fn check_message(&self, req: CheckMessageRequest) -> WasmResult<CheckMessageResponse> {
        Ok(self.sdk.check_message(&req.into())?.into())
    }

    #[wasm_bindgen(js_name = "parse")]
    pub async fn parse(&self, input: String) -> WasmResult<InputType> {
        Ok(self.sdk.parse(&input).await?.into())
    }

    #[wasm_bindgen(js_name = "addEventListener")]
    pub async fn add_event_listener(&self, listener: EventListener) -> WasmResult<String> {
        Ok(self
            .sdk
            .add_event_listener(Box::new(WasmEventListener { listener }))
            .await?)
    }

    #[wasm_bindgen(js_name = "removeEventListener")]
    pub async fn remove_event_listener(&self, id: String) -> WasmResult<()> {
        self.sdk.remove_event_listener(id).await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "prepareSendPayment")]
    pub async fn prepare_send_payment(
        &self,
        req: PrepareSendRequest,
    ) -> WasmResult<PrepareSendResponse> {
        Ok(self.sdk.prepare_send_payment(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "sendPayment")]
    pub async fn send_payment(&self, req: SendPaymentRequest) -> WasmResult<SendPaymentResponse> {
        Ok(self.sdk.send_payment(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "prepareReceivePayment")]
    pub async fn prepare_receive_payment(
        &self,
        req: PrepareReceiveRequest,
    ) -> WasmResult<PrepareReceiveResponse> {
        Ok(self.sdk.prepare_receive_payment(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "receivePayment")]
    pub async fn receive_payment(
        &self,
        req: ReceivePaymentRequest,
    ) -> WasmResult<ReceivePaymentResponse> {
        Ok(self.sdk.receive_payment(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "createBolt12Invoice")]
    pub async fn create_bolt12_invoice(
        &self,
        req: CreateBolt12InvoiceRequest,
    ) -> WasmResult<CreateBolt12InvoiceResponse> {
        Ok(self.sdk.create_bolt12_invoice(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "fetchLightningLimits")]
    pub async fn fetch_lightning_limits(&self) -> WasmResult<LightningPaymentLimitsResponse> {
        Ok(self.sdk.fetch_lightning_limits().await?.into())
    }

    #[wasm_bindgen(js_name = "fetchOnchainLimits")]
    pub async fn fetch_onchain_limits(&self) -> WasmResult<OnchainPaymentLimitsResponse> {
        Ok(self.sdk.fetch_onchain_limits().await?.into())
    }

    #[wasm_bindgen(js_name = "preparePayOnchain")]
    pub async fn prepare_pay_onchain(
        &self,
        req: PreparePayOnchainRequest,
    ) -> WasmResult<PreparePayOnchainResponse> {
        Ok(self.sdk.prepare_pay_onchain(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "payOnchain")]
    pub async fn pay_onchain(&self, req: PayOnchainRequest) -> WasmResult<SendPaymentResponse> {
        Ok(self.sdk.pay_onchain(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "prepareBuyBitcoin")]
    pub async fn prepare_buy_bitcoin(
        &self,
        req: PrepareBuyBitcoinRequest,
    ) -> WasmResult<PrepareBuyBitcoinResponse> {
        Ok(self.sdk.prepare_buy_bitcoin(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "buyBitcoin")]
    pub async fn buy_bitcoin(&self, req: BuyBitcoinRequest) -> WasmResult<String> {
        Ok(self.sdk.buy_bitcoin(&req.into()).await?)
    }

    #[wasm_bindgen(js_name = "listPayments")]
    pub async fn list_payments(&self, req: ListPaymentsRequest) -> WasmResult<Vec<Payment>> {
        Ok(self
            .sdk
            .list_payments(&req.into())
            .await?
            .into_iter()
            .map(|r| r.into())
            .collect())
    }

    #[wasm_bindgen(js_name = "getPayment")]
    pub async fn get_payment(&self, req: GetPaymentRequest) -> WasmResult<Option<Payment>> {
        Ok(self.sdk.get_payment(&req.into()).await?.map(|r| r.into()))
    }

    #[wasm_bindgen(js_name = "fetchPaymentProposedFees")]
    pub async fn fetch_payment_proposed_fees(
        &self,
        req: FetchPaymentProposedFeesRequest,
    ) -> WasmResult<FetchPaymentProposedFeesResponse> {
        Ok(self
            .sdk
            .fetch_payment_proposed_fees(&req.into())
            .await?
            .into())
    }

    #[wasm_bindgen(js_name = "acceptPaymentProposedFees")]
    pub async fn accept_payment_proposed_fees(
        &self,
        req: AcceptPaymentProposedFeesRequest,
    ) -> WasmResult<()> {
        self.sdk.accept_payment_proposed_fees(&req.into()).await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "prepareLnurlPay")]
    pub async fn prepare_lnurl_pay(
        &self,
        req: PrepareLnUrlPayRequest,
    ) -> WasmResult<PrepareLnUrlPayResponse> {
        Ok(self.sdk.prepare_lnurl_pay(req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "lnurlPay")]
    pub async fn lnurl_pay(&self, req: LnUrlPayRequest) -> WasmResult<LnUrlPayResult> {
        Ok(self.sdk.lnurl_pay(req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "lnurlWithdraw")]
    pub async fn lnurl_withdraw(
        &self,
        req: LnUrlWithdrawRequest,
    ) -> WasmResult<LnUrlWithdrawResult> {
        Ok(self.sdk.lnurl_withdraw(req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "lnurlAuth")]
    pub async fn lnurl_auth(
        &self,
        req_data: LnUrlAuthRequestData,
    ) -> WasmResult<LnUrlCallbackStatus> {
        Ok(self.sdk.lnurl_auth(req_data.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "registerWebhook")]
    pub async fn register_webhook(&self, webhook_url: String) -> WasmResult<()> {
        self.sdk.register_webhook(webhook_url).await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "unregisterWebhook")]
    pub async fn unregister_webhook(&self) -> WasmResult<()> {
        self.sdk.unregister_webhook().await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "fetchFiatRates")]
    pub async fn fetch_fiat_rates(&self) -> WasmResult<Vec<Rate>> {
        Ok(self
            .sdk
            .fetch_fiat_rates()
            .await?
            .into_iter()
            .map(|r| r.into())
            .collect())
    }

    #[wasm_bindgen(js_name = "listFiatCurrencies")]
    pub async fn list_fiat_currencies(&self) -> WasmResult<Vec<FiatCurrency>> {
        Ok(self
            .sdk
            .list_fiat_currencies()
            .await?
            .into_iter()
            .map(|r| r.into())
            .collect())
    }

    #[wasm_bindgen(js_name = "listRefundables")]
    pub async fn list_refundables(&self) -> WasmResult<Vec<RefundableSwap>> {
        Ok(self
            .sdk
            .list_refundables()
            .await?
            .into_iter()
            .map(|r| r.into())
            .collect())
    }

    #[wasm_bindgen(js_name = "prepareRefund")]
    pub async fn prepare_refund(
        &self,
        req: PrepareRefundRequest,
    ) -> WasmResult<PrepareRefundResponse> {
        Ok(self.sdk.prepare_refund(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "refund")]
    pub async fn refund(&self, req: RefundRequest) -> WasmResult<RefundResponse> {
        Ok(self.sdk.refund(&req.into()).await?.into())
    }

    #[wasm_bindgen(js_name = "rescanOnchainSwaps")]
    pub async fn rescan_onchain_swaps(&self) -> WasmResult<()> {
        self.sdk.rescan_onchain_swaps().await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "sync")]
    pub async fn sync(&self) -> WasmResult<()> {
        self.sdk.sync(false).await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "recommendedFees")]
    pub async fn recommended_fees(&self) -> WasmResult<RecommendedFees> {
        Ok(self.sdk.recommended_fees().await?.into())
    }

    #[wasm_bindgen(js_name = "backup")]
    pub fn backup(&self, req: BackupRequest) -> WasmResult<()> {
        self.sdk.backup(req.into())?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "restore")]
    pub fn restore(&self, req: RestoreRequest) -> WasmResult<()> {
        self.sdk.restore(req.into())?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "disconnect")]
    pub async fn disconnect(&self) -> WasmResult<()> {
        self.sdk.disconnect().await?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "getNwcUri")]
    pub async fn get_nwc_uri(&self) -> WasmResult<String> {
        Ok(self.sdk.get_nwc_uri().await?)
    }
}
