#![deny(clippy::pedantic, clippy::nursery)]
#![allow(clippy::module_name_repetitions)]
use crate::admin_controlled::{AdminControlled, PausedMask, PAUSE_WITHDRAW, UNPAUSE_ALL};
use crate::connector::{
    Deposit, EngineConnectorWithdraw, EngineFungibleToken, EngineStorageManagement, FundsFinish,
    KnownEngineAccountsManagement, Withdraw,
};
use crate::connector_impl::{
    EthConnector, FinishDepositCallArgs, TransferCallCallArgs, WithdrawResult,
};
use crate::deposit_event::FtTransferMessageData;
use crate::proof::{Proof, VerifyProofArgs};
use crate::types::{panic_err, SdkUnwrap};
use aurora_engine_types::types::Address;
use near_contract_standards::fungible_token::core::FungibleTokenCore;
use near_contract_standards::fungible_token::metadata::{
    FungibleTokenMetadata, FungibleTokenMetadataProvider, FT_METADATA_SPEC,
};
use near_contract_standards::fungible_token::receiver::ext_ft_receiver;
use near_contract_standards::fungible_token::resolver::{ext_ft_resolver, FungibleTokenResolver};
use near_contract_standards::fungible_token::FungibleToken;
use near_sdk::{
    assert_one_yocto,
    borsh::{self, BorshDeserialize, BorshSerialize},
    collections::{LazyOption, LookupSet},
    env,
    json_types::U128,
    near_bindgen, require, AccountId, Balance, BorshStorageKey, Gas, PanicOnDefault, Promise,
    PromiseOrValue,
};

pub mod admin_controlled;
pub mod connector;
pub mod connector_impl;
pub mod deposit_event;
pub mod errors;
pub mod log_entry;
pub mod migration;
pub mod proof;
pub mod types;

const GAS_FOR_RESOLVE_TRANSFER: Gas = Gas(5 * Gas::ONE_TERA.0);
const GAS_FOR_FT_TRANSFER_CALL: Gas = Gas(25 * Gas::ONE_TERA.0 + GAS_FOR_RESOLVE_TRANSFER.0);

/// Eth-connector contract data. It's stored in the storage.
/// Contains:
/// * connector specific data
/// * Fungible token data
/// * paused_mask - admin control flow data
#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
pub struct EthConnectorContract {
    connector: EthConnector,
    ft: FungibleToken,
    metadata: LazyOption<FungibleTokenMetadata>,
    used_proofs: LookupSet<String>,
    known_engine_accounts: LookupSet<AccountId>,
}

#[derive(BorshSerialize, BorshStorageKey)]
enum StorageKey {
    FungibleToken = 0x1,
    Proof = 0x2,
    Metadata = 0x3,
    EngineAccounts = 0x4,
}

impl EthConnectorContract {
    ///  Mint `nETH` tokens
    fn mint_eth_on_near(&mut self, owner_id: &AccountId, amount: Balance) {
        log!("Mint {} nETH tokens for: {}", amount, owner_id);
        // Create account to avoid panic with deposit
        self.register_if_not_exists(owner_id);
        self.ft.internal_deposit(owner_id, amount);
    }

    /// Record used proof as hash key
    fn record_proof(&mut self, key: &String) -> Result<(), errors::ProofUsed> {
        log!("Record proof: {}", key);
        if self.is_used_event(key) {
            return Err(errors::ProofUsed);
        }
        self.used_proofs.insert(key);
        Ok(())
    }

    /// Check is event of proof already used
    fn is_used_event(&self, key: &String) -> bool {
        self.used_proofs.contains(key)
    }

    // Register user and calculate counter
    fn register_if_not_exists(&mut self, account: &AccountId) {
        if !self.ft.accounts.contains_key(account) {
            self.ft.internal_register_account(account);
        }
    }

    fn internal_ft_transfer_call(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        self.register_if_not_exists(&receiver_id);

        let amount: Balance = amount.into();
        log!(
            "Transfer call from {} to {} amount {}",
            sender_id,
            receiver_id,
            amount,
        );

        // Verify message data before `ft_on_transfer` call for Engine accounts
        // to avoid verification panics inside `ft_on_transfer`.
        // Allowed empty message if `receiver_id != known_engin_accounts`.
        if self.known_engine_accounts.contains(&receiver_id) {
            let _ = FtTransferMessageData::parse_on_transfer_message(&msg).sdk_unwrap();
        }

        // Special case, we do not fail if `sender_id == receiver_id`
        // if `predecessor_account_id` call `ft_transfer_call` as receiver itself
        // to call `ft_on_transfer`.
        if sender_id == receiver_id {
            // If `sender_id == receiver_id` we should verify
            // that sender account has sufficient account balance.
            // NOTE: Related to Audit AUR-11 report issue
            require!(
                amount > 0,
                "The amount should be a positive non zero number"
            );
            let balance = self.ft.ft_balance_of(sender_id.clone());
            require!(balance.0 >= amount, "Insufficient sender balance");
        } else {
            self.ft
                .internal_transfer(&sender_id, &receiver_id, amount, memo);
        }

        let receiver_gas = env::prepaid_gas()
            .0
            .checked_sub(GAS_FOR_FT_TRANSFER_CALL.0)
            .unwrap_or_else(|| env::panic_str("Prepaid gas overflow"));
        // Initiating receiver's call and the callback
        ext_ft_receiver::ext(receiver_id.clone())
            .with_static_gas(receiver_gas.into())
            .ft_on_transfer(sender_id.clone(), amount.into(), msg)
            .then(
                ext_ft_resolver::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_RESOLVE_TRANSFER)
                    .ft_resolve_transfer(sender_id, receiver_id, amount.into()),
            )
            .into()
    }
}

#[near_bindgen]
impl EthConnectorContract {
    #[init]
    #[must_use]
    pub fn new(
        prover_account: AccountId,
        eth_custodian_address: Address,
        metadata: &FungibleTokenMetadata,
        account_with_access_right: AccountId,
        owner_id: &AccountId,
    ) -> Self {
        metadata.assert_valid();

        // Get initial Eth Connector arguments
        let paused_mask = UNPAUSE_ALL;
        let connector_data = EthConnector {
            prover_account,
            paused_mask,
            eth_custodian_address,
            account_with_access_right,
            owner_id: owner_id.clone(),
        };
        let mut this = Self {
            ft: FungibleToken {
                accounts: near_sdk::collections::LookupMap::new(StorageKey::FungibleToken),
                total_supply: 0,
                account_storage_usage: 0,
            },
            connector: connector_data,
            metadata: LazyOption::new(StorageKey::Metadata, Some(metadata)),
            used_proofs: LookupSet::new(StorageKey::Proof),
            known_engine_accounts: LookupSet::new(StorageKey::EngineAccounts),
        };
        this.register_if_not_exists(&env::current_account_id());
        this.register_if_not_exists(owner_id);
        this
    }

    #[must_use]
    #[result_serializer(borsh)]
    pub fn is_used_proof(&self, #[serializer(borsh)] proof: &Proof) -> bool {
        self.is_used_event(&proof.get_key())
    }

    #[cfg(feature = "integration-test")]
    #[result_serializer(borsh)]
    #[must_use]
    #[allow(unused_variables)]
    pub fn verify_log_entry(#[serializer(borsh)] proof_args: &VerifyProofArgs) -> bool {
        log!("Call from verify_log_entry");
        true
    }

    #[must_use]
    pub fn get_bridge_prover(&self) -> AccountId {
        self.connector.prover_account.clone()
    }
}

#[near_bindgen]
impl FungibleTokenCore for EthConnectorContract {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        self.register_if_not_exists(&receiver_id);
        self.ft.ft_transfer(receiver_id, amount, memo);
    }

    #[payable]
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        assert_one_yocto();
        require!(
            env::prepaid_gas() > GAS_FOR_FT_TRANSFER_CALL,
            "More gas is required"
        );
        let sender_id = env::predecessor_account_id();
        self.internal_ft_transfer_call(sender_id, receiver_id, amount, memo, msg)
    }

    fn ft_total_supply(&self) -> U128 {
        self.ft.ft_total_supply()
    }

    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.ft.ft_balance_of(account_id)
    }
}

/// Fungible Token Trait implementation for compatibility with Engine NEP-141 methods.
/// It's because should have a known correct `sender_id`. In reference
/// implementation it's `predecessor_account_id`. To resolve it
/// we just set `sender_id` explicitly as function parameter.
/// Also we check access right to manage access rights.
#[near_bindgen]
impl EngineFungibleToken for EthConnectorContract {
    #[payable]
    fn engine_ft_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
    ) {
        self.assert_access_right().sdk_unwrap();
        self.register_if_not_exists(&receiver_id);
        assert_one_yocto();
        let amount: Balance = amount.into();
        self.ft
            .internal_transfer(&sender_id, &receiver_id, amount, memo);
    }

    #[payable]
    fn engine_ft_transfer_call(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        assert_one_yocto();
        require!(
            env::prepaid_gas() > GAS_FOR_FT_TRANSFER_CALL,
            "More gas is required"
        );
        self.assert_access_right().sdk_unwrap();
        self.internal_ft_transfer_call(sender_id, receiver_id, amount, memo, msg)
    }
}

/// Management for a known Engine accounts
#[near_bindgen]
impl KnownEngineAccountsManagement for EthConnectorContract {
    fn set_engine_account(&mut self, engine_account: &AccountId) {
        self.assert_access_right().sdk_unwrap();
        self.known_engine_accounts.insert(engine_account);
    }

    fn remove_engine_account(&mut self, engine_account: &AccountId) {
        self.assert_access_right().sdk_unwrap();
        self.known_engine_accounts.remove(engine_account);
    }

    fn is_engine_account_exist(&self, engine_account: &AccountId) -> bool {
        self.known_engine_accounts.contains(engine_account)
    }
}

/// Implementations used only for `EngineStorageManagement`
impl EthConnectorContract {
    fn internal_storage_balance_of(&self, account_id: &AccountId) -> Option<StorageBalance> {
        if self.ft.accounts.contains_key(account_id) {
            Some(StorageBalance {
                total: self.storage_balance_bounds().min,
                available: 0.into(),
            })
        } else {
            None
        }
    }

    fn internal_storage_unregister(
        &mut self,
        sender_id: AccountId,
        force: Option<bool>,
    ) -> Option<(AccountId, Balance)> {
        assert_one_yocto();
        let account_id = sender_id;
        let force = force.unwrap_or(false);
        if let Some(balance) = self.ft.accounts.get(&account_id) {
            if balance == 0 || force {
                self.ft.accounts.remove(&account_id);
                self.ft.total_supply -= balance;
                Promise::new(account_id.clone()).transfer(self.storage_balance_bounds().min.0 + 1);
                Some((account_id, balance))
            } else {
                env::panic_str(
                    "Can't unregister the account with the positive balance without force",
                )
            }
        } else {
            log!("The account {} is not registered", &account_id);
            None
        }
    }
}

/// Storage Management Trait implementation for compatibility with Engine NEP-141 methods.
/// It's because we should ve known correct `sender_id`. In reference
/// implementation it's `predecessor_account_id`. To resolve it
/// we just set `sender_id` explicitly as function parameter.
#[near_bindgen]
impl EngineStorageManagement for EthConnectorContract {
    /// Store a deposit for account.
    ///
    /// # Panics
    ///
    /// If the attached deposit is less then the balance of the smart contract.
    #[allow(unused_variables)]
    #[payable]
    fn engine_storage_deposit(
        &mut self,
        sender_id: AccountId,
        account_id: Option<AccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        self.assert_access_right().sdk_unwrap();
        let amount: Balance = env::attached_deposit();
        let account_id = account_id.unwrap_or_else(|| sender_id.clone());
        if self.ft.accounts.contains_key(&account_id) {
            log!("The account is already registered, refunding the deposit");
            if amount > 0 {
                Promise::new(sender_id).transfer(amount);
            }
        } else {
            let min_balance = self.storage_balance_bounds().min.0;
            if amount < min_balance {
                env::panic_str("The attached deposit is less than the minimum storage balance");
            }

            self.ft.internal_register_account(&account_id);
            let refund = amount - min_balance;
            if refund > 0 {
                Promise::new(sender_id).transfer(refund);
            }
        }
        self.internal_storage_balance_of(&account_id).unwrap()
    }

    #[payable]
    fn engine_storage_withdraw(
        &mut self,
        sender_id: AccountId,
        amount: Option<U128>,
    ) -> StorageBalance {
        self.assert_access_right().sdk_unwrap();
        assert_one_yocto();
        let predecessor_account_id = sender_id;
        self.internal_storage_balance_of(&predecessor_account_id)
            .map_or_else(
                || {
                    env::panic_str(
                        format!("The account {} is not registered", &predecessor_account_id)
                            .as_str(),
                    );
                },
                |storage_balance| match amount {
                    Some(amount) if amount.0 > 0 => {
                        env::panic_str("The amount is greater than the available storage balance");
                    }
                    _ => storage_balance,
                },
            )
    }

    #[payable]
    fn engine_storage_unregister(&mut self, sender_id: AccountId, force: Option<bool>) -> bool {
        self.assert_access_right().sdk_unwrap();
        self.internal_storage_unregister(sender_id, force).is_some()
    }
}

#[near_bindgen]
impl FungibleTokenResolver for EthConnectorContract {
    #[private]
    fn ft_resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        self.ft.ft_resolve_transfer(sender_id, receiver_id, amount)
    }
}

near_contract_standards::impl_fungible_token_storage!(EthConnectorContract, ft);

#[near_bindgen]
impl FungibleTokenMetadataProvider for EthConnectorContract {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        self.metadata.get().map_or(FungibleTokenMetadata {
            spec: FT_METADATA_SPEC.to_string(),
            name: "Ether".to_string(),
            symbol: "ETH".to_string(),
            icon: Some("data:image/png;base64,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".to_string()),
            reference: None,
            reference_hash: None,
            decimals: 18,
        }, |v|v)
    }
}

#[near_bindgen]
impl AdminControlled for EthConnectorContract {
    #[result_serializer(borsh)]
    fn get_paused_flags(&self) -> PausedMask {
        self.connector.get_paused_flags()
    }

    fn set_paused_flags(&mut self, #[serializer(borsh)] paused: PausedMask) {
        self.connector.assert_owner_access_right().sdk_unwrap();
        self.connector.set_paused_flags(paused);
    }

    fn set_access_right(&mut self, account: &AccountId) {
        self.connector.assert_owner_access_right().sdk_unwrap();
        self.connector.set_access_right(account);
    }

    fn get_account_with_access_right(&self) -> AccountId {
        self.connector.get_account_with_access_right()
    }

    fn is_owner(&self) -> bool {
        self.connector.is_owner()
    }
}

#[near_bindgen]
impl Withdraw for EthConnectorContract {
    #[payable]
    #[result_serializer(borsh)]
    fn withdraw(
        &mut self,
        #[serializer(borsh)] recipient_address: Address,
        #[serializer(borsh)] amount: Balance,
    ) -> WithdrawResult {
        assert_one_yocto();

        // Check is current flow paused. If it's owner just skip assertion.
        self.assert_not_paused(PAUSE_WITHDRAW)
            .map_err(|_| "WithdrawErrorPaused")
            .sdk_unwrap();

        let sender_id = env::predecessor_account_id();
        // Burn tokens to recipient
        self.ft.internal_withdraw(&sender_id, amount);
        WithdrawResult {
            amount,
            recipient_id: recipient_address,
            eth_custodian_address: self.connector.eth_custodian_address,
        }
    }
}

#[near_bindgen]
impl EngineConnectorWithdraw for EthConnectorContract {
    #[payable]
    #[result_serializer(borsh)]
    fn engine_withdraw(
        &mut self,
        #[serializer(borsh)] sender_id: AccountId,
        #[serializer(borsh)] recipient_address: Address,
        #[serializer(borsh)] amount: Balance,
    ) -> WithdrawResult {
        self.assert_access_right().sdk_unwrap();
        assert_one_yocto();

        // Check is current flow paused. If it's owner just skip assertion.
        self.assert_not_paused(PAUSE_WITHDRAW)
            .map_err(|_| "WithdrawErrorPaused")
            .sdk_unwrap();
        // Burn tokens to recipient
        self.ft.internal_withdraw(&sender_id, amount);
        WithdrawResult {
            amount,
            recipient_id: recipient_address,
            eth_custodian_address: self.connector.eth_custodian_address,
        }
    }
}

#[near_bindgen]
impl Deposit for EthConnectorContract {
    fn deposit(&mut self, #[serializer(borsh)] proof: Proof) -> Promise {
        self.connector.deposit(proof)
    }
}

#[near_bindgen]
impl FundsFinish for EthConnectorContract {
    #[private]
    fn finish_deposit(
        &mut self,
        #[serializer(borsh)] deposit_call: FinishDepositCallArgs,
        #[callback_unwrap]
        #[serializer(borsh)]
        verify_log_result: bool,
    ) -> PromiseOrValue<Option<U128>> {
        if !verify_log_result {
            panic_err(errors::ERR_VERIFY_PROOF);
        }

        log!("Finish deposit with the amount: {}", deposit_call.amount);

        // Mint - calculate new balances
        self.mint_eth_on_near(&deposit_call.new_owner_id, deposit_call.amount);
        // Store proof only after `mint` calculations
        self.record_proof(&deposit_call.proof_key).sdk_unwrap();

        // Mint tokens to recipient minus fee
        deposit_call.msg.map_or_else(
            || PromiseOrValue::Value(None),
            |msg| {
                let args = TransferCallCallArgs::try_from_slice(&msg)
                    .map_err(|_| crate::errors::ERR_BORSH_DESERIALIZE)
                    .sdk_unwrap();
                let promise = self.internal_ft_transfer_call(
                    env::predecessor_account_id(),
                    args.receiver_id,
                    args.amount.into(),
                    args.memo,
                    args.msg,
                );
                match promise {
                    PromiseOrValue::Promise(p) => PromiseOrValue::Promise(p),
                    PromiseOrValue::Value(v) => PromiseOrValue::Value(Some(v)),
                }
            },
        )
    }
}

#[cfg(feature = "migration")]
use crate::migration::{CheckResult, InputData, Migration};

#[cfg(feature = "migration")]
#[near_bindgen]
impl Migration for EthConnectorContract {
    /// Migrate contract data
    #[private]
    fn migrate(&mut self, #[serializer(borsh)] data: InputData) {
        // Insert account
        for (account, amount) in &data.accounts {
            self.ft.accounts.insert(account, amount);
        }
        log!("Inserted accounts_eth: {:?}", data.accounts.len());

        // Insert total_eth_supply_on_near
        if let Some(total_eth_supply_on_near) = data.total_supply {
            self.ft.total_supply = total_eth_supply_on_near;
            log!(
                "Inserted total_eth_supply_on_near: {:?}",
                total_eth_supply_on_near
            );
        }

        // Insert account_storage_usage
        if let Some(account_storage_usage) = data.account_storage_usage {
            self.ft.account_storage_usage = account_storage_usage;
            log!(
                "Inserted account_storage_usage: {:?}",
                account_storage_usage
            );
        }

        // Insert Proof
        for proof_key in &data.used_proofs {
            self.used_proofs.insert(proof_key);
        }
        log!("Inserted used_proofs: {:?}", data.used_proofs.len());
    }

    #[result_serializer(borsh)]
    fn check_migration_correctness(&self, #[serializer(borsh)] data: InputData) -> CheckResult {
        use std::collections::HashMap;

        // Check accounts
        let mut accounts_not_found: Vec<AccountId> = vec![];
        let mut accounts_with_amount_not_found = HashMap::new();

        for (account, amount) in &data.accounts {
            self.ft.accounts.get(account).as_ref().map_or_else(
                || accounts_not_found.push(account.clone()),
                |value| {
                    if value != amount {
                        accounts_with_amount_not_found.insert(account.clone(), *value);
                    }
                },
            );
        }
        if !accounts_not_found.is_empty() {
            return CheckResult::AccountNotExist(accounts_not_found);
        }
        if !accounts_with_amount_not_found.is_empty() {
            return CheckResult::AccountAmount(accounts_with_amount_not_found);
        }

        // Check proofs
        let mut proofs_not_found: Vec<String> = vec![];
        for proof in &data.used_proofs {
            if !self.used_proofs.contains(proof) {
                proofs_not_found.push(proof.clone());
            }
        }
        if !proofs_not_found.is_empty() {
            return CheckResult::Proof(proofs_not_found);
        }

        if let Some(account_storage_usage) = data.account_storage_usage {
            if self.ft.account_storage_usage != account_storage_usage {
                return CheckResult::StorageUsage(self.ft.account_storage_usage);
            }
        }
        if let Some(total_supply) = data.total_supply {
            if self.ft.total_supply != total_supply {
                return CheckResult::TotalSupply(self.ft.total_supply);
            }
        }
        CheckResult::Success
    }
}

#[cfg(feature = "integration-test")]
use near_contract_standards::fungible_token::receiver::FungibleTokenReceiver;

#[cfg(feature = "integration-test")]
#[near_bindgen]
impl FungibleTokenReceiver for EthConnectorContract {
    #[allow(unused_variables)]
    fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        PromiseOrValue::Value(U128(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In the original implementation of the eth-connector contract the storage balance
    /// was always 0. This test confirms this is true for the new implementation.
    #[test]
    fn test_storage_balance_bounds() {
        let contract = create_contract();
        let storage_balance = contract
            .storage_balance_of(contract.connector.owner_id.clone())
            .unwrap();
        assert_eq!(storage_balance.total.0, 0);
        assert_eq!(storage_balance.available.0, 0);
    }

    fn create_contract() -> EthConnectorContract {
        let prover_account = "prover.near".parse().unwrap();
        let eth_custodian_address = Address::from_array([0xab; 20]);
        let metadata = FungibleTokenMetadata {
            spec: FT_METADATA_SPEC.to_string(),
            name: "Ether".to_string(),
            symbol: "ETH".to_string(),
            icon: None,
            reference: None,
            reference_hash: None,
            decimals: 18,
        };
        let account_with_access_right = "engine.near".parse().unwrap();
        let owner_id = "owner.near".parse().unwrap();
        EthConnectorContract::new(
            prover_account,
            eth_custodian_address,
            &metadata,
            account_with_access_right,
            &owner_id,
        )
    }
}
