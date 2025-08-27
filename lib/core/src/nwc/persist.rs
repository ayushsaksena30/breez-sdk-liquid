use crate::persist::Persister;
use anyhow::Result;
use serde::{Deserialize, Serialize};

const KEY_NWC_DETAILS: &str = "nwc_details";

#[derive(Serialize, Deserialize)]
pub(crate) struct NwcDetails {
    pub uri: String,
    pub expiry: u32,
    pub our_seckey: Option<String>,
}

impl Persister {
    pub(crate) fn set_nwc_details(&self, details: NwcDetails) -> Result<()> {
        self.update_cached_item(KEY_NWC_DETAILS, serde_json::to_string(&details)?)
    }

    pub(crate) fn get_nwc_details(&self) -> Result<Option<NwcDetails>> {
        Ok(self
            .get_cached_item(KEY_NWC_DETAILS)?
            .and_then(|details| serde_json::from_str(&details).ok()))
    }
}
