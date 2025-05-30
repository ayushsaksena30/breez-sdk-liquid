#![cfg_attr(feature = "test-utils", allow(dead_code))]

use bip39::rand::{self, distributions::Alphanumeric, Rng};

pub(crate) mod bolt12_offer;
pub(crate) mod chain;
pub(crate) mod chain_swap;
pub mod persist;
pub(crate) mod receive_swap;
pub(crate) mod recover;
pub(crate) mod sdk;
pub(crate) mod send_swap;
pub(crate) mod status_stream;
pub(crate) mod swapper;
pub(crate) mod sync;
pub(crate) mod wallet;

pub(crate) fn generate_random_string(size: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(size)
        .map(char::from)
        .collect()
}
