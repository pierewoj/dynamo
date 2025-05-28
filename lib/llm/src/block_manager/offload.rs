// SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Offload Manager
//! The offload manager is responsible for handling all block transfers between different cache levels.
//!
//! ## Offloading
//! Offloading is the process of moving blocks to a cache level further away from the device.
//! When blocks are registered (via [`BlockPool::register_blocks`]), they are automatically sent to the offload manager.
//! Due to limited bandwidth, the offload manager must prioritize which offloads to perform.
//! This is indicated by the `priority` parameter to [`OffloadManager::offload`].
//! When a offload request is received, the offload manager will enqueue it into a priority queue.
//! This priority queue is keyed by the `priority` parameter, where blocks with lower priority values are processed first.
//! Within the same priority, blocks that were sent to the offload manager earlier are processed first.
//!
//! ## Onboarding
//! Onboarding is the process of moving blocks to a cache level closer to the device.
//! All onboardings are manually triggered through the [`OffloadManager::onboard`] method.
//!
//! ## Transfer Managers
//! The offload manager uses two transfer managers to handle the offloading and onboarding of blocks.
//!
//! The [`CudaTransferManager`] is responsible for transfers between the device and host.
//! The [`DiskTransferManager`] is responsible for transfers from host to disk and disk to device.
//!
//! ## Worker Threads
//! The offload manager uses two kinds of worker threads to handle the offloading and onboarding of blocks.
//!
//! The [`OffloadManager::offload_worker`] is responsible for offloading blocks.
//! The [`OffloadManager::onboard_worker`] is responsible for onboarding blocks.
//!
//! The kind of offloads/onboards they perform is dictated by the source and target arguments
//! of the [`OffloadManager::offload`] and [`OffloadManager::onboard`] methods.

use super::block::{BlockError, BlockMetadata, BlockState, ImmutableBlock};
use super::pool::BlockPoolError;
use super::state::TransferContext;
use super::storage::{Cuda, Storage};
use super::{BlockPool, DeviceStorage, DiskStorage, PinnedStorage};
use nixl_sys::Agent as NixlAgent;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::{
    mpsc::{self, error::TryRecvError},
    Mutex,
};

use anyhow::Result;
use std::any::Any;

use std::collections::BTreeSet;

mod pending;
pub mod request;

use pending::{
    CudaTransferManager, DiskTransferManager, PendingTransfer, TransferBatcher, TransferManager,
};
use request::{BlockResult, OffloadRequest, OffloadRequestKey, OnboardRequest};

const MAX_CONCURRENT_TRANSFERS: usize = 4;
const MAX_TRANSFER_BATCH_SIZE: usize = 16;

/// The offload manager handles all block transfers between different cache levels.
pub struct OffloadManager<Metadata: BlockMetadata> {
    // Handles to the device, host, and disk pools.
    disk: Option<Arc<BlockPool<DiskStorage, Metadata>>>,
    host: Option<Arc<BlockPool<PinnedStorage, Metadata>>>,
    device: Option<Arc<BlockPool<DeviceStorage, Metadata>>>,

    /// Queue of offloading requests.
    device_offload_tx: mpsc::UnboundedSender<OffloadRequest<DeviceStorage, Metadata>>,
    host_offload_tx: mpsc::UnboundedSender<OffloadRequest<PinnedStorage, Metadata>>,

    /// Queue of pending onboarding requests.
    host_onboard_tx: mpsc::UnboundedSender<OnboardRequest<PinnedStorage, DeviceStorage, Metadata>>,
    disk_onboard_tx: mpsc::UnboundedSender<OnboardRequest<DiskStorage, DeviceStorage, Metadata>>,

    /// An incrementing counter for offloaded blocks. Within the same priority, blocks with lower tick values are processed first.
    tick: Arc<Mutex<u64>>,
}

impl<Metadata: BlockMetadata> OffloadManager<Metadata> {
    pub fn new(
        disk: Option<Arc<BlockPool<DiskStorage, Metadata>>>,
        host: Option<Arc<BlockPool<PinnedStorage, Metadata>>>,
        device: Option<Arc<BlockPool<DeviceStorage, Metadata>>>,
        nixl_agent: Arc<Option<NixlAgent>>,
        async_rt_handle: Handle,
    ) -> Result<Arc<Self>> {
        let (device_offload_tx, device_offload_rx) = mpsc::unbounded_channel();
        let (host_offload_tx, host_offload_rx) = mpsc::unbounded_channel();

        let (host_onboard_tx, host_onboard_rx) = mpsc::unbounded_channel();
        let (disk_onboard_tx, disk_onboard_rx) = mpsc::unbounded_channel();

        let this = Arc::new(Self {
            disk,
            host,
            device,
            device_offload_tx,
            host_offload_tx,
            host_onboard_tx,
            disk_onboard_tx,
            tick: Arc::new(Mutex::new(0)),
        });

        let this_clone = this.clone();

        let cuda_ctx = Cuda::device_or_create(0)?;

        // We want cuda offloads to happen in parallel with host onboards, so we need to use a different stream.
        let device_offload_transfer_ctx = Arc::new(TransferContext::new(
            nixl_agent.clone(),
            cuda_ctx.new_stream()?,
        ));

        // Device -> Host offload
        let device_clone = this.device.clone();
        let host_clone = this.host.clone();
        async_rt_handle.spawn(async move {
            let res = OffloadManager::offload_worker(
                device_clone,
                host_clone,
                device_offload_rx,
                Arc::new(TransferBatcher::new(
                    CudaTransferManager::new(device_offload_transfer_ctx, MAX_CONCURRENT_TRANSFERS),
                    MAX_TRANSFER_BATCH_SIZE,
                )),
            )
            .await;
            tracing::warn!("Offload worker terminated: {:?}", res);
        });

        let transfer_ctx = Arc::new(TransferContext::new(
            nixl_agent.clone(),
            cuda_ctx.new_stream()?,
        ));

        // Host -> Disk offload
        let host_clone = this.host.clone();
        let disk_clone = this.disk.clone();
        let transfer_ctx_clone = transfer_ctx.clone();
        async_rt_handle.spawn(async move {
            let res = OffloadManager::offload_worker(
                host_clone,
                disk_clone,
                host_offload_rx,
                Arc::new(TransferBatcher::new(
                    DiskTransferManager::new(transfer_ctx_clone, MAX_CONCURRENT_TRANSFERS),
                    MAX_TRANSFER_BATCH_SIZE,
                )),
            )
            .await;
            tracing::warn!("Offload worker terminated: {:?}", res);
        });

        // Host -> Device onboarding
        let host_clone = this.host.clone();
        let device_clone = this.device.clone();
        let transfer_ctx_clone = transfer_ctx.clone();
        async_rt_handle.spawn(async move {
            let res = OffloadManager::onboard_worker(
                host_clone,
                device_clone,
                host_onboard_rx,
                Arc::new(TransferBatcher::new(
                    CudaTransferManager::new(transfer_ctx_clone, MAX_CONCURRENT_TRANSFERS),
                    MAX_TRANSFER_BATCH_SIZE,
                )),
            )
            .await;
            tracing::warn!("Onboard worker terminated: {:?}", res);
        });

        // Disk -> Device onboarding
        let disk_clone = this.disk.clone();
        let device_clone = this.device.clone();
        let transfer_ctx_clone = transfer_ctx.clone();
        async_rt_handle.spawn(async move {
            let res = OffloadManager::onboard_worker(
                disk_clone,
                device_clone,
                disk_onboard_rx,
                Arc::new(TransferBatcher::new(
                    DiskTransferManager::new(transfer_ctx_clone, MAX_CONCURRENT_TRANSFERS),
                    MAX_TRANSFER_BATCH_SIZE,
                )),
            )
            .await;
            tracing::warn!("Onboard worker terminated: {:?}", res);
        });

        Ok(this_clone)
    }

    async fn offload_worker<Source: Storage, Target: Storage>(
        source_pool: Option<Arc<BlockPool<Source, Metadata>>>,
        target_pool: Option<Arc<BlockPool<Target, Metadata>>>,
        mut offload_rx: mpsc::UnboundedReceiver<OffloadRequest<Source, Metadata>>,
        transfer_manager: Arc<dyn TransferManager<Source, Target, Metadata>>,
    ) -> Result<()> {
        if source_pool.is_none() || target_pool.is_none() {
            return Ok(());
        }

        let source_pool = source_pool.as_ref().unwrap();
        let target_pool = target_pool.as_ref().unwrap();

        let mut queue = BTreeSet::new();

        loop {
            // Try to check the offload queue.
            loop {
                match offload_rx.try_recv() {
                    Ok(request) => {
                        queue.insert(request);
                    }
                    Err(TryRecvError::Empty) => {
                        break;
                    }
                    Err(_) => return Ok(()),
                }
            }

            // If there is a request, process it.
            if let Some(request) = queue.pop_first() {
                // Try to upgrade the block to a strong reference.
                let block = match request.block.upgrade() {
                    Some(block) => Some(block),
                    // If unable to upgrade, the block may have been moved to the inactive pool.
                    None => source_pool
                        .match_sequence_hashes(vec![request.sequence_hash].as_slice())
                        .await?
                        .pop()
                        .map(|block| block.mutable_block().clone()),
                };

                // If we've found the block, offload it.
                if let Some(block) = block {
                    // If the block is already in the target, don't offload it.
                    if let Ok(blocks) = target_pool
                        .match_sequence_hashes_blocking(vec![request.sequence_hash].as_slice())
                    {
                        if !blocks.is_empty() {
                            continue;
                        }
                    }

                    let target_blocks = match target_pool.allocate_blocks(1).await {
                        Ok(blocks) => blocks,
                        Err(_) => {
                            tracing::warn!("Target pool full. Skipping offload. This should only ever happen with very small pool sizes.");
                            continue;
                        }
                    };

                    if let Some(target_block) = target_blocks.into_iter().next() {
                        transfer_manager
                            .enqueue_transfer(PendingTransfer::new(
                                vec![block],
                                vec![target_block],
                                None,
                                target_pool.clone(),
                            ))
                            .await?;
                    }
                }
            } else {
                // Await the next request.
                if let Some(request) = offload_rx.recv().await {
                    queue.insert(request);
                }
            }
        }
    }

    async fn onboard_worker<Source: Storage, Target: Storage>(
        source_pool: Option<Arc<BlockPool<Source, Metadata>>>,
        target_pool: Option<Arc<BlockPool<Target, Metadata>>>,
        mut onboard_rx: mpsc::UnboundedReceiver<OnboardRequest<Source, Target, Metadata>>,
        transfer_manager: Arc<dyn TransferManager<Source, Target, Metadata>>,
    ) -> Result<()> {
        if source_pool.is_none() || target_pool.is_none() {
            return Ok(());
        }

        let target_pool = target_pool.as_ref().unwrap();

        // Loop on incoming requests
        while let Some(request) = onboard_rx.recv().await {
            // Try to allocate blocks on the device.
            let target_blocks = match target_pool.allocate_blocks(request.blocks.len()).await {
                Ok(blocks) => blocks,
                Err(err) => {
                    request.response_tx.send(Err(err))?;
                    continue;
                }
            };

            let sources = request
                .blocks
                .iter()
                .map(|b| b.mutable_block().clone())
                .collect();

            transfer_manager
                .enqueue_transfer(PendingTransfer::new(
                    sources,
                    target_blocks,
                    Some(request.response_tx),
                    target_pool.clone(),
                ))
                .await?;
        }
        Ok(())
    }

    pub async fn offload<S: Storage>(
        &self,
        block: &ImmutableBlock<S, Metadata>,
        priority: u64,
    ) -> core::result::Result<(), BlockPoolError> {
        match block.state() {
            BlockState::Registered(_) => {}
            _ => {
                return Err(BlockPoolError::BlockError(BlockError::InvalidState(
                    "Block is not registered.".to_string(),
                )));
            }
        }

        let mut tick = self.tick.lock().await;
        let key = OffloadRequestKey {
            priority,
            timestamp: *tick,
        };
        // Increment a counter for each block. Within the same priority, blocks with lower counter values are processed first.
        *tick += 1;
        drop(tick);

        // This can get called by all pools, regardless of whether or not they have a place to offload to.
        // Because of this, we need to check the block type here.
        let any_block = block as &dyn Any;

        // TODO: What's the performance penalty of this runtime type-checking?
        if let Some(device_block) =
            any_block.downcast_ref::<ImmutableBlock<DeviceStorage, Metadata>>()
        {
            // The host pool doesn't exist, so we can't offload to it.
            if self.device_offload_tx.is_closed() {
                return Ok(());
            }

            let request = OffloadRequest {
                block: Arc::downgrade(device_block.mutable_block()),
                sequence_hash: device_block.sequence_hash()?,
                key,
            };

            self.device_offload_tx.send(request).unwrap();
        } else if let Some(host_block) =
            any_block.downcast_ref::<ImmutableBlock<PinnedStorage, Metadata>>()
        {
            // The disk pool doesn't exist, so we can't offload to it.
            if self.host_offload_tx.is_closed() {
                return Ok(());
            }

            let request = OffloadRequest {
                block: Arc::downgrade(host_block.mutable_block()),
                sequence_hash: host_block.sequence_hash()?,
                key,
            };

            self.host_offload_tx.send(request).unwrap();
        }

        Ok(())
    }

    pub async fn onboard<S: Storage>(
        &self,
        blocks: Vec<ImmutableBlock<S, Metadata>>,
    ) -> BlockResult<DeviceStorage, Metadata> {
        for block in &blocks {
            match block.state() {
                BlockState::Registered(_) => {}
                _ => {
                    return Err(BlockPoolError::BlockError(BlockError::InvalidState(
                        "Block is not registered.".to_string(),
                    )));
                }
            }
        }

        if blocks.is_empty() {
            return Ok(vec![]);
        }

        let (tx, rx) = oneshot::channel();

        let any_block = blocks.first().unwrap() as &dyn Any;

        // TODO: This is really ugly.
        if any_block
            .downcast_ref::<ImmutableBlock<PinnedStorage, Metadata>>()
            .is_some()
        {
            let host_blocks = blocks
                .iter()
                .map(|b| {
                    (b as &dyn Any)
                        .downcast_ref::<ImmutableBlock<PinnedStorage, Metadata>>()
                        .unwrap()
                        .clone()
                })
                .collect();

            self.host_onboard_tx
                .send(OnboardRequest::new(host_blocks, tx))
                .map_err(|_| BlockPoolError::ProgressEngineShutdown)?;
        } else if any_block
            .downcast_ref::<ImmutableBlock<DiskStorage, Metadata>>()
            .is_some()
        {
            let disk_blocks = blocks
                .iter()
                .map(|b| {
                    (b as &dyn Any)
                        .downcast_ref::<ImmutableBlock<DiskStorage, Metadata>>()
                        .unwrap()
                        .clone()
                })
                .collect();

            self.disk_onboard_tx
                .send(OnboardRequest::new(disk_blocks, tx))
                .map_err(|_| BlockPoolError::ProgressEngineShutdown)?;
        } else {
            return Err(BlockPoolError::BlockError(BlockError::Other(
                anyhow::anyhow!("Block type not supported for onboarding."),
            )));
        }

        match rx.await {
            Ok(res) => res,
            Err(_) => Err(BlockPoolError::ProgressEngineShutdown),
        }
    }
}

#[cfg(all(test, feature = "testing-cuda"))]
mod tests {
    use super::*;
    use crate::block_manager::block::test_utils::get_private_token;

    use crate::block_manager::{
        block::{
            nixl::BlockHandleInfo, BasicMetadata, BlockDataExt, BlockDataProvider, BlockExt,
            Blocks, MutableBlock,
        },
        layout::{nixl::NixlLayout, FullyContiguous},
        pool::BlockPool,
        storage::{
            DeviceAllocator, DeviceStorage, DiskAllocator, DiskStorage, PinnedAllocator,
            PinnedStorage, StorageType,
        },
        DType, LayoutConfig,
    };
    use nixl_sys::{MemoryRegion, NixlDescriptor};

    use aligned_vec::avec;
    use cudarc::runtime::sys::{cudaMemcpy, cudaMemcpyKind, cudaMemset};
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::mem::ManuallyDrop;
    use std::os::unix::io::FromRawFd;

    const BLOCK_SIZE: usize = 4;
    const NUM_LAYERS: usize = 8;

    type DevicePool = Option<Arc<BlockPool<DeviceStorage, BasicMetadata>>>;
    type HostPool = Option<Arc<BlockPool<PinnedStorage, BasicMetadata>>>;
    type DiskPool = Option<Arc<BlockPool<DiskStorage, BasicMetadata>>>;

    lazy_static::lazy_static! {
        static ref NIXL_AGENT: Arc<Option<NixlAgent>> = {
            let agent = NixlAgent::new("offload-manager").unwrap();
            let (_, ucx_params) = agent.get_plugin_params("UCX").unwrap();
            let (_, gds_params) = agent.get_plugin_params("GDS").unwrap();
            agent.create_backend("UCX", &ucx_params).unwrap();
            agent.create_backend("GDS", &gds_params).unwrap();
            Arc::new(Some(agent))
        };
    }

    fn build_pools(
        device_blocks: usize,
        host_blocks: Option<usize>,
        disk_blocks: Option<usize>,
        inner_dim: Option<usize>,
    ) -> Result<(
        Arc<OffloadManager<BasicMetadata>>,
        DevicePool,
        HostPool,
        DiskPool,
    )> {
        let mut config = LayoutConfig {
            num_blocks: device_blocks,
            num_layers: NUM_LAYERS,
            outer_dim: 1,
            page_size: BLOCK_SIZE,
            inner_dim: inner_dim.unwrap_or(1024),
            alignment: 1,
            dtype: DType::FP16,
        };

        let agent_arc = NIXL_AGENT.clone();
        let agent = agent_arc.as_ref().as_ref().unwrap();

        let mut device = FullyContiguous::allocate(config.clone(), &DeviceAllocator::default())?;

        device.nixl_register(agent, None)?;

        let device_blocks = Blocks::<_, BasicMetadata>::new(device, 42, 0)?.into_blocks()?;
        let device_pool = Some(Arc::new(
            BlockPool::builder().blocks(device_blocks).build()?,
        ));

        let host_pool = if let Some(host_blocks) = host_blocks {
            config.num_blocks = host_blocks;
            let mut host = FullyContiguous::allocate(config.clone(), &PinnedAllocator::default())?;
            host.nixl_register(agent, None)?;
            let host_blocks = Blocks::<_, BasicMetadata>::new(host, 42, 0)?.into_blocks()?;
            Some(Arc::new(BlockPool::builder().blocks(host_blocks).build()?))
        } else {
            None
        };

        let disk_pool = if let Some(disk_blocks) = disk_blocks {
            config.num_blocks = disk_blocks;
            let mut disk = FullyContiguous::allocate(config, &DiskAllocator)?;
            disk.nixl_register(agent, None)?;
            let disk_blocks = Blocks::<_, BasicMetadata>::new(disk, 42, 0)?.into_blocks()?;
            Some(Arc::new(BlockPool::builder().blocks(disk_blocks).build()?))
        } else {
            None
        };

        let async_rt_handle = Handle::current();

        let manager = OffloadManager::new(
            disk_pool.clone(),
            host_pool.clone(),
            device_pool.clone(),
            agent_arc,
            async_rt_handle,
        )?;

        Ok((manager, device_pool, host_pool, disk_pool))
    }

    /// Create a block in the 'RESET' state.
    async fn get_block<S: Storage, Metadata: BlockMetadata>(
        pool: &Arc<BlockPool<S, Metadata>>,
    ) -> Result<MutableBlock<S, Metadata>> {
        pool.allocate_blocks(1)
            .await?
            .into_iter()
            .next()
            .ok_or(anyhow::anyhow!("Failed to allocate block"))
    }

    /// Create a block in the 'PARTIAL' state.
    async fn partial_block<S: Storage, Metadata: BlockMetadata>(
        pool: &Arc<BlockPool<S, Metadata>>,
        token: u32,
    ) -> Result<MutableBlock<S, Metadata>> {
        let mut block = get_block(pool).await?;
        block.init_sequence(42)?;
        block.add_token(token)?;
        Ok(block)
    }

    /// Create a block in the 'COMPLETED' state.
    async fn completed_block<S: Storage, Metadata: BlockMetadata>(
        pool: &Arc<BlockPool<S, Metadata>>,
        tokens: [u32; BLOCK_SIZE],
    ) -> Result<MutableBlock<S, Metadata>> {
        let mut block = get_block(pool).await?;
        block.init_sequence(42)?;
        for token in tokens {
            block.add_token(token)?;
        }
        block.commit()?;
        Ok(block)
    }

    fn populate_block<S: Storage + NixlDescriptor>(
        block: &impl BlockDataProvider<StorageType = S>,
        value: u8,
    ) -> Result<()> {
        let block_data = block.block_data(get_private_token());
        let block_view = block_data.block_view()?;
        let block_size = block_view.size();

        match block_data.storage_type() {
            StorageType::Device(_) | StorageType::Pinned => unsafe {
                cudaMemset(
                    block_view.as_ptr() as *mut std::ffi::c_void,
                    value as i32,
                    block_size,
                )
                .result()?;
            },
            StorageType::Disk => {
                let nixl_desc = block_view.as_nixl_descriptor();
                let mut file: ManuallyDrop<File>;
                let data = avec![[4096] | value; block_size];

                unsafe {
                    file = ManuallyDrop::new(File::from_raw_fd(nixl_desc.device_id() as i32));
                    file.seek(SeekFrom::Start(nixl_desc.as_ptr() as u64))?;
                }
                file.write_all(&data)?;
                file.sync_all()?;
                file.flush()?;
            }
            _ => panic!(),
        }

        Ok(())
    }

    fn get_block_contents<S: Storage + NixlDescriptor>(
        block: &impl BlockDataProvider<StorageType = S>,
    ) -> Result<Vec<u8>> {
        let block_data = block.block_data(get_private_token());
        let block_view = block_data.block_view()?;
        let size = block_view.size();

        let mut contents: Vec<u8> = vec![0; size];

        match block_data.storage_type() {
            StorageType::Device(_) => unsafe {
                cudaMemcpy(
                    contents.as_mut_ptr() as *mut std::ffi::c_void,
                    block_view.as_ptr() as *const std::ffi::c_void,
                    size,
                    cudaMemcpyKind::cudaMemcpyDeviceToHost,
                )
                .result()?;
            },
            StorageType::Pinned => unsafe {
                contents = std::slice::from_raw_parts(block_view.as_ptr(), size).to_vec();
            },
            StorageType::Disk => {
                let nixl_desc = block_view.as_nixl_descriptor();
                let mut file: ManuallyDrop<File>;
                let mut aligned = avec![[4096] | 0; size];

                unsafe {
                    file = ManuallyDrop::new(File::from_raw_fd(nixl_desc.device_id() as i32));
                    file.seek(SeekFrom::Start(nixl_desc.as_ptr() as u64))?;
                }
                file.read_exact(&mut aligned)?;
                contents = aligned.to_vec();
            }
            _ => anyhow::bail!("Unsupported storage type."),
        }

        Ok(contents.to_vec())
    }

    fn check_block_contents(
        block1: &impl BlockDataProvider<StorageType = impl Storage + NixlDescriptor>,
        block2: &impl BlockDataProvider<StorageType = impl Storage + NixlDescriptor>,
        value: u8,
    ) -> Result<()> {
        let contents1 = get_block_contents(block1)?;
        let contents2 = get_block_contents(block2)?;

        for (c1_value, c2_value) in contents1.iter().zip(contents2.iter()) {
            if *c1_value != *c2_value || *c1_value != value {
                panic!("{} != {} != {}", c1_value, c2_value, value);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_offload_invalid_blocks() -> Result<()> {
        let (offload_manager, device_pool, _, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();

        // Check blocks in the 'RESET' state.
        let immutable_block = ImmutableBlock::new(Arc::new(get_block(device_pool).await?));

        assert!(matches!(
            offload_manager.offload(&immutable_block, 0).await,
            Err(BlockPoolError::BlockError(BlockError::InvalidState(_)))
        ));

        // Check blocks in the 'PARTIAL' state.
        let immutable_block = ImmutableBlock::new(Arc::new(partial_block(device_pool, 0).await?));
        assert!(matches!(
            offload_manager.offload(&immutable_block, 0).await,
            Err(BlockPoolError::BlockError(BlockError::InvalidState(_)))
        ));

        // Check blocks in the 'COMPLETED' state.
        let immutable_block = ImmutableBlock::new(Arc::new(
            completed_block(device_pool, [0; BLOCK_SIZE]).await?,
        ));
        assert!(matches!(
            offload_manager.offload(&immutable_block, 0).await,
            Err(BlockPoolError::BlockError(BlockError::InvalidState(_)))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn test_offload_registered_blocks() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        // Create a block and register it with the offload manager
        let block = completed_block(device_pool, [0, 1, 2, 3]).await?;

        let immutable_device_block = device_pool
            .register_blocks(vec![block])
            .await?
            .into_iter()
            .next()
            .ok_or(anyhow::anyhow!("Failed to register block"))?;

        populate_block(&immutable_device_block, 42)?;

        // Offloads should only go to G2 (for now)
        offload_manager.offload(&immutable_device_block, 0).await?;

        // Wait for it to be processed.
        // TODO: This is a bit of a hack, and may lead to non-deterministic behavior.
        // In theory, the offload + memcpy should take much less time than this.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check that the block exists in the host pool
        let host_blocks = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;

        assert_eq!(host_blocks.len(), 1);
        assert_eq!(
            host_blocks[0].sequence_hash()?,
            immutable_device_block.sequence_hash()?
        );

        check_block_contents(&immutable_device_block, &host_blocks[0], 42)?;

        Ok(())
    }

    #[tokio::test]
    async fn test_no_host_blocks_available() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        let host_blocks = host_pool.allocate_blocks(4).await?;
        assert_eq!(host_blocks.len(), 4);

        let device_block = completed_block(device_pool, [0, 1, 2, 3]).await?;
        let immutable_device_block = device_pool
            .register_blocks(vec![device_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        offload_manager.offload(&immutable_device_block, 0).await?;

        // Wait for offload to be processed.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The offload should fail gracefuly due to a lack of host blocks
        let matched_host_blocks = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(matched_host_blocks.len(), 0);

        // Wait for blocks to be returned to the pool.
        drop(host_blocks);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Try the offload again.
        offload_manager.offload(&immutable_device_block, 0).await?;

        // Wait for offload to be processed.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // This time, the offload should succeed.
        let matched_host_blocks = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(matched_host_blocks.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_onboard() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        // Allocate and fill a block on the host.
        let host_block = completed_block(host_pool, [0, 1, 2, 3]).await?;
        let immutable_host_block = host_pool
            .register_blocks(vec![host_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        populate_block(&immutable_host_block, 42)?;

        // Onboard the block.
        let onboarded_blocks = offload_manager
            .onboard(vec![immutable_host_block.clone()])
            .await?;

        assert_eq!(onboarded_blocks.len(), 1);
        // Check that the sequence hash is the same.
        assert_eq!(
            onboarded_blocks[0].sequence_hash()?,
            immutable_host_block.sequence_hash()?
        );
        // Check that the block is registered.
        assert!(matches!(
            onboarded_blocks[0].state(),
            BlockState::Registered(_)
        ));

        check_block_contents(&immutable_host_block, &onboarded_blocks[0], 42)?;

        // Wait for the new value to show up in the device pool.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let device_blocks = device_pool
            .match_sequence_hashes(vec![onboarded_blocks[0].sequence_hash()?].as_slice())
            .await?;
        assert_eq!(device_blocks.len(), 1);
        assert_eq!(
            device_blocks[0].sequence_hash()?,
            onboarded_blocks[0].sequence_hash()?
        );

        // Check that this is the same block.
        check_block_contents(&immutable_host_block, &device_blocks[0], 42)?;

        Ok(())
    }

    #[tokio::test]
    async fn test_offload_onboard() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        let device_block = completed_block(device_pool, [0, 1, 2, 3]).await?;
        let immutable_device_block = device_pool
            .register_blocks(vec![device_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        populate_block(&immutable_device_block, 42)?;
        // Offload the block to the host.
        offload_manager.offload(&immutable_device_block, 0).await?;

        // Wait for the offload to be processed.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check that the block exists in the host pool.
        let immutable_host_block = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?
            .into_iter()
            .next()
            .unwrap();

        check_block_contents(&immutable_device_block, &immutable_host_block, 42)?;

        // Remove the device block from the pool by dropping it and allocating more blocks.
        drop(immutable_device_block);

        // Wait for the block to be returned to the pool.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let device_blocks = device_pool.allocate_blocks(4).await?;
        assert_eq!(device_blocks.len(), 4);

        drop(device_blocks);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check that the block is not in the device pool.
        let device_blocks = device_pool
            .match_sequence_hashes(vec![immutable_host_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(device_blocks.len(), 0);

        // Onboard the block back to the device pool.
        let onboarded_blocks = offload_manager
            .onboard(vec![immutable_host_block.clone()])
            .await?;
        assert_eq!(onboarded_blocks.len(), 1);
        assert_eq!(
            onboarded_blocks[0].sequence_hash()?,
            immutable_host_block.sequence_hash()?
        );
        assert!(matches!(
            onboarded_blocks[0].state(),
            BlockState::Registered(_)
        ));

        check_block_contents(&immutable_host_block, &onboarded_blocks[0], 42)?;

        Ok(())
    }

    #[tokio::test]
    async fn test_onboard_err_handling() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        let host_block = completed_block(host_pool, [0, 1, 2, 3]).await?;
        let immutable_host_block = host_pool
            .register_blocks(vec![host_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        let device_blocks = device_pool.allocate_blocks(4).await?;
        assert_eq!(device_blocks.len(), 4);

        let res = offload_manager
            .onboard(vec![immutable_host_block.clone()])
            .await;
        assert!(matches!(
            res.err().unwrap(),
            BlockPoolError::NotEnoughBlocksAvailable(_, _)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn test_offload_onboard_no_host_blocks() -> Result<()> {
        let (offload_manager, device_pool, _, _) = build_pools(4, None, None, None)?;

        let device_pool = device_pool.as_ref().unwrap();

        let device_block = completed_block(device_pool, [0, 1, 2, 3]).await?;
        let immutable_device_block = device_pool
            .register_blocks(vec![device_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        offload_manager.offload(&immutable_device_block, 0).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_offload_disk() -> Result<()> {
        let (offload_manager, _, host_pool, disk_pool) = build_pools(4, Some(4), Some(4), None)?;

        let host_pool = host_pool.as_ref().unwrap();
        let disk_pool = disk_pool.as_ref().unwrap();

        let host_block = completed_block(host_pool, [0, 1, 2, 3]).await?;
        let immutable_host_block = host_pool
            .register_blocks(vec![host_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        populate_block(&immutable_host_block, 42)?;

        offload_manager.offload(&immutable_host_block, 0).await?;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let disk_blocks = disk_pool
            .match_sequence_hashes(vec![immutable_host_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(disk_blocks.len(), 1);
        assert_eq!(
            disk_blocks[0].sequence_hash()?,
            immutable_host_block.sequence_hash()?
        );

        check_block_contents(&immutable_host_block, &disk_blocks[0], 42)?;

        Ok(())
    }

    #[tokio::test]
    async fn test_onboard_disk() -> Result<()> {
        let (offload_manager, device_pool, _, disk_pool) = build_pools(4, None, Some(4), None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let disk_pool = disk_pool.as_ref().unwrap();

        let disk_block = completed_block(disk_pool, [0, 1, 2, 3]).await?;
        let immutable_disk_block = disk_pool
            .register_blocks(vec![disk_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        populate_block(&immutable_disk_block, 42)?;

        let device_block = offload_manager
            .onboard(vec![immutable_disk_block.clone()])
            .await?;

        check_block_contents(&immutable_disk_block, &device_block[0], 42)?;

        assert_eq!(device_block.len(), 1);
        assert_eq!(
            device_block[0].sequence_hash()?,
            immutable_disk_block.sequence_hash()?
        );
        assert_eq!(
            device_pool
                .match_sequence_hashes(vec![immutable_disk_block.sequence_hash()?].as_slice())
                .await?
                .len(),
            1
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_bulk_transfer_disk() -> Result<()> {
        let (offload_manager, device_pool, host_pool, disk_pool) =
            build_pools(8, Some(8), Some(8), None)?;

        let disk_pool = disk_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();
        let device_pool = device_pool.as_ref().unwrap();

        let mut host_blocks = Vec::new();

        for i in 0..8 {
            let block = completed_block(host_pool, [i; 4]).await?;
            populate_block(&block, i as u8)?;
            host_blocks.push(block);
        }

        let immutable_host_blocks = host_pool.register_blocks(host_blocks).await?;

        for block in &immutable_host_blocks {
            offload_manager.offload(block, 0).await?;
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let mut disk_blocks = Vec::new();

        for (i, host_block) in immutable_host_blocks.iter().enumerate() {
            let blocks = disk_pool
                .match_sequence_hashes(vec![host_block.sequence_hash()?].as_slice())
                .await?;
            assert_eq!(blocks.len(), 1);
            check_block_contents(host_block, &blocks[0], i as u8)?;
            disk_blocks.push(blocks[0].clone());
        }

        let device_blocks = offload_manager.onboard(disk_blocks.clone()).await?;
        assert_eq!(device_blocks.len(), disk_blocks.len());

        for (i, disk_block) in disk_blocks.iter().enumerate() {
            let blocks = device_pool
                .match_sequence_hashes(vec![disk_block.sequence_hash()?].as_slice())
                .await?;
            assert_eq!(blocks.len(), 1);
            check_block_contents(disk_block, &blocks[0], i as u8)?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_transfer_batcher() -> Result<()> {
        let (offload_manager, device_pool, _, disk_pool) = build_pools(
            2 * MAX_TRANSFER_BATCH_SIZE + 1,
            None,
            Some(2 * MAX_TRANSFER_BATCH_SIZE + 1),
            None,
        )?;

        let device_pool = device_pool.as_ref().unwrap();
        let disk_pool = disk_pool.as_ref().unwrap();

        let mut disk_blocks = Vec::new();

        for i in 0..2 * MAX_TRANSFER_BATCH_SIZE + 1 {
            let disk_block = completed_block(disk_pool, [i as u32; 4]).await?;
            populate_block(&disk_block, i as u8)?;
            disk_blocks.push(disk_block);
        }

        let immutable_disk_blocks = disk_pool.register_blocks(disk_blocks).await?;

        let device_blocks = offload_manager
            .onboard(immutable_disk_blocks.clone())
            .await?;
        assert_eq!(device_blocks.len(), 2 * MAX_TRANSFER_BATCH_SIZE + 1);

        for (i, device_block) in device_blocks.iter().enumerate() {
            let blocks = device_pool
                .match_sequence_hashes(vec![device_block.sequence_hash()?].as_slice())
                .await?;
            check_block_contents(device_block, &blocks[0], i as u8)?;
            assert_eq!(blocks.len(), 1);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_onboard_unsupported_block_type() -> Result<()> {
        let (offload_manager, device_pool, _, _) = build_pools(1, None, None, None)?;

        let device_pool = device_pool.as_ref().unwrap();

        let block = completed_block(device_pool, [0; 4]).await?;

        let registered_block = device_pool
            .register_blocks(vec![block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        let onboarded_blocks = offload_manager.onboard(vec![registered_block]).await;
        assert!(matches!(
            onboarded_blocks,
            Err(BlockPoolError::BlockError(BlockError::Other(_)))
        ));

        Ok(())
    }

    #[tokio::test]
    async fn test_offload_transfer_metadata() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        let mut device_block = completed_block(device_pool, [0; 4]).await?;

        populate_block(&device_block, 42)?;

        let new_metadata = device_block.metadata().update_priority(1);
        device_block.update_metadata(new_metadata);

        let immutable_device_block = device_pool
            .register_blocks(vec![device_block])
            .await?
            .into_iter()
            .next()
            .unwrap();
        offload_manager.offload(&immutable_device_block, 0).await?;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let host_blocks = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(host_blocks.len(), 1);
        check_block_contents(&immutable_device_block, &host_blocks[0], 42)?;
        assert_eq!(host_blocks[0].metadata().priority(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_onboard_duplicate() -> Result<()> {
        let (offload_manager, device_pool, host_pool, _) = build_pools(4, Some(4), None, None)?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();

        let device_block = completed_block(device_pool, [0; 4]).await?;

        let immutable_device_block = device_pool
            .register_blocks(vec![device_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        populate_block(&immutable_device_block, 42)?;

        offload_manager.offload(&immutable_device_block, 0).await?;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let host_blocks = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(host_blocks.len(), 1);

        let onboarded_blocks = offload_manager
            .onboard(vec![host_blocks[0].clone()])
            .await?;
        assert_eq!(onboarded_blocks.len(), 1);
        check_block_contents(&host_blocks[0], &onboarded_blocks[0], 42)?;

        // This should be the same block that we put on the device.
        // The block that was copied should be discarded by the block pool.
        assert_eq!(
            onboarded_blocks[0].block_idx(),
            immutable_device_block.block_idx()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_transfer_big_blocks() -> Result<()> {
        // Try a block size of 32 MB.
        let inner_dim = 2_usize.pow(20) * 32 / NUM_LAYERS / BLOCK_SIZE;
        let (offload_manager, device_pool, host_pool, disk_pool) =
            build_pools(2, Some(2), Some(2), Some(inner_dim))?;

        let device_pool = device_pool.as_ref().unwrap();
        let host_pool = host_pool.as_ref().unwrap();
        let disk_pool = disk_pool.as_ref().unwrap();

        let device_block = completed_block(device_pool, [0; 4]).await?;

        populate_block(&device_block, 42)?;

        let immutable_device_block = device_pool
            .register_blocks(vec![device_block])
            .await?
            .into_iter()
            .next()
            .unwrap();

        // Offload to host.
        offload_manager.offload(&immutable_device_block, 0).await?;

        // Wait for the offload to be processed.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let host_blocks = host_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(host_blocks.len(), 1);
        check_block_contents(&immutable_device_block, &host_blocks[0], 42)?;

        // Offload to disk
        offload_manager.offload(&host_blocks[0], 0).await?;

        // Wait for the offload to be processed.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let disk_blocks = disk_pool
            .match_sequence_hashes(vec![immutable_device_block.sequence_hash()?].as_slice())
            .await?;
        assert_eq!(disk_blocks.len(), 1);
        check_block_contents(&host_blocks[0], &disk_blocks[0], 42)?;

        // Onboard to device.
        let device_blocks = offload_manager.onboard(disk_blocks.clone()).await?;
        assert_eq!(device_blocks.len(), 1);
        check_block_contents(&disk_blocks[0], &device_blocks[0], 42)?;

        Ok(())
    }
}
