//! In-memory compact block cache used during sync.
//!
//! Blocks are downloaded, scanned and dropped within a single sync run, so there is
//! no reason to persist them.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::Mutex;

use async_trait::async_trait;
use zcash_client_backend::data_api::chain::{BlockCache, BlockSource, error};
use zcash_client_backend::data_api::scanning::ScanRange;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_protocol::consensus::BlockHeight;

#[derive(Debug)]
pub struct CacheError(String);

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block cache: {}", self.0)
    }
}

impl std::error::Error for CacheError {}

#[derive(Default)]
pub struct MemBlockCache {
    blocks: Mutex<BTreeMap<u32, CompactBlock>>,
}

impl MemBlockCache {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<u32, CompactBlock>>, CacheError> {
        self.blocks
            .lock()
            .map_err(|_| CacheError("lock poisoned".into()))
    }
}

fn heights(range: &ScanRange) -> Range<u32> {
    u32::from(range.block_range().start)..u32::from(range.block_range().end)
}

impl BlockSource for MemBlockCache {
    type Error = CacheError;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), error::Error<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), error::Error<WalletErrT, Self::Error>>,
    {
        let start = from_height.map(u32::from).unwrap_or(0);
        let blocks: Vec<CompactBlock> = self
            .lock()
            .map_err(error::Error::BlockSource)?
            .range(start..)
            .take(limit.unwrap_or(usize::MAX))
            .map(|(_, block)| block.clone())
            .collect();
        for block in blocks {
            with_block(block)?;
        }
        Ok(())
    }
}

#[async_trait]
impl BlockCache for MemBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let blocks = self.lock()?;
        let tip = match range {
            Some(range) => blocks.range(heights(range)).next_back().map(|(h, _)| *h),
            None => blocks.keys().next_back().copied(),
        };
        Ok(tip.map(BlockHeight::from_u32))
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        Ok(self
            .lock()?
            .range(heights(range))
            .map(|(_, block)| block.clone())
            .collect())
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut blocks = self.lock()?;
        for block in compact_blocks {
            let height = u32::try_from(block.height)
                .map_err(|_| CacheError("block height out of range".into()))?;
            blocks.insert(height, block);
        }
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        let range = heights(&range);
        self.lock()?.retain(|height, _| !range.contains(height));
        Ok(())
    }
}
