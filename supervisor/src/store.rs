use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    Binding, Digest, Error, Limits, NativeResult, Operation, Outcome, Receipt, Result, RuntimeId,
    WorkerId,
};

pub(crate) struct Store {
    db: Connection,
    // Never unlink the lock: replacing its inode would allow a second live owner.
    _lock: File,
    pub id: WorkerId,
    pub epoch: u64,
    pub binding: Binding,
    pub limits: Limits,
    poisoned: bool,
}

impl Store {
    pub fn open(path: &Path, binding: Binding, limits: Limits) -> Result<Self> {
        crate::validate_binding(&binding)?;
        validate_limits(limits)?;
        // The host durably provisions this private directory. Creating unsynced
        // ancestors here could lose the entire store after a power failure despite
        // successful SQLite commits inside it.
        if !std::fs::metadata(path)?.is_dir() {
            return Err(Error::InvalidInput);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(path.join("owner.lock"))?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                Error::Owned
            } else {
                Error::Io(error)
            }
        })?;
        let database = path.join("worker.sqlite3");
        // Check before SQLite can read/cache an unexpectedly large existing database.
        match std::fs::metadata(&database) {
            Ok(metadata) if metadata.len() > u64::from(limits.database_bytes) => {
                return Err(Error::Capacity);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut db = Connection::open(database)?;
        db.busy_timeout(Duration::ZERO)?;
        db.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=EXTRA;
             PRAGMA fullfsync=ON;
             PRAGMA mmap_size=0;
             PRAGMA cache_size=-256;
             PRAGMA temp_store=MEMORY;",
        )?;
        let page_size: u32 = db.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        db.pragma_update(None, "max_page_count", limits.database_bytes / page_size)?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            let tables: u32 =
                db.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            if tables != 0 {
                return Err(Error::InvalidStore);
            }
            let mut id = [0; 16];
            getrandom::getrandom(&mut id).map_err(|_| Error::Random)?;
            let tx = db.transaction()?;
            tx.execute_batch(
                "CREATE TABLE worker (
                    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                    id BLOB NOT NULL CHECK(length(id)=16),
                    provider TEXT NOT NULL, session TEXT NOT NULL,
                    operations INTEGER NOT NULL, controls INTEGER NOT NULL,
                    payload_bytes INTEGER NOT NULL, result_bytes INTEGER NOT NULL,
                    database_bytes INTEGER NOT NULL,
                    epoch INTEGER NOT NULL, birth INTEGER NOT NULL,
                    live INTEGER
                 );
                 CREATE TABLE operations (
                    id TEXT PRIMARY KEY, digest BLOB NOT NULL CHECK(length(digest)=32),
                    kind INTEGER NOT NULL CHECK(kind IN (0,1)), target TEXT,
                    birth INTEGER NOT NULL, state INTEGER NOT NULL CHECK(state IN (0,1,2)),
                    turn TEXT, result BLOB
                 );
                 PRAGMA user_version=1;",
            )?;
            tx.execute(
                "INSERT INTO worker VALUES(1,?1,?2,?3,?4,?5,?6,?7,?8,0,0,NULL)",
                params![
                    id.as_slice(),
                    binding.provider,
                    binding.session,
                    limits.operations,
                    limits.controls,
                    limits.payload_bytes,
                    limits.result_bytes,
                    limits.database_bytes
                ],
            )?;
            tx.commit()?;
        } else if version != 1 {
            return Err(Error::InvalidStore);
        }
        let (id, stored_binding, stored_limits) = db.query_row(
            "SELECT id,provider,session,operations,controls,payload_bytes,result_bytes,database_bytes FROM worker WHERE singleton=1",
            [], |r| Ok((r.get::<_, Vec<u8>>(0)?, Binding {provider:r.get(1)?, session:r.get(2)?},
                (r.get::<_, u32>(3)?,r.get::<_, u32>(4)?,r.get::<_, u32>(5)?,r.get::<_, u32>(6)?,r.get::<_, u32>(7)?))),
        )?;
        if binding != stored_binding {
            return Err(Error::BindingMismatch);
        }
        if stored_limits
            != (
                limits.operations,
                limits.controls,
                limits.payload_bytes,
                limits.result_bytes,
                limits.database_bytes,
            )
        {
            return Err(Error::InvalidInput);
        }
        let id = WorkerId(id.try_into().map_err(|_| Error::InvalidStore)?);
        let epoch: i64 = db.query_row("SELECT epoch FROM worker", [], |r| r.get(0))?;
        let epoch = epoch
            .checked_add(1)
            .filter(|n| *n > 0)
            .ok_or(Error::InvalidStore)?;
        db.execute("UPDATE worker SET epoch=?1", [epoch])?;
        Ok(Self {
            db,
            _lock: lock,
            id,
            epoch: epoch as u64,
            binding,
            limits,
            poisoned: false,
        })
    }

    pub fn healthy(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    // Any write error might be an ambiguous commit. Poison even SQLITE_FULL so this
    // owner cannot assume rollback and dispatch again. Stop does not use this path.
    fn write<T>(
        &mut self,
        action: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T> {
        self.healthy()?;
        match action(&mut self.db) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.poisoned = true;
                Err(error.into())
            }
        }
    }

    pub fn live(&self) -> Result<Option<RuntimeId>> {
        let generation: Option<u64> = self
            .db
            .query_row("SELECT live FROM worker", [], |r| r.get(0))?;
        Ok(generation.map(|generation| RuntimeId {
            worker: self.id,
            generation,
        }))
    }

    pub fn begin_runtime(&mut self) -> Result<RuntimeId> {
        self.healthy()?;
        if self.live()?.is_some() {
            return Err(Error::RecoveryRequired);
        }
        let birth: i64 = self
            .db
            .query_row("SELECT birth FROM worker", [], |r| r.get(0))?;
        let birth = birth
            .checked_add(1)
            .filter(|n| *n > 0)
            .ok_or(Error::InvalidStore)?;
        self.write(|db| db.execute("UPDATE worker SET birth=?1,live=?1", [birth]))?;
        Ok(RuntimeId {
            worker: self.id,
            generation: birth as u64,
        })
    }

    pub fn clear_runtime(&mut self, runtime: RuntimeId) -> Result<()> {
        if self.live()? != Some(runtime) {
            return Err(Error::Stale);
        }
        self.write(|db| db.execute("UPDATE worker SET live=NULL", []))?;
        Ok(())
    }

    pub fn receipt(&self, id: &str) -> Result<Option<Receipt>> {
        self.healthy()?;
        let row = self
            .db
            .query_row(
                "SELECT digest,kind,target,birth,state,turn,result FROM operations WHERE id=?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, u8>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, u64>(3)?,
                        r.get::<_, u8>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<Vec<u8>>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((digest, kind, target, birth, state, turn, result)) = row else {
            return Ok(None);
        };
        let operation = match (kind, target) {
            (0, None) => Operation::Submit,
            (1, Some(target)) if crate::text_ok(&target, 128) => Operation::Interrupt(target),
            _ => return Err(Error::InvalidStore),
        };
        let outcome = match (state, turn, result) {
            (0, None, None) => Outcome::SubmittedUnknown,
            (1, Some(turn), None) if crate::text_ok(&turn, 1024) => Outcome::Accepted(turn),
            (2, Some(turn), Some(result))
                if crate::text_ok(&turn, 1024)
                    && result.len() <= self.limits.result_bytes as usize =>
            {
                Outcome::Terminal {
                    turn,
                    result: NativeResult(result),
                }
            }
            _ => return Err(Error::InvalidStore),
        };
        Ok(Some(Receipt {
            id: id.to_owned(),
            operation,
            digest: Digest(digest.try_into().map_err(|_| Error::InvalidStore)?),
            runtime: RuntimeId {
                worker: self.id,
                generation: birth,
            },
            outcome,
        }))
    }

    pub fn unresolved(&self) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE kind=0 AND state!=2)",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn insert(&mut self, receipt: &Receipt) -> Result<()> {
        let (kind, target, capacity) = match &receipt.operation {
            Operation::Submit => (0, None, self.limits.operations),
            Operation::Interrupt(target) => (1, Some(target), self.limits.controls),
        };
        let count: u32 = self.db.query_row(
            "SELECT count(*) FROM operations WHERE kind=?1",
            [kind],
            |r| r.get(0),
        )?;
        if count >= capacity {
            return Err(Error::Capacity);
        }
        self.write(|db| {
            db.execute(
                "INSERT INTO operations VALUES(?1,?2,?3,?4,?5,0,NULL,NULL)",
                params![
                    receipt.id,
                    receipt.digest.0.as_slice(),
                    kind,
                    target,
                    receipt.runtime.generation
                ],
            )
        })?;
        Ok(())
    }

    pub fn save(&mut self, receipt: &Receipt) -> Result<()> {
        let (state, turn, result) = match &receipt.outcome {
            Outcome::SubmittedUnknown => (0, None, None),
            Outcome::Accepted(turn) => (1, Some(turn), None),
            Outcome::Terminal { turn, result } => (2, Some(turn), Some(&result.0)),
        };
        self.write(|db| {
            let tx = db.transaction()?;
            tx.execute("UPDATE operations SET state=?2,turn=?3,result=?4 WHERE id=?1", params![receipt.id,state,turn,result])?;
            // Terminal turn evidence also settles every cancellation of that turn.
            if state == 2 && receipt.operation == Operation::Submit {
                tx.execute("UPDATE operations SET state=2,turn=?2,result=?3 WHERE kind=1 AND target=?1 AND birth=?4", params![receipt.id,turn,result,receipt.runtime.generation])?;
            }
            tx.commit()
        })
    }
}

fn validate_limits(limits: Limits) -> Result<()> {
    let count = u64::from(limits.operations) + u64::from(limits.controls);
    // Reserve both result growth and B-tree/index overhead for every admitted ID.
    let reserved = count * (u64::from(limits.result_bytes) + 8192) + 65536;
    if limits.operations == 0
        || limits.controls == 0
        || limits.payload_bytes == 0
        || limits.payload_bytes > 16 * 1024 * 1024
        || limits.result_bytes == 0
        || limits.result_bytes > 1024 * 1024
        || limits.database_bytes > 1024 * 1024 * 1024
        || reserved > u64::from(limits.database_bytes)
        || limits.leases == 0
        || limits.leases > 1024
        || limits.lease_ttl.is_zero()
        || limits.detached_ttl.is_zero()
        || limits.lease_ttl > Duration::from_secs(86400)
        || limits.detached_ttl > Duration::from_secs(365 * 86400)
    {
        return Err(Error::InvalidInput);
    }
    Ok(())
}
