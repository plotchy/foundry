//! The in memory DB

use crate::{
    eth::backend::db::{
        AsHashDB, Db, MaybeHashDatabase, SerializableAccountRecord, SerializableState, StateDb,
    },
    mem::state::{state_merkle_trie_root, trie_hash_db},
    revm::primitives::AccountInfo,
    Address, U256,
};
use ethers::prelude::H256;
use foundry_evm::utils::h160_to_b160;
use tracing::{trace, warn};

// reexport for convenience
use crate::mem::state::storage_trie_db;
use foundry_evm::executor::backend::{snapshot::StateSnapshot, DatabaseResult};
pub use foundry_evm::executor::{backend::MemDb, DatabaseRef};

impl Db for MemDb {
    fn insert_account(&mut self, address: Address, account: AccountInfo) {
        self.inner.insert_account_info(address.into(), account)
    }

    fn set_storage_at(&mut self, address: Address, slot: U256, val: U256) -> DatabaseResult<()> {
        self.inner.insert_account_storage(address.into(), slot.into(), val.into())
    }

    fn insert_block_hash(&mut self, number: U256, hash: H256) {
        self.inner.block_hashes.insert(number.into(), hash.into());
    }

    fn dump_state(&self) -> DatabaseResult<Option<SerializableState>> {
        let accounts = self
            .inner
            .accounts
            .clone()
            .into_iter()
            .map(|(k, v)| -> DatabaseResult<_> {
                let code = if let Some(code) = v.info.code {
                    code
                } else {
                    self.inner.code_by_hash(v.info.code_hash)?
                }
                .to_checked();
                Ok((
                    k.into(),
                    SerializableAccountRecord {
                        nonce: v.info.nonce,
                        balance: v.info.balance.into(),
                        code: code.bytes()[..code.len()].to_vec().into(),
                        storage: v.storage.into_iter().map(|k| (k.0.into(), k.1.into())).collect(),
                    },
                ))
            })
            .collect::<Result<_, _>>()?;

        Ok(Some(SerializableState { accounts }))
    }

    /// Creates a new snapshot
    fn snapshot(&mut self) -> U256 {
        let id = self.snapshots.insert(self.inner.clone());
        trace!(target: "backend::memdb", "Created new snapshot {}", id);
        id
    }

    fn revert(&mut self, id: U256) -> bool {
        if let Some(snapshot) = self.snapshots.remove(id) {
            self.inner = snapshot;
            trace!(target: "backend::memdb", "Reverted snapshot {}", id);
            true
        } else {
            warn!(target: "backend::memdb", "No snapshot to revert for {}", id);
            false
        }
    }

    fn maybe_state_root(&self) -> Option<H256> {
        Some(state_merkle_trie_root(&self.inner.accounts))
    }

    fn current_state(&self) -> StateDb {
        StateDb::new(MemDb { inner: self.inner.clone(), ..Default::default() })
    }
}

impl MaybeHashDatabase for MemDb {
    fn maybe_as_hash_db(&self) -> Option<(AsHashDB, H256)> {
        Some(trie_hash_db(&self.inner.accounts))
    }

    fn maybe_account_db(&self, addr: Address) -> Option<(AsHashDB, H256)> {
        if let Some(acc) = self.inner.accounts.get(&h160_to_b160(addr)) {
            Some(storage_trie_db(&acc.storage))
        } else {
            Some(storage_trie_db(&Default::default()))
        }
    }

    fn clear_into_snapshot(&mut self) -> StateSnapshot {
        self.inner.clear_into_snapshot()
    }

    fn clear(&mut self) {
        self.inner.clear();
    }

    fn init_from_snapshot(&mut self, snapshot: StateSnapshot) {
        self.inner.init_from_snapshot(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        eth::backend::db::{Db, SerializableAccountRecord, SerializableState},
        revm::primitives::AccountInfo,
        Address,
    };
    use bytes::Bytes;
    use ethers::types::U256;
    use foundry_evm::{
        executor::{backend::MemDb, DatabaseRef},
        revm::primitives::{Bytecode, KECCAK_EMPTY, U256 as rU256},
    };
    use std::{collections::BTreeMap, str::FromStr};

    // verifies that all substantial aspects of a loaded account remain the state after an account
    // is dumped and reloaded
    #[test]
    fn test_dump_reload_cycle() {
        let test_addr: Address =
            Address::from_str("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266").unwrap();

        let mut dump_db = MemDb::default();

        let contract_code: Bytecode =
            Bytecode::new_raw(Bytes::from("fake contract code")).to_checked();

        dump_db.insert_account(
            test_addr,
            AccountInfo {
                balance: rU256::from(123456),
                code_hash: KECCAK_EMPTY,
                code: Some(contract_code.clone()),
                nonce: 1234,
            },
        );

        dump_db.set_storage_at(test_addr, "0x1234567".into(), "0x1".into()).unwrap();

        let state = dump_db.dump_state().unwrap().unwrap();

        let mut load_db = MemDb::default();

        load_db.load_state(state).unwrap();

        let loaded_account = load_db.basic(test_addr.into()).unwrap().unwrap();

        assert_eq!(loaded_account.balance, rU256::from(123456));
        assert_eq!(load_db.code_by_hash(loaded_account.code_hash).unwrap(), contract_code);
        assert_eq!(loaded_account.nonce, 1234);
        assert_eq!(
            load_db.storage(test_addr.into(), Into::<U256>::into("0x1234567").into()).unwrap(),
            Into::<U256>::into("0x1").into()
        );
    }

    // verifies that multiple accounts can be loaded at a time, and storage is merged within those
    // accounts as well.
    #[test]
    fn test_load_state_merge() {
        let test_addr: Address =
            Address::from_str("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266").unwrap();
        let test_addr2: Address =
            Address::from_str("0x70997970c51812dc3a010c7d01b50e0d17dc79c8").unwrap();

        let contract_code: Bytecode =
            Bytecode::new_raw(Bytes::from("fake contract code")).to_checked();

        let mut db = MemDb::default();

        db.insert_account(
            test_addr,
            AccountInfo {
                balance: rU256::from(123456),
                code_hash: KECCAK_EMPTY,
                code: Some(contract_code.clone()),
                nonce: 1234,
            },
        );

        db.set_storage_at(test_addr, "0x1234567".into(), "0x1".into()).unwrap();
        db.set_storage_at(test_addr, "0x1234568".into(), "0x2".into()).unwrap();

        let mut new_state = SerializableState::default();

        new_state.accounts.insert(
            test_addr2,
            SerializableAccountRecord {
                balance: Default::default(),
                code: Default::default(),
                nonce: 1,
                storage: Default::default(),
            },
        );

        let mut new_storage = BTreeMap::default();
        new_storage.insert("0x1234568".into(), "0x5".into());

        new_state.accounts.insert(
            test_addr,
            SerializableAccountRecord {
                balance: 100100.into(),
                code: contract_code.bytes()[..contract_code.len()].to_vec().into(),
                nonce: 100,
                storage: new_storage,
            },
        );

        db.load_state(new_state).unwrap();

        let loaded_account = db.basic(test_addr.into()).unwrap().unwrap();
        let loaded_account2 = db.basic(test_addr2.into()).unwrap().unwrap();

        assert_eq!(loaded_account2.nonce, 1);

        assert_eq!(loaded_account.balance, rU256::from(100100));
        assert_eq!(db.code_by_hash(loaded_account.code_hash).unwrap(), contract_code);
        assert_eq!(loaded_account.nonce, 1234);
        assert_eq!(
            db.storage(test_addr.into(), Into::<U256>::into("0x1234567").into()).unwrap(),
            Into::<U256>::into("0x1").into()
        );
        assert_eq!(
            db.storage(test_addr.into(), Into::<U256>::into("0x1234568").into()).unwrap(),
            Into::<U256>::into("0x5").into()
        );
    }
}
