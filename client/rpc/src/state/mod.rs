// This file is part of Substrate.

// Copyright (C) 2017-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Substrate state API.

mod state_full;
mod state_light;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::marker::PhantomData;

use crate::SubscriptionTaskExecutor;

use futures::{future, StreamExt};
use jsonrpsee_types::error::{Error as JsonRpseeError, CallError as JsonRpseeCallError};
use jsonrpsee_ws_server::{RpcModule, SubscriptionSink};
use futures::FutureExt;

use sc_rpc_api::{DenyUnsafe, state::ReadProof};
use sc_client_api::light::{RemoteBlockchain, Fetcher};
use sp_core::{Bytes, storage::{PrefixedStorageKey, StorageChangeSet, StorageData, StorageKey, well_known_keys}};
use sp_version::RuntimeVersion;
use sp_runtime::{generic::BlockId, traits::Block as BlockT};

use sp_api::{Metadata, ProvideRuntimeApi, CallApiAt};

use self::error::Error;

pub use sc_rpc_api::state::*;
pub use sc_rpc_api::child_state::*;
use sc_client_api::{
	ExecutorProvider, StorageProvider, BlockchainEvents, Backend, BlockBackend, ProofProvider
};
use sp_blockchain::{HeaderMetadata, HeaderBackend};

const STORAGE_KEYS_PAGED_MAX_COUNT: u32 = 1000;

/// State backend API.
#[async_trait::async_trait]
pub trait StateBackend<Block: BlockT, Client>: Send + Sync + 'static
	where
		Block: BlockT + 'static,
		Client: Send + Sync + 'static,
{
	/// Call runtime method at given block.
	async fn call(
		&self,
		block: Option<Block::Hash>,
		method: String,
		call_data: Bytes,
	) -> Result<Bytes, Error>;

	/// Returns the keys with prefix, leave empty to get all the keys.
	async fn storage_keys(
		&self,
		block: Option<Block::Hash>,
		prefix: StorageKey,
	) -> Result<Vec<StorageKey>, Error>;

	/// Returns the keys with prefix along with their values, leave empty to get all the pairs.
	async fn storage_pairs(
		&self,
		block: Option<Block::Hash>,
		prefix: StorageKey,
	) -> Result<Vec<(StorageKey, StorageData)>, Error>;

	/// Returns the keys with prefix with pagination support.
	async fn storage_keys_paged(
		&self,
		block: Option<Block::Hash>,
		prefix: Option<StorageKey>,
		count: u32,
		start_key: Option<StorageKey>,
	) -> Result<Vec<StorageKey>, Error>;

	/// Returns a storage entry at a specific block's state.
	async fn storage(
		&self,
		block: Option<Block::Hash>,
		key: StorageKey,
	) -> Result<Option<StorageData>, Error>;

	/// Returns the hash of a storage entry at a block's state.
	async fn storage_hash(
		&self,
		block: Option<Block::Hash>,
		key: StorageKey,
	) -> Result<Option<Block::Hash>, Error>;

	/// Returns the size of a storage entry at a block's state.
	///
	/// If data is available at `key`, it is returned. Else, the sum of values who's key has `key`
	/// prefix is returned, i.e. all the storage (double) maps that have this prefix.
	async fn storage_size(
		&self,
		block: Option<Block::Hash>,
		key: StorageKey,
	) -> Result<Option<u64>, Error>;

	/// Returns the runtime metadata as an opaque blob.
	async fn metadata(&self, block: Option<Block::Hash>) -> Result<Bytes, Error>;

	/// Get the runtime version.
	async fn runtime_version(&self, block: Option<Block::Hash>) -> Result<RuntimeVersion, Error>;

	/// Query historical storage entries (by key) starting from a block given as the second parameter.
	///
	/// NOTE This first returned result contains the initial state of storage for all keys.
	/// Subsequent values in the vector represent changes to the previous state (diffs).
	async fn query_storage(
		&self,
		from: Block::Hash,
		to: Option<Block::Hash>,
		keys: Vec<StorageKey>,
	) -> Result<Vec<StorageChangeSet<Block::Hash>>, Error>;

	/// Query storage entries (by key) starting at block hash given as the second parameter.
	async fn query_storage_at(
		&self,
		keys: Vec<StorageKey>,
		at: Option<Block::Hash>
	) -> Result<Vec<StorageChangeSet<Block::Hash>>, Error>;

	/// Returns proof of storage entries at a specific block's state.
	async fn read_proof(
		&self,
		block: Option<Block::Hash>,
		keys: Vec<StorageKey>,
	) -> Result<ReadProof<Block::Hash>, Error>;

	/// Trace storage changes for block
	async fn trace_block(
		&self,
		block: Block::Hash,
		targets: Option<String>,
		storage_keys: Option<String>,
	) -> Result<sp_rpc::tracing::TraceBlockResponse, Error>;
}

/// Create new state API that works on full node.
pub fn new_full<BE, Block: BlockT, Client>(
	client: Arc<Client>,
	executor: Arc<SubscriptionTaskExecutor>,
	deny_unsafe: DenyUnsafe,
) -> (State<Block, Client>, ChildState<Block, Client>)
	where
		Block: BlockT + 'static,
		BE: Backend<Block> + 'static,
		Client: ExecutorProvider<Block> + StorageProvider<Block, BE> + ProofProvider<Block>
			+ HeaderMetadata<Block, Error = sp_blockchain::Error> + BlockchainEvents<Block>
			+ CallApiAt<Block> + HeaderBackend<Block>
			+ BlockBackend<Block> + ProvideRuntimeApi<Block> + Send + Sync + 'static,
		Client::Api: Metadata<Block>,
{
	let child_backend = Box::new(
		self::state_full::FullState::new(client.clone())
	);
	let backend = Arc::new(self::state_full::FullState::new(client.clone()));
	(
		State { backend, client, executor, deny_unsafe },
		ChildState { backend: child_backend }
	)
}

/// Create new state API that works on light node.
pub fn new_light<BE, Block: BlockT, Client, F: Fetcher<Block>>(
	client: Arc<Client>,
	remote_blockchain: Arc<dyn RemoteBlockchain<Block>>,
	fetcher: Arc<F>,
	executor: Arc<SubscriptionTaskExecutor>,
	deny_unsafe: DenyUnsafe,
) -> (State<Block, Client>, ChildState<Block, Client>)
	where
		Block: BlockT + 'static,
		BE: Backend<Block> + 'static,
		Client: ExecutorProvider<Block> + StorageProvider<Block, BE>
			+ HeaderMetadata<Block, Error = sp_blockchain::Error>
			+ ProvideRuntimeApi<Block> + HeaderBackend<Block> + BlockchainEvents<Block>
			+ Send + Sync + 'static,
		F: Send + Sync + 'static,
{
	let child_backend = Box::new(self::state_light::LightState::new(
			client.clone(),
			remote_blockchain.clone(),
			fetcher.clone(),
	));

	let backend = Arc::new(self::state_light::LightState::new(
			client.clone(),
			remote_blockchain,
			fetcher,
	));
	(
		State { backend, client, executor, deny_unsafe },
		ChildState { backend: child_backend }
	)
}

/// State API with subscriptions support.
pub struct State<Block, Client> {
	backend: Arc<dyn StateBackend<Block, Client>>,
	executor: Arc<SubscriptionTaskExecutor>,
	// TODO: this is pretty dumb. the `FullState` struct has a `client` in it, but I don't know how to get a
	// reference to it. I could impl `ChainBackend` which has a `client()` method, but that's pretty lame. I could
	// also add a `client()` method to the `StateBackend` trait but that's also terrible.
	client: Arc<Client>,
	/// Whether to deny unsafe calls
	deny_unsafe: DenyUnsafe,
}

impl<Block, Client> State<Block, Client>
	where
		Block: BlockT + 'static,
		Client: BlockchainEvents<Block> + CallApiAt<Block> + HeaderBackend<Block>
			 + Send + Sync + 'static,
{
	/// Convert this to a RPC module.
	pub fn into_rpc_module(self) -> Result<RpcModule<Self>, JsonRpseeError> {
		let mut module = RpcModule::new(self);

		module.register_async_method("state_call", |params, state| {
			let (method, data, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};

			async move {
				state.backend.call(block, method, data).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getKeys", |params, state| {
			let (key_prefix, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage_keys(block, key_prefix).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getPairs", |params, state| {
			let (key_prefix, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.deny_unsafe.check_if_safe()?;
				state.backend.storage_pairs(block, key_prefix).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getKeysPaged", |params, state| {
			let (prefix, count, start_key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				if count > STORAGE_KEYS_PAGED_MAX_COUNT {
					return Err(JsonRpseeCallError::Failed(Box::new(Error::InvalidCount {
							value: count,
							max: STORAGE_KEYS_PAGED_MAX_COUNT,
						})
					));
				}
				state.backend.storage_keys_paged(block, prefix, count,start_key)
					.await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getStorage", |params, state| {
			let (key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage(block, key).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getStorageHash", |params, state| {
			let (key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage(block, key).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getStorageSize", |params, state| {
			let (key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage_size(block, key).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getMetadata", |params, state| {
			let maybe_block = params.one().ok();
			async move {
				state.backend.metadata(maybe_block).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getRuntimeVersion", |params, state| {
			let at = params.one().ok();
			async move {
				state.deny_unsafe.check_if_safe()?;
				state.backend.runtime_version(at).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_queryStorage", |params, state| {
			let (keys, from, to) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.deny_unsafe.check_if_safe()?;
				state.backend.query_storage(from, to, keys).await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_queryStorageAt", |params, state| {
			let (keys, at) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.deny_unsafe.check_if_safe()?;
				state.backend.query_storage_at(keys, at).await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_getReadProof", |params, state| {
			let (keys, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.deny_unsafe.check_if_safe()?;
				state.backend.read_proof(block, keys).await.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_async_method("state_traceBlock", |params, state| {
			let (block, targets, storage_keys) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.deny_unsafe.check_if_safe()?;
				state.backend.trace_block(block, targets, storage_keys).await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		module.register_subscription(
			"state_subscribeRuntimeVersion",
			"state_unsubscribeRuntimeVersion",
			|_params, mut sink, ctx| {
				let executor = ctx.executor.clone();
				let client = ctx.client.clone();

				let mut previous_version = client.runtime_version_at(&BlockId::hash(client.info().best_hash))
					.expect("best hash is valid; qed");
				let _ = sink.send(&previous_version);
				let rt_version_stream = client.storage_changes_notification_stream(Some(&[StorageKey(well_known_keys::CODE.to_vec())]), None, )
					.map_err(|blockchain_err| Error::Client(Box::new(blockchain_err)))
					.map_err(to_jsonrpsee_call_error)?;

				let fut = async move {
                    rt_version_stream
                        .filter_map(|_| {
                            let info = client.info();
                            let version = client
                                .runtime_version_at(&BlockId::hash(info.best_hash));
                                match version {
                                    Ok(v) => if previous_version != v {
                                            previous_version = v.clone();
                                            future::ready(Some(v))
                                        } else {
                                            future::ready(None)
                                        },
                                    Err(e) => {
                                        log::error!("Could not fetch current runtime version. Error={:?}", e);
                                        future::ready(None)
                                    }
                                }
                        })
                        .take_while(|version| {
							future::ready(
								sink.send(&version).map_or_else(|e| {
									log::error!("Could not send data to the state_subscribeRuntimeVersion subscriber: {:?}", e);
									false
								}, |_| true)
							)

                        })
                        .for_each(|_| future::ready(()))
                        .await;
                }.boxed();
				executor.execute_new(fut);
				Ok(())
		})?;

		module.register_subscription(
			"state_subscribeStorage",
			"state_unsubscribeStorage",
			|params, mut sink, ctx| {
				let executor = ctx.executor.clone();
				let backend = ctx.backend.clone();
				let keys = params.one::<Option<Vec<StorageKey>>>()?;

				let initial = {
					let block = ctx.client.info().best_hash;
					let changes: Vec<(StorageKey, Option<StorageData>)> = keys.as_ref().map(|keys| {
						keys
							.iter()
							.map(|storage_key| {
								futures::executor::block_on(
									StateBackend::storage(&*backend, Some(block.clone()).into(), storage_key.clone())
										.map(|val| (storage_key.clone(), val.unwrap_or(None)))
								)
							})
							.collect()
					}).unwrap_or_default();
					vec![StorageChangeSet { block, changes }]
				};
				sink.send(&initial)?;

				let stream = ctx.client.storage_changes_notification_stream(
					keys.as_ref().map(|keys| &**keys),
					None
					)
					.map_err(|blockchain_err| Error::Client(Box::new(blockchain_err)))
					.map_err(to_jsonrpsee_call_error)?;

				let fut = async move {
					stream.map(|(block, changes)| {
						StorageChangeSet {
							block,
							changes: changes
								.iter()
								.filter_map(|(o_sk, k, v)| {
									// Note: the first `Option<&StorageKey>` seems to be the parent key, so it's set only
									// for storage events stemming from child storage, `None` otherwise. This RPC only
									// returns non-child storage.
									if o_sk.is_none() {
										Some((k.clone(), v.cloned()))
									} else {
										None
									}
								}).collect(),
						}
					})
        			.take_while(|changes| {
						future::ready(
							sink.send(&changes).map_or_else(|e| {
								log::error!("Could not send data to the state_subscribeStorage subscriber: {:?}", e);
								false
							}, |_| true)
						)
					})
        			.for_each(|_| future::ready(()))
					.await;
				}.boxed();

				executor.execute_new(fut);
				Ok(())
		})?;

		Ok(module)
	}
}

/// Child state backend API.
#[async_trait::async_trait]
pub trait ChildStateBackend<Block: BlockT, Client>: Send + Sync + 'static
	where
		Block: BlockT + 'static,
		Client: Send + Sync + 'static,
{
	/// Returns proof of storage for a child key entries at a specific block's state.
	async fn read_child_proof(
		&self,
		block: Option<Block::Hash>,
		storage_key: PrefixedStorageKey,
		keys: Vec<StorageKey>,
	) -> Result<ReadProof<Block::Hash>, Error>;

	/// Returns the keys with prefix from a child storage,
	/// leave prefix empty to get all the keys.
	async fn storage_keys(
		&self,
		block: Option<Block::Hash>,
		storage_key: PrefixedStorageKey,
		prefix: StorageKey,
	) -> Result<Vec<StorageKey>, Error>;

	/// Returns a child storage entry at a specific block's state.
	async fn storage(
		&self,
		block: Option<Block::Hash>,
		storage_key: PrefixedStorageKey,
		key: StorageKey,
	) -> Result<Option<StorageData>, Error>;

	/// Returns the hash of a child storage entry at a block's state.
	async fn storage_hash(
		&self,
		block: Option<Block::Hash>,
		storage_key: PrefixedStorageKey,
		key: StorageKey,
	) -> Result<Option<Block::Hash>, Error>;

	/// Returns the size of a child storage entry at a block's state.
	async fn storage_size(
		&self,
		block: Option<Block::Hash>,
		storage_key: PrefixedStorageKey,
		key: StorageKey,
	) -> Result<Option<u64>, Error> {
		self.storage(block, storage_key, key)
			.await
			.map(|x| x.map(|x| x.0.len() as u64))
	}
}

/// Child state API with subscriptions support.
pub struct ChildState<Block, Client> {
	backend: Box<dyn ChildStateBackend<Block, Client>>,
}

impl<Block, Client> ChildState<Block, Client>
	where
		Block: BlockT + 'static,
		Client: Send + Sync + 'static,
{
	/// Convert this to a RPC module.
	pub fn into_rpc_module(self) -> Result<RpcModule<Self>, JsonRpseeError> {
		let mut ctx_module = RpcModule::new(self);

		ctx_module.register_async_method("childstate_getStorage", |params, state| {
			let (storage_key, key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage(block, storage_key, key)
					.await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		ctx_module.register_async_method("childstate_getKeys", |params, state| {
			let (storage_key, key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage_keys(block, storage_key, key)
					.await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		ctx_module.register_async_method("childstate_getStorageHash", |params, state| {
			let (storage_key, key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage_hash(block, storage_key, key)
					.await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		ctx_module.register_async_method("childstate_getStorageSize", |params, state| {
			let (storage_key, key, block) = match params.parse() {
				Ok(params) => params,
				Err(e) => return Box::pin(future::err(e)),
			};
			async move {
				state.backend.storage_size(block, storage_key, key)
					.await
					.map_err(to_jsonrpsee_call_error)
			}.boxed()
		})?;

		Ok(ctx_module)
	}

}

fn client_err(err: sp_blockchain::Error) -> Error {
	Error::Client(Box::new(err))
}

fn to_jsonrpsee_call_error(err: Error) -> JsonRpseeCallError {
	JsonRpseeCallError::Failed(Box::new(err))
}
