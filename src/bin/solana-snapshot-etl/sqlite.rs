use borsh::BorshDeserialize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{error, warn};
use rusqlite::{Connection};
use solana_sdk::program_pack::Pack;
use solana_snapshot_etl::append_vec::{AppendVec, StoredAccountMeta};
use solana_snapshot_etl::parallel::{AppendVecConsumer, GenericResult};
use solana_snapshot_etl::{append_vec_iter, AppendVecIterator};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use rusqlite::types::Null;
use solana_sdk::bs58;

use crate::mpl_metadata;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) struct SqliteIndexer {
    db: Connection,
    db_path: PathBuf,
    db_temp_guard: TempFileGuard,

    multi_progress: MultiProgress,
    progress: Arc<Progress>,
}

struct Progress {
    accounts_counter: ProgressCounter,
    token_accounts_counter: ProgressCounter,
    metaplex_accounts_counter: ProgressCounter,
}

pub(crate) struct IndexStats {
    pub(crate) accounts_total: u64,
    pub(crate) token_accounts_total: u64,
}

impl SqliteIndexer {
    pub(crate) fn new(db_path: PathBuf) -> Result<Self> {
        // Create temporary DB file, which gets promoted on success.
        let temp_file_name = format!("_{}.tmp", db_path.file_name().unwrap().to_string_lossy());
        let db_temp_path = db_path.with_file_name(&temp_file_name);
        let _ = std::fs::remove_file(&db_temp_path);
        let db_temp_guard = TempFileGuard::new(db_temp_path.clone());

        // Open database.
        let db = Self::create_db(&db_temp_path)?;


        // Create progress bars.
        let spinner_style = ProgressStyle::with_template(
            "{prefix:>13.bold.dim} {spinner} rate={per_sec:>13} total={human_pos:>11}",
        )
            .unwrap();
        let multi_progress = MultiProgress::new();
        let accounts_counter = ProgressCounter::new(
            multi_progress.add(
                ProgressBar::new_spinner()
                    .with_style(spinner_style.clone())
                    .with_prefix("accs"),
            ),
        );
        let token_accounts_counter = ProgressCounter::new(
            multi_progress.add(
                ProgressBar::new_spinner()
                    .with_style(spinner_style.clone())
                    .with_prefix("token_accs"),
            ),
        );
        let metaplex_accounts_counter = ProgressCounter::new(
            multi_progress.add(
                ProgressBar::new_spinner()
                    .with_style(spinner_style)
                    .with_prefix("metaplex_accs"),
            ),
        );

        Ok(Self {
            db,
            db_path,
            db_temp_guard,

            multi_progress,
            progress: Arc::new(Progress {
                accounts_counter,
                token_accounts_counter,
                metaplex_accounts_counter,
            }),
        })
    }

    fn create_db(path: &Path) -> Result<Connection> {
        let db = Connection::open(&path)?;
        db.pragma_update(None, "synchronous", false)?;
        db.pragma_update(None, "journal_mode", "off")?;
        db.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        db.pragma_update(None, "temp_store", "MEMORY")?;
        db.pragma_update(None, "cache_size", -2000000)?;
        db.execute(
            "\
CREATE TABLE account  (
    pubkey BLOB(32) NOT NULL PRIMARY KEY,
    data_len INTEGER(8) NOT NULL,
    owner BLOB(32) NOT NULL,
    lamports INTEGER(8) NOT NULL,
    executable INTEGER(1) NOT NULL,
    rent_epoch INTEGER(8) NOT NULL
);",
            [],
        )?;
        db.execute(
            "\
CREATE TABLE token_mint (
    pubkey TEXT(32) NOT NULL PRIMARY KEY,
    mint_authority BLOB(32) NULL,
    supply INTEGER(8) NOT NULL,
    decimals INTEGER(2) NOT NULL,
    is_initialized BOOL NOT NULL,
    freeze_authority BLOB(32) NULL
);",
            [],
        )?;
        db.execute(
            "\
CREATE TABLE token_account (
    pubkey TEXT(32) NOT NULL PRIMARY KEY,
    mint TEXT(32) NOT NULL,
    owner TEXT(32) NOT NULL,
    amount INTEGER(8) NOT NULL,
    delegate TEXT(32),
    state INTEGER(1) NOT NULL,
    is_native INTEGER(8),
    delegated_amount INTEGER(8) NOT NULL,
    close_authority BLOB(32)
);",
            [],
        )?;
        db.execute(
            "\
CREATE TABLE token_multisig (
    pubkey BLOB(32) NOT NULL,
    signer BLOB(32) NOT NULL,
    m INTEGER(2) NOT NULL,
    n INTEGER(2) NOT NULL,
    PRIMARY KEY (pubkey, signer)
);",
            [],
        )?;
        db.execute(
            "\
CREATE TABLE token_metadata (
    pubkey TEXT(32) NOT NULL,
    mint TEXT(32) NOT NULL,
    name TEXT(32) NOT NULL,
    update_authority TEXT(32) NULL,
    creators TEXT NULL,
    symbol TEXT(10) NOT NULL,
    uri TEXT(200) NOT NULL,
    seller_fee_basis_points INTEGER(4) NOT NULL,
    primary_sale_happened INTEGER(1) NOT NULL,
    is_mutable INTEGER(1) NOT NULL,
    edition_nonce INTEGER(2) NULL,
    collection_verified INTEGER(1) NULL,
    collection_key BLOB(32) NULL
);",
            [],
        )?;
        db.execute("CREATE INDEX token_metadata_mint_idx ON token_metadata(mint);", [])?;
        db.execute("CREATE INDEX token_account_mint_idx ON token_account(mint);", [])?;
        Ok(db)
    }

    pub(crate) fn set_cache_size(&mut self, size_mib: i64) -> Result<()> {
        let size = size_mib * 1024;
        self.db.pragma_update(None, "cache_size", -size)?;
        Ok(())
    }

    pub(crate) fn insert_all(mut self, iterator: AppendVecIterator) -> Result<IndexStats> {
        let mut worker: Worker = Worker {
            db: &self.db,
            progress: Arc::clone(&self.progress),
            token_metadata_counter: 0,
            token_metadata_params: Vec::new(),
            batch_size: 200,
            token_mint_counter: 0,
            token_mint_params: Vec::new(),
            token_account_counter: 0,
            token_account_params: Vec::new(),
        };

        for append_vec in iterator {
            worker.on_append_vec(append_vec?)?;
        }

        if worker.token_metadata_counter > 0 {
            worker.insert_token_metadata_metadata()?;
        }

        if worker.token_mint_counter > 0 {
            worker.insert_token_mint()?;
        }

        if worker.token_account_counter > 0 {
            worker.insert_token_account()?;
        }

        self.db.pragma_update(None, "query_only", true)?;
        let stats = IndexStats {
            accounts_total: self.progress.accounts_counter.get(),
            token_accounts_total: self.progress.token_accounts_counter.get(),
        };
        self.db_temp_guard.promote(self.db_path)?;
        let _ = &self.multi_progress;
        Ok(stats)
    }
}

struct Worker<'a> {
    db: &'a Connection,
    progress: Arc<Progress>,
    token_metadata_counter: i32,
    token_metadata_params: Vec<rusqlite::types::Value>,
    batch_size: i32,

    token_mint_counter: i32,
    token_mint_params: Vec<rusqlite::types::Value>,

    token_account_counter: i32,
    token_account_params: Vec<rusqlite::types::Value>,
}

impl<'a> AppendVecConsumer for Worker<'a> {
    fn on_append_vec(&mut self, append_vec: AppendVec) -> GenericResult<()> {
        for acc in append_vec_iter(Rc::new(append_vec)) {
            self.insert_account(&acc.access().unwrap())?;
        }
        Ok(())
    }
}

impl<'a> Worker<'a> {
    fn insert_account(&mut self, account: &StoredAccountMeta) -> Result<()> {
        // self.insert_account_meta(account)?;
        if account.account_meta.owner == spl_token::id() {
            self.insert_token(account)?;
        }
        if account.account_meta.owner == mpl_metadata::id() {
            self.insert_token_metadata(account)?;
        }
        self.progress.accounts_counter.inc();
        Ok(())
    }

//     fn insert_account_meta(&mut self, account: &StoredAccountMeta) -> Result<()> {
//         let mut account_insert = self.db.prepare_cached(
//             "\
// INSERT OR REPLACE INTO account (pubkey, data_len, owner, lamports, executable, rent_epoch)
//     VALUES (?, ?, ?, ?, ?, ?);",
//         )?;
//         account_insert.insert(params![
//             account.meta.pubkey.as_ref(),
//             account.meta.data_len as i64,
//             account.account_meta.owner.as_ref(),
//             account.account_meta.lamports as i64,
//             account.account_meta.executable,
//             account.account_meta.rent_epoch as i64,
//         ])?;
//         Ok(())
//     }

    fn insert_token(&mut self, account: &StoredAccountMeta) -> Result<()> {
        match account.meta.data_len as usize {
            spl_token::state::Account::LEN => {
                if let Ok(token_account) = spl_token::state::Account::unpack(account.data) {
                    let p = self.prepare_token_account_params(account, &token_account)?;
                    self.token_account_params.extend(p);
                    self.token_account_counter = self.token_account_counter + 1;

                    if self.token_account_counter == self.batch_size {
                        self.insert_token_account()?;
                    }
                }
            }
            spl_token::state::Mint::LEN => {
                if let Ok(token_mint) = spl_token::state::Mint::unpack(account.data) {
                    if token_mint.supply as i64 == 1 {
                        let p = self.prepare_token_mint_params(account, &token_mint)?;
                        self.token_mint_params.extend(p);
                        self.token_mint_counter = self.token_mint_counter + 1;

                        if self.token_mint_counter == self.batch_size {
                            self.insert_token_mint()?;
                        }
                    }
                }
            }
            spl_token::state::Multisig::LEN => {
                if let Ok(_token_multisig) = spl_token::state::Multisig::unpack(account.data) {
                    // self.insert_token_multisig(account, &token_multisig)?;
                }
            }
            _ => {
                warn!(
                    "Token program account {} has unexpected size {}",
                    account.meta.pubkey, account.meta.data_len
                );
                return Ok(());
            }
        }
        self.progress.token_accounts_counter.inc();
        Ok(())
    }

    fn prepare_token_account_params(
        &mut self,
        account: &StoredAccountMeta,
        token_account: &spl_token::state::Account,
    ) -> Result<Vec<rusqlite::types::Value>> {
        let p = vec![
            rusqlite::types::Value::from(bs58::encode(account.meta.pubkey.as_ref()).into_string()),
            rusqlite::types::Value::from(bs58::encode(token_account.mint.as_ref()).into_string()),
            rusqlite::types::Value::from(bs58::encode(token_account.owner.as_ref()).into_string()),
            rusqlite::types::Value::from(token_account.amount as i64),
            rusqlite::types::Value::from(Option::<String>::from(token_account.delegate.map(|key| bs58::encode(key.as_ref()).into_string()))),
            rusqlite::types::Value::from(token_account.state as u8),
            rusqlite::types::Value::from(Option::<i64>::from(token_account.is_native.map(|i| i as i64))),
            rusqlite::types::Value::from(token_account.delegated_amount as i64),
            rusqlite::types::Value::from(Option::<String>::from(token_account.close_authority.map(|key| bs58::encode(key.as_ref()).into_string()))),
        ];

        Ok(p)
    }

    fn insert_token_account(
        &mut self,
    ) -> Result<()> {
        let mut with_params = " (?, ?, ?, ?, ?, ?, ?, ?, ?),".repeat(self.token_account_counter as usize);
        with_params.pop();
        let with_params = with_params.as_str();

        let st = format!("INSERT OR REPLACE INTO token_account (pubkey, mint, owner, amount, delegate, state, is_native, delegated_amount, close_authority) VALUES {}", with_params);
        let mut stmt = self.db.prepare_cached(st.as_str()).unwrap();
        stmt.execute(rusqlite::params_from_iter(&self.token_account_params)).unwrap();
        self.token_account_params.clear();
        self.token_account_counter = 0;
        Ok(())
    }

    fn prepare_token_mint_params(
        &mut self,
        account: &StoredAccountMeta,
        token_mint: &spl_token::state::Mint,
    ) -> Result<Vec<rusqlite::types::Value>> {
        let p = vec![
            rusqlite::types::Value::from(bs58::encode(account.meta.pubkey.as_ref()).into_string()),
            rusqlite::types::Value::from(Option::<String>::from(token_mint.mint_authority.map(|key| bs58::encode(key.as_ref()).into_string()))),
            rusqlite::types::Value::from(token_mint.supply as i64),
            rusqlite::types::Value::from(token_mint.decimals),
            rusqlite::types::Value::from(token_mint.is_initialized),
            rusqlite::types::Value::from(Option::<String>::from(token_mint.freeze_authority.map(|key| bs58::encode(key.as_ref()).into_string()))),
        ];

        Ok(p)
    }

    fn insert_token_mint(
        &mut self,
    ) -> Result<()> {
        let mut with_params = " (?, ?, ?, ?, ?, ?),".repeat(self.token_mint_counter as usize);
        with_params.pop();
        let with_params = with_params.as_str();

        let st = format!("INSERT OR REPLACE INTO token_mint (pubkey, mint_authority, supply, decimals, is_initialized, freeze_authority) VALUES {}", with_params);
        let mut stmt = self.db.prepare_cached(st.as_str()).unwrap();
        stmt.execute(rusqlite::params_from_iter(&self.token_mint_params)).unwrap();
        self.token_mint_params.clear();
        self.token_mint_counter = 0;
        Ok(())
    }

//     fn insert_token_multisig(
//         &mut self,
//         account: &StoredAccountMeta,
//         token_multisig: &spl_token::state::Multisig,
//     ) -> Result<()> {
//         let mut token_multisig_insert = self.db.prepare_cached(
//             "\
// INSERT OR REPLACE INTO token_multisig (pubkey, signer, m, n)
//     VALUES (?, ?, ?, ?);",
//         )?;
//         for signer in &token_multisig.signers[..token_multisig.n as usize] {
//             token_multisig_insert.insert(params![
//                 account.meta.pubkey.as_ref(),
//                 signer.as_ref(),
//                 token_multisig.m,
//                 token_multisig.n
//             ])?;
//         }
//         Ok(())
//     }

    fn insert_token_metadata(&mut self, account: &StoredAccountMeta) -> Result<()> {
        if account.data.is_empty() {
            return Ok(());
        }
        let mut data_peek = account.data;
        let account_key = match mpl_metadata::AccountKey::deserialize(&mut data_peek) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        match account_key {
            mpl_metadata::AccountKey::MetadataV1 => {
                let meta_v1 = mpl_metadata::Metadata::deserialize(&mut data_peek).map_err(|e| {
                    format!(
                        "Invalid token-metadata v1 metadata acc {}: {}",
                        account.meta.pubkey, e
                    )
                })?;

                let meta_v1_1 = mpl_metadata::MetadataExt::deserialize(&mut data_peek).ok();
                let meta_v1_2 = meta_v1_1
                    .as_ref()
                    .and_then(|_| mpl_metadata::MetadataExtV1_2::deserialize(&mut data_peek).ok());

                let p = self.prepare_token_metadata_params(
                    account,
                    &meta_v1,
                    meta_v1_1.as_ref(),
                    meta_v1_2.as_ref(),
                )?;

                self.token_metadata_params.extend(p);
                self.token_metadata_counter = self.token_metadata_counter + 1;
            }
            _ => return Ok(()), // TODO
        }

        if self.token_metadata_counter == self.batch_size {
            self.insert_token_metadata_metadata()?;
        }

        self.progress.metaplex_accounts_counter.inc();
        Ok(())
    }

    fn prepare_token_metadata_params(
        &self,
        account: &StoredAccountMeta,
        meta_v1: &mpl_metadata::Metadata,
        meta_v1_1: Option<&mpl_metadata::MetadataExt>,
        meta_v1_2: Option<&mpl_metadata::MetadataExtV1_2>,
    ) -> Result<Vec<rusqlite::types::Value>> {
        let collection = meta_v1_2.as_ref().and_then(|m| m.collection.as_ref());
        let p = vec![
            rusqlite::types::Value::from(bs58::encode(account.meta.pubkey.as_ref().to_owned()).into_string()),
            rusqlite::types::Value::from(bs58::encode(meta_v1.mint.as_ref()).into_string()),
            rusqlite::types::Value::from(meta_v1.data.name.clone()),
            rusqlite::types::Value::from(bs58::encode(meta_v1.update_authority.as_ref()).into_string()),
            if !meta_v1.data.creators.is_none() {
                rusqlite::types::Value::from(Some(serde_json::to_string(&meta_v1.data.creators.as_ref().unwrap().iter().map(|c| bs58::encode(c.address.as_ref()).into_string()).collect::<Vec<String>>())?))
            } else { rusqlite::types::Value::from(Null) },
            rusqlite::types::Value::from(meta_v1.data.symbol.clone()),
            rusqlite::types::Value::from(meta_v1.data.uri.clone()),
            rusqlite::types::Value::from(meta_v1.data.seller_fee_basis_points),
            rusqlite::types::Value::from(meta_v1.primary_sale_happened),
            rusqlite::types::Value::from(meta_v1.is_mutable),
            rusqlite::types::Value::from(meta_v1_1.map(|c| c.edition_nonce)),
            rusqlite::types::Value::from(collection.map(|c| c.verified)),
            rusqlite::types::Value::from(collection.map(|c| bs58::encode(c.key.as_ref()).into_string())),
        ];
        Ok(p)
    }

    fn insert_token_metadata_metadata(
        &mut self,
    ) -> Result<()> {
        let mut with_params = " (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?),".repeat(self.token_metadata_counter as usize);
        with_params.pop();
        let with_params = with_params.as_str();

        let st = format!("\
        INSERT OR REPLACE INTO token_metadata (
    pubkey,
    mint,
    name,
    update_authority,
    creators,
    symbol,
    uri,
    seller_fee_basis_points,
    primary_sale_happened,
    is_mutable,
    edition_nonce,
    collection_verified,
    collection_key
) VALUES {}", with_params);
        let mut stmt = self.db.prepare_cached(st.as_str()).unwrap();
        stmt.execute(rusqlite::params_from_iter(&self.token_metadata_params)).unwrap();
        self.token_metadata_params.clear();
        self.token_metadata_counter = 0;
        Ok(())
    }
}

struct ProgressCounter {
    progress_bar: Mutex<ProgressBar>,
    counter: AtomicU64,
}

impl ProgressCounter {
    fn new(progress_bar: ProgressBar) -> Self {
        Self {
            progress_bar: Mutex::new(progress_bar),
            counter: AtomicU64::new(0),
        }
    }

    fn get(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    fn inc(&self) {
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        if count % 1024 == 0 {
            self.progress_bar.lock().unwrap().set_position(count)
        }
    }
}

impl Drop for ProgressCounter {
    fn drop(&mut self) {
        let progress_bar = self.progress_bar.lock().unwrap();
        progress_bar.set_position(self.get());
        progress_bar.finish();
    }
}

struct TempFileGuard {
    pub path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn promote<P: AsRef<Path>>(&mut self, new_name: P) -> std::io::Result<()> {
        std::fs::rename(
            self.path.take().expect("cannot promote non-existent file"),
            new_name,
        )
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(e) = std::fs::remove_file(path) {
                error!("Failed to remove temp DB: {}", e);
            }
        }
    }
}
