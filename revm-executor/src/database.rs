use crate::utils::{collect_account_proofs, collect_storage_proofs};
use bus_mapping::{
    circuit_input_builder::l2::update_codedb,
    state_db,
    state_db::{CodeDB, StateDB},
};
use eth_types::{l2_types::BlockTrace, ToBigEndian, ToWord, Word, H160, H256};
use log::{trace, Level};
use mpt_zktrie::state::{AccountData, ZktrieState};
use revm::{
    db::DatabaseRef,
    primitives::{AccountInfo, Address, Bytecode, B256, U256},
    DatabaseCommit,
};
use std::{collections::HashMap, convert::Infallible};
use zktrie::ZkTrie;

pub struct EvmDatabase {
    tx_id: usize,
    code_db: CodeDB,
    pub(crate) sdb: StateDB,
    zktrie: ZkTrie,
}

impl EvmDatabase {
    pub fn new(l2_trace: &BlockTrace) -> Self {
        let mut sdb = StateDB::new();
        for parsed in
            ZktrieState::parse_account_from_proofs(collect_account_proofs(&l2_trace.storage_trace))
        {
            let (addr, acc) = parsed.unwrap();
            trace!("insert account {:?} {:?}", addr, acc);
            sdb.set_account(&addr, state_db::Account::from(&acc));
        }

        for parsed in
            ZktrieState::parse_storage_from_proofs(collect_storage_proofs(&l2_trace.storage_trace))
        {
            let ((addr, key), val) = parsed.unwrap();
            *sdb.get_storage_mut(&addr, &key).1 = val.into();
        }

        let mut code_db = CodeDB::new();
        code_db.insert(Vec::new());
        update_codedb(&mut code_db, &sdb, &l2_trace).unwrap();

        let old_root = l2_trace.storage_trace.root_before;
        let zktrie_state = ZktrieState::from_trace_with_additional(
            old_root,
            collect_account_proofs(&l2_trace.storage_trace),
            collect_storage_proofs(&l2_trace.storage_trace),
            l2_trace
                .storage_trace
                .deletion_proofs
                .iter()
                .map(ethers_core::types::Bytes::as_ref),
        )
        .unwrap();
        let root = *zktrie_state.root();
        log::debug!("building partial statedb done, root {}", hex::encode(root));
        let mem_db = zktrie_state.into_inner();
        let zktrie = mem_db.new_trie(&root).unwrap();

        EvmDatabase {
            tx_id: 1,
            code_db,
            sdb,
            zktrie,
        }
    }

    pub fn root(&self) -> H256 {
        H256::from(self.zktrie.root())
    }
}

impl DatabaseRef for EvmDatabase {
    type Error = Infallible;

    /// Get basic account information.
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let (exist, acc) = self.sdb.get_account(&H160::from(**address));
        log::trace!("loaded account: {address:?}, exist: {exist}, acc: {acc:?}");
        if exist {
            let mut acc = AccountInfo {
                balance: U256::from_be_bytes(acc.balance.to_be_bytes()),
                nonce: acc.nonce.as_u64(),
                code_hash: B256::from(acc.code_hash.to_fixed_bytes()),
                keccak_code_hash: B256::from(acc.keccak_code_hash.to_fixed_bytes()),
                // if None, code_by_hash will be used to fetch it if code needs to be loaded from
                // inside revm.
                code: None,
            };
            let code = self
                .code_db
                .0
                .get(&H256(*acc.code_hash))
                .cloned()
                .unwrap_or_default();
            let bytecode = Bytecode::new_raw(revm::primitives::Bytes::from(code.to_vec()));
            acc.code = Some(bytecode);
            Ok(Some(acc))
        } else {
            Ok(None)
        }
    }

    /// Get account code by its hash.
    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
        panic!("Should not be called. Code is already loaded");
    }

    /// Get storage value of address at index.
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let (_, val) = self.sdb.get_storage(
            &H160::from(**address),
            &eth_types::U256::from_little_endian(index.as_le_slice()),
        );
        Ok(U256::from_be_bytes(val.to_be_bytes()))
    }

    /// Get block hash by block number.
    fn block_hash_ref(&self, _: U256) -> Result<B256, Self::Error> {
        unimplemented!("BLOCKHASH is disabled")
    }
}

impl revm::Database for EvmDatabase {
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        DatabaseRef::basic_ref(self, address)
    }

    fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        panic!("Should not be called. Code is already loaded");
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        DatabaseRef::storage_ref(self, address, index)
    }

    fn block_hash(&mut self, _: U256) -> Result<B256, Self::Error> {
        unimplemented!("BLOCKHASH is disabled")
    }
}

impl DatabaseCommit for EvmDatabase {
    fn commit(&mut self, changes: HashMap<Address, revm::primitives::Account>) {
        for (addr, incoming) in changes {
            let addr = H160::from(**addr);
            let (_, acc) = self.sdb.get_account_mut(&addr);
            let is_empty = acc.is_empty();
            if is_empty && incoming.is_empty() {
                continue;
            }

            if log::log_enabled!(Level::Trace) {
                let mut incoming = incoming.clone();
                incoming.info.code = None;
                trace!(
                    "commit: addr: {:?}, acc: {:?}, old: {:?}",
                    addr,
                    incoming,
                    acc
                );
            }

            let mut acc_data = self
                .zktrie
                .get_account(addr.as_bytes())
                .map(AccountData::from)
                .unwrap_or_default();

            if !incoming.storage.is_empty() {
                // get current storage root
                let storage_root_before = acc_data.storage_root;
                // get storage tire
                let mut storage_tire = self
                    .zktrie
                    .get_db()
                    .new_trie(storage_root_before.as_fixed_bytes())
                    .expect("unable to get storage trie");

                for (storage_key, slot) in incoming.storage.iter() {
                    if !slot.present_value().is_zero() {
                        acc.storage.insert(
                            eth_types::U256::from_little_endian(storage_key.as_le_slice()),
                            eth_types::U256::from_little_endian(slot.present_value().as_le_slice()),
                        );

                        storage_tire
                            .update_store(
                                &storage_key.to_be_bytes::<32>(),
                                &slot.present_value().to_be_bytes(),
                            )
                            .expect("failed to update storage");
                    } else if !slot.original_value().is_zero() {
                        acc.storage.remove(&eth_types::U256::from_little_endian(
                            storage_key.as_le_slice(),
                        ));
                        storage_tire.delete(&storage_key.to_be_bytes::<32>());
                    }
                }

                acc_data.storage_root = H256::from(storage_tire.root());
            }

            let new_balance = Word::from_little_endian(incoming.info.balance.as_le_slice());
            if acc.balance != new_balance {
                acc.balance = new_balance;
                acc_data.balance = new_balance;
            }

            if acc.nonce.as_u64() != incoming.info.nonce {
                acc.nonce = incoming.info.nonce.to_word();
                acc_data.nonce = incoming.info.nonce;
            }

            if (is_empty && !incoming.is_empty())
                || acc.code_hash != H256::from(*incoming.info.code_hash)
            {
                let poseidon_code_hash = H256::from(incoming.info.code_hash.0);
                let keccak_code_hash = H256::from(incoming.info.keccak_code_hash.0);
                let code_size = incoming
                    .info
                    .code
                    .as_ref()
                    .map(|c| c.len())
                    .unwrap_or_default();

                acc.code_hash = poseidon_code_hash;
                acc.keccak_code_hash = keccak_code_hash;
                acc.code_size = code_size.to_word();

                acc_data.poseidon_code_hash = poseidon_code_hash;
                acc_data.keccak_code_hash = keccak_code_hash;
                acc_data.code_size = code_size as u64;
            }

            self.zktrie
                .update_account(addr.as_bytes(), &acc_data.into())
                .expect("failed to update account");
        }

        self.tx_id += 1;
    }
}
