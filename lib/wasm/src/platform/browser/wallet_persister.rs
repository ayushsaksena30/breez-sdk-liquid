use crate::platform::wallet_persister_common::maybe_merge_updates;
use anyhow::{anyhow, Context};
use breez_sdk_liquid::wallet::persister::lwk_wollet::{PersistError, Update, WolletDescriptor};
use breez_sdk_liquid::wallet::persister::{lwk_wollet, LwkPersister, WalletCachePersister};
use indexed_db_futures::database::Database;
use indexed_db_futures::iter::ArrayMapIter;
use indexed_db_futures::object_store::ObjectStore;
use indexed_db_futures::query_source::QuerySource;
use indexed_db_futures::transaction::TransactionMode;
use indexed_db_futures::Build;
use log::{info, warn};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{Receiver, Sender};

const IDB_STORE_NAME: &str = "BREEZ_SDK_LIQUID_WALLET_CACHE_STORE";

#[sdk_macros::async_trait]
pub(crate) trait AsyncWalletStorage: Send + Sync + Clone + 'static {
    // Load all existing updates from the storage backend.
    async fn load_updates(&self) -> anyhow::Result<Vec<Update>>;

    // Persist a single update at a given index.
    async fn persist_update(&self, update: Update, index: u32) -> anyhow::Result<()>;

    // Clear all persisted data.
    async fn clear(&self) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub(crate) struct AsyncWalletCachePersister<S: AsyncWalletStorage> {
    lwk_persister: Arc<AsyncLwkPersister<S>>,
}

impl<S: AsyncWalletStorage> AsyncWalletCachePersister<S> {
    pub async fn new(storage: Arc<S>) -> anyhow::Result<Self> {
        Ok(Self {
            lwk_persister: Arc::new(AsyncLwkPersister::new(storage).await?),
        })
    }
}

#[sdk_macros::async_trait]
impl<S: AsyncWalletStorage> WalletCachePersister for AsyncWalletCachePersister<S> {
    fn get_lwk_persister(&self) -> anyhow::Result<LwkPersister> {
        let persister = std::sync::Arc::clone(&self.lwk_persister);
        Ok(persister as LwkPersister)
    }

    async fn clear_cache(&self) -> anyhow::Result<()> {
        self.lwk_persister.storage.clear().await?;
        self.lwk_persister.updates.lock().unwrap().clear();
        Ok(())
    }
}

struct AsyncLwkPersister<S: AsyncWalletStorage> {
    updates: Mutex<Vec<Update>>,
    sender: Sender<(Update, /*index*/ u32)>,
    storage: Arc<S>,
}

impl<S: AsyncWalletStorage> AsyncLwkPersister<S> {
    async fn new(storage: Arc<S>) -> anyhow::Result<Self> {
        let updates = storage.load_updates().await?;
        info!("Loaded {} updates from storage", updates.len());

        let (sender, receiver) = tokio::sync::mpsc::channel(20);

        Self::start_persist_task(storage.clone(), receiver);

        Ok(Self {
            updates: Mutex::new(updates),
            sender,
            storage,
        })
    }

    fn start_persist_task(storage: Arc<S>, mut receiver: Receiver<(Update, /*index*/ u32)>) {
        wasm_bindgen_futures::spawn_local(async move {
            // Persist updates and break on any error (giving up on cache persistence for the rest of the session)
            // A failed update followed by a successful one may leave the cache in an inconsistent state
            while let Some((update, index)) = receiver.recv().await {
                info!("Starting to persist wallet cache update at index {}", index);
                if let Err(e) = storage.persist_update(update, index).await {
                    log::error!("Failed to persist wallet cache update: {:?} - giving up on persisting wallet updates...", e);
                    break;
                }
            }
        });
    }
}

impl<S: AsyncWalletStorage> lwk_wollet::Persister for AsyncLwkPersister<S> {
    fn get(&self, index: usize) -> Result<Option<Update>, PersistError> {
        Ok(self.updates.lock().unwrap().get(index).cloned())
    }

    fn push(&self, update: Update) -> Result<(), PersistError> {
        let mut updates = self.updates.lock().unwrap();

        let (update, write_index) = maybe_merge_updates(update, updates.last(), updates.len());

        if let Err(e) = self.sender.try_send((update.clone(), write_index as u32)) {
            log::error!("Failed to send update to persister task {e}");
        }

        if write_index < updates.len() {
            updates[write_index] = update;
        } else {
            updates.push(update);
        }

        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct IndexedDbWalletStorage {
    db_name: String,
    desc: WolletDescriptor,
}

impl IndexedDbWalletStorage {
    pub fn new(working_dir: &Path, desc: WolletDescriptor) -> Self {
        let db_name = format!("{}-wallet-cache", working_dir.to_string_lossy());
        Self { db_name, desc }
    }
}

#[sdk_macros::async_trait]
impl AsyncWalletStorage for IndexedDbWalletStorage {
    async fn load_updates(&self) -> anyhow::Result<Vec<Update>> {
        let idb = open_indexed_db(&self.db_name).await?;

        let tx = idb
            .transaction([IDB_STORE_NAME])
            .with_mode(TransactionMode::Readonly)
            .build()
            .map_err(|e| anyhow!("Failed to build transaction: {}", e))?;

        let store = tx
            .object_store(IDB_STORE_NAME)
            .map_err(|e| anyhow!("Failed to open object store: {}", e))?;

        let updates_bytes: ArrayMapIter<Vec<u8>> = store
            .get_all()
            .await
            .map_err(|e| anyhow!("Failed to get all updates: {}", e))?;

        let max_index = get_max_index(&store).await?;
        if let Some(max_index) = max_index {
            if max_index != updates_bytes.len() as u32 - 1 {
                warn!("Wallet cache updates in IndexedDb are not contiguous. The (length - 1) is {}, but the max index is {max_index}. \
                This means it got corrupted (likely by concurrent SDK instances). Clearing the cache.", updates_bytes.len() as u32 - 1);
                self.clear().await?;
                return Ok(Vec::new());
            }
        }

        let mut updates = Vec::new();
        for update_bytes_result in updates_bytes {
            let update_bytes =
                update_bytes_result.map_err(|e| anyhow!("Failed to get update bytes: {}", e))?;
            updates.push(
                Update::deserialize_decrypted(&update_bytes, &self.desc)
                    .context("Failed to deserialize update")?,
            );
        }

        Ok(updates)
    }

    async fn persist_update(&self, update: Update, index: u32) -> anyhow::Result<()> {
        let update_bytes = update
            .serialize_encrypted(&self.desc)
            .map_err(|e| anyhow!("Failed to serialize update: {e}"))?;

        let idb = open_indexed_db(&self.db_name)
            .await
            .map_err(|e| anyhow!("Failed to open IndexedDB: {e}"))?;

        let tx = idb
            .transaction([IDB_STORE_NAME])
            .with_mode(TransactionMode::Readwrite)
            .build()
            .map_err(|e| anyhow!("Failed to build transaction: {e}"))?;

        let store = tx
            .object_store(IDB_STORE_NAME)
            .map_err(|e| anyhow!("Failed to open object store: {e}"))?;

        let max_index = get_max_index(&store).await?;

        if let Some(max_index) = max_index {
            if index > max_index + 1 {
                return Err(anyhow!(
                    "Index {index} is greater than the maximum index {max_index} + 1"
                ));
            }
        } else if index != 0 {
            return Err(anyhow!("Index {index} is not 0"));
        }

        store
            .put(update_bytes)
            .with_key(index)
            .await
            .map_err(|e| anyhow!("Failed to put update in store: {e}"))?;

        tx.commit()
            .await
            .map_err(|e| anyhow!("Failed to commit transaction: {e}"))?;

        Ok(())
    }

    async fn clear(&self) -> anyhow::Result<()> {
        let idb = open_indexed_db(&self.db_name).await?;

        let tx = idb
            .transaction([IDB_STORE_NAME])
            .with_mode(TransactionMode::Readwrite)
            .build()
            .map_err(|e| anyhow!("Failed to build transaction: {}", e))?;

        let store = tx
            .object_store(IDB_STORE_NAME)
            .map_err(|e| anyhow!("Failed to open object store: {}", e))?;

        store
            .clear()
            .map_err(|e| anyhow!("Failed to clear object store: {}", e))?;

        tx.commit()
            .await
            .map_err(|e| PersistError::Other(format!("Failed to commit transaction: {}", e)))?;

        Ok(())
    }
}

pub(crate) async fn open_indexed_db(name: &str) -> Result<Database, PersistError> {
    let db = Database::open(name)
        .with_version(1u32)
        .with_on_upgrade_needed(|event, db| {
            if let (0.0, Some(1.0)) = (event.old_version(), event.new_version()) {
                db.create_object_store(IDB_STORE_NAME).build()?;
            }

            Ok(())
        })
        .await
        .map_err(|e| PersistError::Other(format!("Failed to open IndexedDB: {}", e)))?;
    Ok(db)
}

async fn get_max_index(store: &ObjectStore<'_>) -> Result<Option<u32>, PersistError> {
    let keys: ArrayMapIter<u32> = store.get_all_keys().await.map_err(|e| {
        PersistError::Other(format!("Failed to get all keys from object store: {}", e))
    })?;
    let keys = keys.filter_map(|k| k.ok()).collect::<Vec<_>>();
    Ok(keys.into_iter().max())
}

#[cfg(test)]
mod tests {
    use crate::platform::browser::wallet_persister::{
        open_indexed_db, AsyncWalletStorage, IndexedDbWalletStorage, IDB_STORE_NAME,
    };
    use crate::platform::wallet_persister_common::tests::{get_lwk_update, get_wollet_descriptor};
    use anyhow::anyhow;
    use indexed_db_futures::transaction::TransactionMode;
    use indexed_db_futures::Build;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[sdk_macros::async_test_wasm]
    async fn test_load_updates() -> anyhow::Result<()> {
        let desc = get_wollet_descriptor()?;
        let storage = IndexedDbWalletStorage::new("test-load-updates".as_ref(), desc);

        // Non-contiguous keys are prevented (first index is 0)
        assert!(storage
            .persist_update(get_lwk_update(1, false), 1)
            .await
            .is_err());
        assert!(storage.load_updates().await?.is_empty());

        storage.persist_update(get_lwk_update(5, false), 0).await?;
        storage.persist_update(get_lwk_update(10, false), 1).await?;

        // Overwritting is allowed
        storage.persist_update(get_lwk_update(15, false), 1).await?;

        let updates = storage.load_updates().await?;
        assert_eq!(updates.len(), 2);

        // Non-contiguous keys are prevented
        assert!(storage
            .persist_update(get_lwk_update(10, false), 3)
            .await
            .is_err());

        let updates = storage.load_updates().await?;
        assert_eq!(updates.len(), 2);

        // If for any reason there is a gap, the cache is cleared on load
        storage.persist_update(get_lwk_update(15, false), 2).await?;
        delete_update_on_index(1, &storage.db_name).await;
        let updates = storage.load_updates().await?;
        assert_eq!(updates.len(), 0);

        Ok(())
    }

    async fn delete_update_on_index(index: u32, db_name: &str) {
        let idb = open_indexed_db(db_name).await.unwrap();

        let tx = idb
            .transaction([IDB_STORE_NAME])
            .with_mode(TransactionMode::Readwrite)
            .build()
            .unwrap();

        let store = tx.object_store(IDB_STORE_NAME).unwrap();

        store.delete(index).await.unwrap();

        tx.commit().await.unwrap();
    }
}
