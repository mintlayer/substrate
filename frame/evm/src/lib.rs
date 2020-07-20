// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! EVM execution module for Substrate

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

mod backend;
mod tests;

pub use crate::backend::{Account, Log, Vicinity, Backend};

use sp_std::{vec::Vec, marker::PhantomData};
#[cfg(feature = "std")]
use codec::{Encode, Decode};
#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};
use frame_support::{ensure, decl_module, decl_storage, decl_event, decl_error};
use frame_support::weights::Weight;
use frame_support::traits::{Currency, WithdrawReason, ExistenceRequirement, Get};
use frame_system::ensure_signed;
use sp_runtime::ModuleId;
use sp_core::{U256, H256, H160, Hasher};
use sp_runtime::{
	DispatchResult, traits::{UniqueSaturatedInto, AccountIdConversion, SaturatedConversion},
};
use sha3::{Digest, Keccak256};
pub use evm::{ExitReason, ExitSucceed, ExitError, ExitRevert, ExitFatal};
use evm::Config;
use evm::executor::StackExecutor;
use evm::backend::ApplyBackend;

/// Type alias for currency balance.
pub type BalanceOf<T> = <<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;

/// Trait that outputs the current transaction gas price.
pub trait FeeCalculator {
	/// Return the minimal required gas price.
	fn min_gas_price() -> U256;
}

impl FeeCalculator for () {
	fn min_gas_price() -> U256 { U256::zero() }
}

/// Trait for converting account ids of `balances` module into
/// `H160` for EVM module.
///
/// Accounts and contracts of this module are stored in its own
/// storage, in an Ethereum-compatible format. In order to communicate
/// with the rest of Substrate module, we require an one-to-one
/// mapping of Substrate account to Ethereum address.
pub trait ConvertAccountId<A> {
	/// Given a Substrate address, return the corresponding Ethereum address.
	fn convert_account_id(account_id: &A) -> H160;
}

/// Hash and then truncate the account id, taking the last 160-bit as the Ethereum address.
pub struct HashTruncateConvertAccountId<H>(PhantomData<H>);

impl<H: Hasher> Default for HashTruncateConvertAccountId<H> {
	fn default() -> Self {
		Self(PhantomData)
	}
}

impl<H: Hasher, A: AsRef<[u8]>> ConvertAccountId<A> for HashTruncateConvertAccountId<H> {
	fn convert_account_id(account_id: &A) -> H160 {
		let account_id = H::hash(account_id.as_ref());
		let account_id_len = account_id.as_ref().len();
		let mut value = [0u8; 20];
		let value_len = value.len();

		if value_len > account_id_len {
			value[(value_len - account_id_len)..].copy_from_slice(account_id.as_ref());
		} else {
			value.copy_from_slice(&account_id.as_ref()[(account_id_len - value_len)..]);
		}

		H160::from(value)
	}
}

/// Custom precompiles to be used by EVM engine.
pub trait Precompiles {
	/// Try to execute the code address as precompile. If the code address is not
	/// a precompile or the precompile is not yet available, return `None`.
	/// Otherwise, calculate the amount of gas needed with given `input` and
	/// `target_gas`. Return `Some(Ok(status, output, gas_used))` if the execution
	/// is successful. Otherwise return `Some(Err(_))`.
	fn execute(
		address: H160,
		input: &[u8],
		target_gas: Option<usize>
	) -> Option<core::result::Result<(ExitSucceed, Vec<u8>, usize), ExitError>>;
}

impl Precompiles for () {
	fn execute(
		_address: H160,
		_input: &[u8],
		_target_gas: Option<usize>
	) -> Option<core::result::Result<(ExitSucceed, Vec<u8>, usize), ExitError>> {
		None
	}
}

/// Substrate system chain ID.
pub struct SystemChainId;

impl Get<u64> for SystemChainId {
	fn get() -> u64 {
		sp_io::misc::chain_id()
	}
}

static ISTANBUL_CONFIG: Config = Config::istanbul();

/// EVM module trait
pub trait Trait: frame_system::Trait + pallet_timestamp::Trait {
	/// The EVM's module id
	type ModuleId: Get<ModuleId>;
	/// Calculator for current gas price.
	type FeeCalculator: FeeCalculator;
	/// Convert account ID to H160;
	type ConvertAccountId: ConvertAccountId<Self::AccountId>;
	/// Currency type for deposit and withdraw.
	type Currency: Currency<Self::AccountId>;
	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;
	/// Precompiles associated with this EVM engine.
	type Precompiles: Precompiles;
	/// Chain ID of EVM.
	type ChainId: Get<u64>;

	/// EVM config used in the module.
	fn config() -> &'static Config {
		&ISTANBUL_CONFIG
	}
}

#[cfg(feature = "std")]
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, Serialize, Deserialize)]
/// Account definition used for genesis block construction.
pub struct GenesisAccount {
	/// Account nonce.
	pub nonce: U256,
	/// Account balance.
	pub balance: U256,
	/// Full account storage.
	pub storage: std::collections::BTreeMap<H256, H256>,
	/// Account code.
	pub code: Vec<u8>,
}

decl_storage! {
	trait Store for Module<T: Trait> as EVM {
		Accounts get(fn accounts): map hasher(blake2_128_concat) H160 => Account;
		AccountCodes get(fn account_codes): map hasher(blake2_128_concat) H160 => Vec<u8>;
		AccountStorages get(fn account_storages):
			double_map hasher(blake2_128_concat) H160, hasher(blake2_128_concat) H256 => H256;
	}

	add_extra_genesis {
		config(accounts): std::collections::BTreeMap<H160, GenesisAccount>;
		build(|config: &GenesisConfig| {
			for (address, account) in &config.accounts {
				Accounts::insert(address, Account {
					balance: account.balance,
					nonce: account.nonce,
				});
				AccountCodes::insert(address, &account.code);

				for (index, value) in &account.storage {
					AccountStorages::insert(address, index, value);
				}
			}
		});
	}
}

decl_event! {
	/// EVM events
	pub enum Event<T> where
		<T as frame_system::Trait>::AccountId,
	{
		/// Ethereum events from contracts.
		Log(Log),
		/// A contract has been created at given [address].
		Created(H160),
		/// A [contract] was attempted to be created, but the execution failed.
		CreatedFailed(H160),
		/// A [contract] has been executed successfully with states applied.
		Executed(H160),
		/// A [contract] has been executed with errors. States are reverted with only gas fees applied.
		ExecutedFailed(H160),
		/// A deposit has been made at a given address. [sender, address, value]
		BalanceDeposit(AccountId, H160, U256),
		/// A withdrawal has been made from a given address. [sender, address, value]
		BalanceWithdraw(AccountId, H160, U256),
	}
}

decl_error! {
	pub enum Error for Module<T: Trait> {
		/// Not enough balance to perform action
		BalanceLow,
		/// Calculating total fee overflowed
		FeeOverflow,
		/// Calculating total payment overflowed
		PaymentOverflow,
		/// Withdraw fee failed
		WithdrawFailed,
		/// Gas price is too low.
		GasPriceTooLow,
		/// Nonce is invalid
		InvalidNonce,
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		fn deposit_event() = default;

		const ModuleId: ModuleId = T::ModuleId::get();

		/// Deposit balance from currency/balances module into EVM.
		#[weight = 0]
		fn deposit_balance(origin, value: BalanceOf<T>) {
			let sender = ensure_signed(origin)?;

			let imbalance = T::Currency::withdraw(
				&sender,
				value,
				WithdrawReason::Reserve.into(),
				ExistenceRequirement::AllowDeath,
			)?;
			T::Currency::resolve_creating(&Self::account_id(), imbalance);

			let bvalue = U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(value));
			let address = T::ConvertAccountId::convert_account_id(&sender);
			Accounts::mutate(&address, |account| {
				account.balance += bvalue;
			});
			Module::<T>::deposit_event(Event::<T>::BalanceDeposit(sender, address, bvalue));
		}

		/// Withdraw balance from EVM into currency/balances module.
		#[weight = 0]
		fn withdraw_balance(origin, value: BalanceOf<T>) {
			let sender = ensure_signed(origin)?;
			let address = T::ConvertAccountId::convert_account_id(&sender);
			let bvalue = U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(value));

			let mut account = Accounts::get(&address);
			account.balance = account.balance.checked_sub(bvalue)
				.ok_or(Error::<T>::BalanceLow)?;

			let imbalance = T::Currency::withdraw(
				&Self::account_id(),
				value,
				WithdrawReason::Reserve.into(),
				ExistenceRequirement::AllowDeath
			)?;

			Accounts::insert(&address, account);

			T::Currency::resolve_creating(&sender, imbalance);
			Module::<T>::deposit_event(Event::<T>::BalanceWithdraw(sender, address, bvalue));
		}

		/// Issue an EVM call operation. This is similar to a message call transaction in Ethereum.
		#[weight = (*gas_price).saturated_into::<Weight>().saturating_mul(*gas_limit as Weight)]
		fn call(
			origin,
			target: H160,
			input: Vec<u8>,
			value: U256,
			gas_limit: u32,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResult {
			ensure!(gas_price >= T::FeeCalculator::min_gas_price(), Error::<T>::GasPriceTooLow);

			let sender = ensure_signed(origin)?;
			let source = T::ConvertAccountId::convert_account_id(&sender);

			match Self::execute_call(
				source,
				target,
				input,
				value,
				gas_limit,
				gas_price,
				nonce,
				true,
			)? {
				(ExitReason::Succeed(_), _, _) => {
					Module::<T>::deposit_event(Event::<T>::Executed(target));
				},
				(_, _, _) => {
					Module::<T>::deposit_event(Event::<T>::ExecutedFailed(target));
				},
			}

			Ok(())
		}

		/// Issue an EVM create operation. This is similar to a contract creation transaction in
		/// Ethereum.
		#[weight = (*gas_price).saturated_into::<Weight>().saturating_mul(*gas_limit as Weight)]
		fn create(
			origin,
			init: Vec<u8>,
			value: U256,
			gas_limit: u32,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResult {
			ensure!(gas_price >= T::FeeCalculator::min_gas_price(), Error::<T>::GasPriceTooLow);

			let sender = ensure_signed(origin)?;
			let source = T::ConvertAccountId::convert_account_id(&sender);

			match Self::execute_create(
				source,
				init,
				value,
				gas_limit,
				gas_price,
				nonce,
				true,
			)? {
				(ExitReason::Succeed(_), create_address, _) => {
					Module::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				(_, create_address, _) => {
					Module::<T>::deposit_event(Event::<T>::CreatedFailed(create_address));
				},
			}

			Ok(())
		}

		/// Issue an EVM create2 operation.
		#[weight = (*gas_price).saturated_into::<Weight>().saturating_mul(*gas_limit as Weight)]
		fn create2(
			origin,
			init: Vec<u8>,
			salt: H256,
			value: U256,
			gas_limit: u32,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResult {
			ensure!(gas_price >= T::FeeCalculator::min_gas_price(), Error::<T>::GasPriceTooLow);

			let sender = ensure_signed(origin)?;
			let source = T::ConvertAccountId::convert_account_id(&sender);

			match Self::execute_create2(
				source,
				init,
				salt,
				value,
				gas_limit,
				gas_price,
				nonce,
				true,
			)? {
				(ExitReason::Succeed(_), create_address, _) => {
					Module::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				(_, create_address, _) => {
					Module::<T>::deposit_event(Event::<T>::CreatedFailed(create_address));
				},
			}

			Ok(())
		}
	}
}

impl<T: Trait> Module<T> {
	/// The account ID of the EVM module.
	///
	/// This actually does computation. If you need to keep using it, then make sure you cache the
	/// value and only call this once.
	pub fn account_id() -> T::AccountId {
		T::ModuleId::get().into_account()
	}

	/// Check whether an account is empty.
	pub fn is_account_empty(address: &H160) -> bool {
		let account = Accounts::get(address);
		let code_len = AccountCodes::decode_len(address).unwrap_or(0);

		account.nonce == U256::zero() &&
			account.balance == U256::zero() &&
			code_len == 0
	}

	/// Remove an account if its empty.
	pub fn remove_account_if_empty(address: &H160) {
		if Self::is_account_empty(address) {
			Self::remove_account(address)
		}
	}

	/// Remove an account from state.
	fn remove_account(address: &H160) {
		Accounts::remove(address);
		AccountCodes::remove(address);
		AccountStorages::remove_prefix(address);
	}

	/// Execute a create transaction on behalf of given sender.
	pub fn execute_create(
		source: H160,
		init: Vec<u8>,
		value: U256,
		gas_limit: u32,
		gas_price: U256,
		nonce: Option<U256>,
		apply_state: bool,
	) -> Result<(ExitReason, H160, U256), Error<T>> {
		Self::execute_evm(
			source,
			value,
			gas_limit,
			gas_price,
			nonce,
			apply_state,
			|executor| {
				let address = executor.create_address(
					evm::CreateScheme::Legacy { caller: source },
				);
				(executor.transact_create(
					source,
					value,
					init,
					gas_limit as usize,
				), address)
			},
		)
	}

	/// Execute a create2 transaction on behalf of a given sender.
	pub fn execute_create2(
		source: H160,
		init: Vec<u8>,
		salt: H256,
		value: U256,
		gas_limit: u32,
		gas_price: U256,
		nonce: Option<U256>,
		apply_state: bool,
	) -> Result<(ExitReason, H160, U256), Error<T>> {
		let code_hash = H256::from_slice(Keccak256::digest(&init).as_slice());
		Self::execute_evm(
			source,
			value,
			gas_limit,
			gas_price,
			nonce,
			apply_state,
			|executor| {
				let address = executor.create_address(
					evm::CreateScheme::Create2 { caller: source, code_hash, salt },
				);
				(executor.transact_create2(
					source,
					value,
					init,
					salt,
					gas_limit as usize,
				), address)
			},
		)
	}

	/// Execute a call transaction on behalf of a given sender.
	pub fn execute_call(
		source: H160,
		target: H160,
		input: Vec<u8>,
		value: U256,
		gas_limit: u32,
		gas_price: U256,
		nonce: Option<U256>,
		apply_state: bool,
	) -> Result<(ExitReason, Vec<u8>, U256), Error<T>> {
		Self::execute_evm(
			source,
			value,
			gas_limit,
			gas_price,
			nonce,
			apply_state,
			|executor| executor.transact_call(
				source,
				target,
				value,
				input,
				gas_limit as usize,
			),
		)
	}

	/// Execute an EVM operation.
	fn execute_evm<F, R>(
		source: H160,
		value: U256,
		gas_limit: u32,
		gas_price: U256,
		nonce: Option<U256>,
		apply_state: bool,
		f: F,
	) -> Result<(ExitReason, R, U256), Error<T>> where
		F: FnOnce(&mut StackExecutor<Backend<T>>) -> (ExitReason, R),
	{
		let vicinity = Vicinity {
			gas_price,
			origin: source,
		};

		let mut backend = Backend::<T>::new(&vicinity);
		let mut executor = StackExecutor::new_with_precompile(
			&backend,
			gas_limit as usize,
			T::config(),
			T::Precompiles::execute,
		);

		let total_fee = gas_price.checked_mul(U256::from(gas_limit))
			.ok_or(Error::<T>::FeeOverflow)?;
		let total_payment = value.checked_add(total_fee).ok_or(Error::<T>::PaymentOverflow)?;
		let source_account = Accounts::get(&source);
		ensure!(source_account.balance >= total_payment, Error::<T>::BalanceLow);
		executor.withdraw(source, total_fee).map_err(|_| Error::<T>::WithdrawFailed)?;

		if let Some(nonce) = nonce {
			ensure!(source_account.nonce == nonce, Error::<T>::InvalidNonce);
		}

		let (retv, reason) = f(&mut executor);

		let used_gas = U256::from(executor.used_gas());
		let actual_fee = executor.fee(gas_price);
		executor.deposit(source, total_fee.saturating_sub(actual_fee));

		if apply_state {
			let (values, logs) = executor.deconstruct();
			backend.apply(values, logs, true);
		}

		Ok((retv, reason, used_gas))
	}
}
