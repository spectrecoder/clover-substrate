#![cfg_attr(not(feature = "std"), no_std)]

pub mod runner;
pub mod precompiles;

pub use crate::precompiles::{Precompile, Precompiles};
pub use crate::runner::Runner;
pub use fp_evm::{Account, Log, Vicinity, ExecutionInfo, CallInfo, CreateInfo};
pub use evm::{ExitReason, ExitSucceed, ExitError, ExitRevert, ExitFatal};

use sp_std::{marker::PhantomData, vec::Vec};
#[cfg(feature = "std")]
use codec::{Encode, Decode};
#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};
use frame_support::{decl_module, decl_storage, decl_event, decl_error};
use frame_support::weights::{Weight, Pays, PostDispatchInfo};
use frame_support::traits::{Currency, ExistenceRequirement, Get, OnKilledAccount};
use frame_support::dispatch::DispatchResultWithPostInfo;
use frame_system::RawOrigin;
use sp_core::{U256, H256, H160};
use sp_runtime::{AccountId32, traits::{UniqueSaturatedInto, BadOrigin}};
use evm::Config;
use fp_evm::AddressMapping;
use clover_traits::account::MergeAccount;

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

pub trait EnsureAddressOrigin<OuterOrigin> {
	/// Success return type.
	type Success;

	/// Perform the origin check.
	fn ensure_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<Self::Success, BadOrigin> {
		Self::try_address_origin(address, origin).map_err(|_| BadOrigin)
	}

	/// Try with origin.
	fn try_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<Self::Success, OuterOrigin>;
}

/// Ensure that the EVM address is the same as the Substrate address. This only works if the account
/// ID is `H160`.
pub struct EnsureAddressSame;

impl<OuterOrigin> EnsureAddressOrigin<OuterOrigin> for EnsureAddressSame where
	OuterOrigin: Into<Result<RawOrigin<H160>, OuterOrigin>> + From<RawOrigin<H160>>,
{
	type Success = H160;

	fn try_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<H160, OuterOrigin> {
		origin.into().and_then(|o| match o {
			RawOrigin::Signed(who) if &who == address => Ok(who),
			r => Err(OuterOrigin::from(r))
		})
	}
}

/// Ensure that the origin is root.
pub struct EnsureAddressRoot<AccountId>(sp_std::marker::PhantomData<AccountId>);

impl<OuterOrigin, AccountId> EnsureAddressOrigin<OuterOrigin> for EnsureAddressRoot<AccountId> where
	OuterOrigin: Into<Result<RawOrigin<AccountId>, OuterOrigin>> + From<RawOrigin<AccountId>>,
{
	type Success = ();

	fn try_address_origin(
		_address: &H160,
		origin: OuterOrigin,
	) -> Result<(), OuterOrigin> {
		origin.into().and_then(|o| match o {
			RawOrigin::Root => Ok(()),
			r => Err(OuterOrigin::from(r)),
		})
	}
}

/// Ensure that the origin never happens.
pub struct EnsureAddressNever<AccountId>(sp_std::marker::PhantomData<AccountId>);

impl<OuterOrigin, AccountId> EnsureAddressOrigin<OuterOrigin> for EnsureAddressNever<AccountId> {
	type Success = AccountId;

	fn try_address_origin(
		_address: &H160,
		origin: OuterOrigin,
	) -> Result<AccountId, OuterOrigin> {
		Err(origin)
	}
}

/// Ensure that the address is truncated hash of the origin. Only works if the account id is
/// `AccountId32`.
pub struct EnsureAddressTruncated;

impl<OuterOrigin> EnsureAddressOrigin<OuterOrigin> for EnsureAddressTruncated where
	OuterOrigin: Into<Result<RawOrigin<AccountId32>, OuterOrigin>> + From<RawOrigin<AccountId32>>,
{
	type Success = AccountId32;

	fn try_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<AccountId32, OuterOrigin> {
		origin.into().and_then(|o| match o {
			RawOrigin::Signed(who)
				if AsRef::<[u8; 32]>::as_ref(&who)[0..20] == address[0..20] => Ok(who),
			r => Err(OuterOrigin::from(r))
		})
	}
}

/// A mapping function that converts Ethereum gas to Substrate weight
pub trait GasToWeight {
	fn gas_to_weight(gas: u32) -> Weight;
}

impl GasToWeight for () {
	fn gas_to_weight(gas: u32) -> Weight {
		gas as Weight
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
	/// Calculator for current gas price.
	type FeeCalculator: FeeCalculator;

	/// Maps Ethereum gas to Substrate weight.
	type GasToWeight: GasToWeight;

	/// Allow the origin to call on behalf of given address.
	type CallOrigin: EnsureAddressOrigin<Self::Origin>;
	/// Allow the origin to withdraw on behalf of given address.
	type WithdrawOrigin: EnsureAddressOrigin<Self::Origin, Success=Self::AccountId>;

	/// Mapping from address to account id.
	type AddressMapping: AddressMapping<Self::AccountId>;

	/// Merge free balance from source to dest.
	type MergeAccount: MergeAccount<Self::AccountId>;

	/// Currency type for withdraw and balance storage.
	type Currency: Currency<Self::AccountId>;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;
	/// Precompiles associated with this EVM engine.
	type Precompiles: Precompiles;
	/// Chain ID of EVM.
	type ChainId: Get<u64>;
	/// EVM execution runner.
	type Runner: Runner<Self>;

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
		pub AccountConnection get(fn account_connection): map hasher(blake2_128_concat) H160 => H160;
		AccountCodes get(fn account_codes): map hasher(blake2_128_concat) H160 => Vec<u8>;
		AccountStorages get(fn account_storages):
			double_map hasher(blake2_128_concat) H160, hasher(blake2_128_concat) H256 => H256;
	}

	add_extra_genesis {
		config(accounts): std::collections::BTreeMap<H160, GenesisAccount>;
		build(|config: &GenesisConfig| {
			for (address, account) in &config.accounts {
				let account_id = T::AddressMapping::into_account_id(address);

				// ASSUME: in one single EVM transaction, the nonce will not increase more than
				// `u128::max_value()`.
				for _ in 0..account.nonce.low_u128() {
					frame_system::Module::<T>::inc_account_nonce(&account_id);
				}

				T::Currency::deposit_creating(
					&account_id,
					account.balance.low_u128().unique_saturated_into(),
				);

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
		/// A contract has been created at given \[address\].
		Created(H160),
		/// A \[contract\] was attempted to be created, but the execution failed.
		CreatedFailed(H160),
		/// A \[contract\] has been executed successfully with states applied.
		Executed(H160),
		/// A \[contract\] has been executed with errors. States are reverted with only gas fees applied.
		ExecutedFailed(H160),
		/// A deposit has been made at a given address. \[sender, address, value\]
		BalanceDeposit(AccountId, H160, U256),
		/// A withdrawal has been made from a given address. \[sender, address, value\]
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

		/// Withdraw balance from EVM into currency/balances module.
		#[weight = 0]
		fn withdraw(origin, address: H160, value: BalanceOf<T>) {
			let destination = T::WithdrawOrigin::ensure_address_origin(&address, origin)?;
			let address_account_id = T::AddressMapping::into_account_id(&address);

			T::Currency::transfer(
				&address_account_id,
				&destination,
				value,
				ExistenceRequirement::AllowDeath
			)?;
		}

		/// Issue an EVM call operation. This is similar to a message call transaction in Ethereum.
		#[weight = T::GasToWeight::gas_to_weight(*gas_limit)]
		fn call(
			origin,
			source: H160,
			target: H160,
			input: Vec<u8>,
			value: U256,
			gas_limit: u32,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResultWithPostInfo {
			T::CallOrigin::ensure_address_origin(&source, origin)?;

			let info = T::Runner::call(
				source,
				target,
				input,
				value,
				gas_limit,
				Some(gas_price),
				nonce,
				T::config(),
			)?;

			match info.exit_reason {
				ExitReason::Succeed(_) => {
					Module::<T>::deposit_event(Event::<T>::Executed(target));
				},
				_ => {
					Module::<T>::deposit_event(Event::<T>::ExecutedFailed(target));
				},
			};

			Ok(PostDispatchInfo {
				actual_weight: Some(T::GasToWeight::gas_to_weight(info.used_gas.low_u32())),
				pays_fee: Pays::No,
			})
		}

		/// Issue an EVM create operation. This is similar to a contract creation transaction in
		/// Ethereum.
		#[weight = T::GasToWeight::gas_to_weight(*gas_limit)]
		fn create(
			origin,
			source: H160,
			init: Vec<u8>,
			value: U256,
			gas_limit: u32,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResultWithPostInfo {
			T::CallOrigin::ensure_address_origin(&source, origin)?;

			let info = T::Runner::create(
				source,
				init,
				value,
				gas_limit,
				Some(gas_price),
				nonce,
				T::config(),
			)?;

			match info {
				CreateInfo {
					exit_reason: ExitReason::Succeed(_),
					value: create_address,
					..
				} => {
					Module::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				CreateInfo {
					exit_reason: _,
					value: create_address,
					..
				} => {
					Module::<T>::deposit_event(Event::<T>::CreatedFailed(create_address));
				},
			}

			Ok(PostDispatchInfo {
				actual_weight: Some(T::GasToWeight::gas_to_weight(info.used_gas.low_u32())),
				pays_fee: Pays::No,
			})
		}

		/// Issue an EVM create2 operation.
		#[weight = T::GasToWeight::gas_to_weight(*gas_limit)]
		fn create2(
			origin,
			source: H160,
			init: Vec<u8>,
			salt: H256,
			value: U256,
			gas_limit: u32,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResultWithPostInfo {
			T::CallOrigin::ensure_address_origin(&source, origin)?;

			let info = T::Runner::create2(
				source,
				init,
				salt,
				value,
				gas_limit,
				Some(gas_price),
				nonce,
				T::config(),
			)?;

			match info {
				CreateInfo {
					exit_reason: ExitReason::Succeed(_),
					value: create_address,
					..
				} => {
					Module::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				CreateInfo {
					exit_reason: _,
					value: create_address,
					..
				} => {
					Module::<T>::deposit_event(Event::<T>::CreatedFailed(create_address));
				},
			}

			Ok(PostDispatchInfo {
				actual_weight: Some(T::GasToWeight::gas_to_weight(info.used_gas.low_u32())),
				pays_fee: Pays::No,
			})
		}
	}
}

impl<T: Trait> Module<T> {
	/// Check whether an account is empty.
	pub fn is_account_empty(address: &H160) -> bool {
		let account = Self::account_basic(address);
		let code_len = AccountCodes::decode_len(address).unwrap_or(0);

		account.nonce == U256::zero() &&
			account.balance == U256::zero() &&
			code_len == 0
	}

	/// Remove an account if its empty.
	pub fn remove_account_if_empty(address: &H160) {
		if Self::is_account_empty(address) {
			Self::remove_account(address);
		}
	}

	/// Remove an account.
	pub fn remove_account(address: &H160) {
		AccountCodes::remove(address);
		AccountStorages::remove_prefix(address);
	}

	/// Get the account basic in EVM format.
	pub fn account_basic(address: &H160) -> Account {
		let account_id = T::AddressMapping::into_account_id(address);

		let nonce = frame_system::Module::<T>::account_nonce(&account_id);
		let balance = T::Currency::free_balance(&account_id);

		Account {
			nonce: U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(nonce)),
			balance: U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(balance)),
		}
	}
}

pub struct CallKillAccount<T>(PhantomData<T>);
impl<T: Trait> OnKilledAccount<T::AccountId> for CallKillAccount<T> {
	fn on_killed_account(who: &T::AccountId) {
		if let Some(address) = T::AddressMapping::to_evm_address(who) {
			Module::<T>::remove_account(&address)
		}
	}
}
