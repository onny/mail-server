use std::time::{Duration, Instant};

use ahash::{AHashMap, AHashSet};
use foundationdb::{
    options::{MutationType, StreamingMode},
    FdbError, KeySelector, RangeOption,
};
use futures::StreamExt;
use rand::Rng;

use crate::{
    write::{
        key::{DeserializeBigEndian, KeySerializer},
        now, Batch, Operation,
    },
    AclKey, BitmapKey, Deserialize, IndexKey, LogKey, Serialize, Store, ValueKey, SUBSPACE_VALUES,
};

use super::bitmap::{next_available_index, DenseBitmap, BITS_PER_BLOCK};

#[cfg(not(feature = "test_mode"))]
pub const ID_ASSIGNMENT_EXPIRY: u64 = 60 * 60; // seconds
#[cfg(not(feature = "test_mode"))]
const MAX_COMMIT_ATTEMPTS: u32 = 10;
#[cfg(not(feature = "test_mode"))]
const MAX_COMMIT_TIME: Duration = Duration::from_secs(10);

#[cfg(feature = "test_mode")]
pub static ID_ASSIGNMENT_EXPIRY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(60 * 60); // seconds
#[cfg(feature = "test_mode")]
const MAX_COMMIT_ATTEMPTS: u32 = 1000;
#[cfg(feature = "test_mode")]
const MAX_COMMIT_TIME: Duration = Duration::from_secs(3600);

#[cfg(feature = "test_mode")]
lazy_static::lazy_static! {
pub static ref BITMAPS: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<Vec<u8>, std::collections::HashSet<u32>>>> =
                    std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
}

impl Store {
    pub async fn write(&self, batch: Batch) -> crate::Result<()> {
        let start = Instant::now();
        let mut retry_count = 0;
        let mut set_bitmaps = AHashMap::new();
        let mut clear_bitmaps = AHashMap::new();

        loop {
            let mut account_id = u32::MAX;
            let mut collection = u8::MAX;
            let mut document_id = u32::MAX;

            let trx = self.db.create_trx()?;

            for op in &batch.ops {
                match op {
                    Operation::AccountId {
                        account_id: account_id_,
                    } => {
                        account_id = *account_id_;
                    }
                    Operation::Collection {
                        collection: collection_,
                    } => {
                        collection = *collection_;
                    }
                    Operation::DocumentId {
                        document_id: document_id_,
                    } => {
                        document_id = *document_id_;
                    }
                    Operation::Value { family, field, set } => {
                        let key = ValueKey {
                            account_id,
                            collection,
                            document_id,
                            family: *family,
                            field: *field,
                        }
                        .serialize();
                        if let Some(value) = set {
                            trx.set(&key, value);
                        } else {
                            trx.clear(&key);
                        }
                    }
                    Operation::Index { field, key, set } => {
                        let key = IndexKey {
                            account_id,
                            collection,
                            document_id,
                            field: *field,
                            key,
                        }
                        .serialize();
                        if *set {
                            trx.set(&key, &[]);
                        } else {
                            trx.clear(&key);
                        }
                    }
                    Operation::Bitmap {
                        family,
                        field,
                        key,
                        set,
                    } => {
                        if retry_count == 0 {
                            if *set {
                                &mut set_bitmaps
                            } else {
                                &mut clear_bitmaps
                            }
                            .entry(
                                BitmapKey {
                                    account_id,
                                    collection,
                                    family: *family,
                                    field: *field,
                                    block_num: DenseBitmap::block_num(document_id),
                                    key,
                                }
                                .serialize(),
                            )
                            .or_insert_with(DenseBitmap::empty)
                            .set(document_id);
                        }
                    }
                    Operation::Acl {
                        grant_account_id,
                        set,
                    } => {
                        let key = AclKey {
                            grant_account_id: *grant_account_id,
                            to_account_id: account_id,
                            to_collection: collection,
                            to_document_id: document_id,
                        }
                        .serialize();
                        if let Some(value) = set {
                            trx.set(&key, value);
                        } else {
                            trx.clear(&key);
                        }
                    }
                    Operation::Log {
                        collection,
                        change_id,
                        set,
                    } => {
                        let key = LogKey {
                            account_id,
                            collection: *collection,
                            change_id: *change_id,
                        }
                        .serialize();
                        trx.set(&key, set);
                    }
                    Operation::AssertValue {
                        field,
                        family,
                        assert_value,
                    } => {
                        let key = ValueKey {
                            account_id,
                            collection,
                            document_id,
                            family: *family,
                            field: *field,
                        }
                        .serialize();
                        if trx
                            .get(&key, false)
                            .await
                            .unwrap_or_default()
                            .map_or(true, |bytes| !assert_value.matches(bytes.as_ref()))
                        {
                            trx.cancel();
                            return Err(crate::Error::AssertValueFailed);
                        }
                    }
                }
            }

            for (key, bitmap) in &set_bitmaps {
                trx.atomic_op(key, &bitmap.bitmap, MutationType::BitOr);
            }

            for (key, bitmap) in &clear_bitmaps {
                trx.atomic_op(key, &bitmap.bitmap, MutationType::BitXor);
            }

            match trx.commit().await {
                Ok(_) => {
                    #[cfg(feature = "test_mode")]
                    {
                        for op in &batch.ops {
                            match op {
                                Operation::AccountId {
                                    account_id: account_id_,
                                } => {
                                    account_id = *account_id_;
                                }
                                Operation::Collection {
                                    collection: collection_,
                                } => {
                                    collection = *collection_;
                                }
                                Operation::DocumentId {
                                    document_id: document_id_,
                                } => {
                                    document_id = *document_id_;
                                }
                                Operation::Bitmap {
                                    family,
                                    field,
                                    key,
                                    set,
                                } => {
                                    let key = BitmapKey {
                                        account_id,
                                        collection,
                                        family: *family,
                                        field: *field,
                                        block_num: DenseBitmap::block_num(document_id),
                                        key,
                                    }
                                    .serialize();
                                    if *set {
                                        assert!(
                                            BITMAPS
                                                .lock()
                                                .entry(key.clone())
                                                .or_default()
                                                .insert(document_id),
                                            "key {key:?} already contains document {document_id}"
                                        );
                                    } else {
                                        assert!(
                                            BITMAPS
                                                .lock()
                                                .get_mut(&key)
                                                .unwrap()
                                                .remove(&document_id),
                                            "key {key:?} does not contain document {document_id}"
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    return Ok(());
                }
                Err(err) => {
                    if retry_count < MAX_COMMIT_ATTEMPTS && start.elapsed() < MAX_COMMIT_TIME {
                        err.on_error().await?;
                        retry_count += 1;
                    } else {
                        return Err(FdbError::from(err).into());
                    }
                }
            }
        }
    }

    pub async fn assign_document_id(
        &self,
        account_id: u32,
        collection: impl Into<u8>,
    ) -> crate::Result<u32> {
        let start = Instant::now();
        let collection = collection.into();

        loop {
            //let mut assign_source = 0;
            // First try to reuse an expired assigned id
            let begin = IndexKey {
                account_id,
                collection,
                document_id: 0,
                field: u8::MAX,
                key: &[],
            }
            .serialize();
            let end = IndexKey {
                account_id,
                collection,
                document_id: u32::MAX,
                field: u8::MAX,
                key: &[],
            }
            .serialize();
            let trx = self.db.create_trx()?;

            let mut values = trx.get_ranges(
                RangeOption {
                    begin: KeySelector::first_greater_or_equal(begin),
                    end: KeySelector::first_greater_or_equal(end),
                    mode: StreamingMode::Iterator,
                    reverse: false,
                    ..RangeOption::default()
                },
                true,
            );

            #[cfg(not(feature = "test_mode"))]
            let expired_timestamp = now() - ID_ASSIGNMENT_EXPIRY;
            #[cfg(feature = "test_mode")]
            let expired_timestamp =
                now() - ID_ASSIGNMENT_EXPIRY.load(std::sync::atomic::Ordering::Relaxed);
            let mut reserved_ids = AHashSet::new();
            let mut expired_ids = Vec::new();
            while let Some(values) = values.next().await {
                for value in values? {
                    let key = value.key();
                    let document_id =
                        key.deserialize_be_u32(key.len() - std::mem::size_of::<u32>())?;
                    if u64::deserialize(value.value())? <= expired_timestamp {
                        // Found an expired id, reuse it
                        expired_ids.push(document_id);
                    } else {
                        // Keep track of all reserved ids
                        reserved_ids.insert(document_id);
                    }
                }
            }
            drop(values);

            let mut document_id = u32::MAX;

            if !expired_ids.is_empty() {
                // Obtain a random id from the expired ids
                if expired_ids.len() > 1 {
                    document_id = expired_ids[rand::thread_rng().gen_range(0..expired_ids.len())];
                    //assign_source = 1;
                } else {
                    document_id = expired_ids[0];
                    //assign_source = 2;
                }
            } else {
                // Find the next available id
                let mut key = BitmapKey::document_ids(account_id, collection);
                let begin = key.serialize();
                key.block_num = u32::MAX;
                let end = key.serialize();
                let mut values = trx.get_ranges(
                    RangeOption {
                        begin: KeySelector::first_greater_or_equal(begin),
                        end: KeySelector::first_greater_or_equal(end),
                        mode: StreamingMode::Iterator,
                        reverse: false,
                        ..RangeOption::default()
                    },
                    true,
                );

                'outer: while let Some(values) = values.next().await {
                    for value in values? {
                        let key = value.key();
                        if let Some(next_id) = next_available_index(
                            value.value(),
                            key.deserialize_be_u32(key.len() - std::mem::size_of::<u32>())?,
                            &reserved_ids,
                        ) {
                            document_id = next_id;
                            //assign_source = 3;

                            break 'outer;
                        }
                    }
                }
            }

            // If no ids were found, assign the first available id that is not reserved
            if document_id == u32::MAX {
                document_id = 1024;
                for document_id_ in 0..BITS_PER_BLOCK {
                    if !reserved_ids.contains(&document_id_) {
                        document_id = document_id_;
                        //assign_source = 4;

                        break;
                    }
                }
            }

            // Reserve the id
            let key = IndexKey {
                account_id,
                collection,
                document_id,
                field: u8::MAX,
                key: &[],
            }
            .serialize();
            trx.get(&key, false).await?; // Read to create conflict range
            trx.set(&key, &now().serialize());

            match trx.commit().await {
                Ok(_) => {
                    //println!("assigned id: {document_id} {assign_source}");

                    return Ok(document_id);
                }
                Err(err) => {
                    if start.elapsed() < MAX_COMMIT_TIME {
                        err.on_error().await?;
                    } else {
                        return Err(FdbError::from(err).into());
                    }
                }
            }
        }
    }

    pub async fn assign_change_id(&self, account_id: u32) -> crate::Result<u64> {
        let start = Instant::now();
        let counter = KeySerializer::new(std::mem::size_of::<u32>() + 2)
            .write(SUBSPACE_VALUES)
            .write(account_id)
            .finalize();

        loop {
            // Read id
            let trx = self.db.create_trx()?;
            let id = if let Some(bytes) = trx.get(&counter, false).await? {
                u64::deserialize(&bytes)? + 1
            } else {
                0
            };
            trx.set(&counter, &id.serialize());

            match trx.commit().await {
                Ok(_) => {
                    return Ok(id);
                }
                Err(err) => {
                    if start.elapsed() < MAX_COMMIT_TIME {
                        err.on_error().await?;
                    } else {
                        return Err(FdbError::from(err).into());
                    }
                }
            }
        }
    }

    #[cfg(feature = "test_mode")]
    pub async fn destroy(&self) {
        let trx = self.db.create_trx().unwrap();
        trx.clear_range(&[0u8], &[u8::MAX]);
        trx.commit().await.unwrap();
    }
}
