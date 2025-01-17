// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::storage::mvcc::{GcInfo, MvccReader, MvccTxn, Result as MvccResult, MAX_TXN_WRITE_SIZE};
use crate::storage::Snapshot;
use txn_types::{Key, TimeStamp, Write, WriteType};

pub fn gc<'a, S: Snapshot>(
    txn: &'a mut MvccTxn,
    reader: &'a mut MvccReader<S>,
    key: Key,
    save_points: Vec<TimeStamp>,
) -> MvccResult<GcInfo> {
    let gc = Gc::new(txn, reader, key);
    let info = gc.run(save_points)?;
    info.report_metrics();

    Ok(info)
}

/// Iterates over the versions of `key`, see the `run` method.
struct Gc<'a, S: Snapshot> {
    key: Key,
    cur_ts: TimeStamp,
    info: GcInfo,
    txn: &'a mut MvccTxn,
    reader: &'a mut MvccReader<S>,
}

impl<'a, S: Snapshot> Gc<'a, S> {
    fn new(txn: &'a mut MvccTxn, reader: &'a mut MvccReader<S>, key: Key) -> Gc<'a, S> {
        Gc {
            key,
            cur_ts: TimeStamp::max(),
            info: GcInfo {
                found_versions: 0,
                deleted_versions: 0,
                is_completed: false,
            },
            txn,
            reader,
        }
    }

    fn delete_write(&mut self, write: Write, ts: TimeStamp) {
        self.txn.delete_write(self.key.clone(), ts);
        if write.write_type == WriteType::Put && write.short_value.is_none() {
            self.txn.delete_value(self.key.clone(), write.start_ts);
        }
        self.info.deleted_versions += 1;
    }

    fn next_write(&mut self) -> MvccResult<Option<(TimeStamp, Write)>> {
        let result = self.reader.seek_write(&self.key, self.cur_ts)?;
        if let Some((commit, _)) = result {
            self.cur_ts = commit.prev();
            self.info.found_versions += 1;
        }
        Ok(result)
    }

    fn run(mut self, save_points: Vec<TimeStamp>) -> MvccResult<GcInfo> {
        let mut state = State::Rewind(save_points);

        while let Some((commit, write)) = self.next_write()? {
            if self.txn.write_size >= MAX_TXN_WRITE_SIZE {
                return Ok(self.info);
            }

            state.step(&mut self, write, commit);
        }

        match state {
            State::RemoveIdempotent(_, Some((commit, write)))
            | State::RemoveAll(_, Some((commit, write))) => {
                self.delete_write(write, commit);
            }
            _ => {}
        };

        self.info.is_completed = true;
        Ok(self.info)
    }
}

enum State {
    // Rewind to TimeStamp.
    Rewind(Vec<TimeStamp>),
    // Remove locks and rollbacks until we get to a put or delete.
    RemoveIdempotent(Vec<TimeStamp>, Option<(TimeStamp, Write)>),
    // Parameter is the latest delete which can be removed if we complete removal of
    // everything else.
    RemoveAll(Vec<TimeStamp>, Option<(TimeStamp, Write)>),
}

impl State {
    /// Process a single version of a key/value.
    fn step(&mut self, gc: &mut Gc<'_, impl Snapshot>, write: Write, commit_ts: TimeStamp) {
        match self {
            State::Rewind(save_points) => {
                let last = save_points.last().unwrap();
                if commit_ts <= *last {
                    let mut sp = save_points.to_owned();
                    sp.pop();
                    *self = State::RemoveIdempotent(sp, None);
                    self.step(gc, write, commit_ts);
                }
            }
            State::RemoveIdempotent(save_points, last_delete) => match save_points.last() {
                Some(last) if commit_ts <= *last => {
                    let mut sp = save_points.to_owned();
                    sp.pop();
                    *self = State::RemoveIdempotent(sp, last_delete.clone());
                    self.step(gc, write, commit_ts);
                }
                _ => match write.write_type {
                    WriteType::Put => {
                        *self = State::RemoveAll(save_points.to_owned(), None);
                    }
                    WriteType::Delete => {
                        *self = State::RemoveAll(save_points.to_owned(), Some((commit_ts, write)));
                    }
                    WriteType::Rollback | WriteType::Lock => {
                        gc.delete_write(write, commit_ts);
                    }
                },
            },
            State::RemoveAll(save_points, last_delete) => match save_points.last() {
                Some(last) if commit_ts <= *last => {
                    save_points.pop();
                    *self = State::RemoveIdempotent(save_points.to_owned(), last_delete.clone());
                    self.step(gc, write, commit_ts);
                }
                _ => gc.delete_write(write, commit_ts),
            },
        }
    }
}

pub mod tests {
    use super::*;
    use crate::storage::kv::SnapContext;
    use crate::storage::mvcc::tests::write;
    use crate::storage::{Engine, ScanMode};
    use concurrency_manager::ConcurrencyManager;
    use kvproto::kvrpcpb::Context;

    #[cfg(test)]
    use crate::storage::{
        mvcc::tests::{must_get, must_get_none},
        txn::tests::*,
        RocksEngine, TestEngineBuilder,
    };
    #[cfg(test)]
    use txn_types::SHORT_VALUE_MAX_LEN;

    pub fn must_succeed<E: Engine>(engine: &E, key: &[u8], safe_point: impl Into<TimeStamp>) {
        let ctx = SnapContext::default();
        let snapshot = engine.snapshot(ctx).unwrap();
        let cm = ConcurrencyManager::new(1.into());
        let mut txn = MvccTxn::new(TimeStamp::zero(), cm);
        let mut reader = MvccReader::new(snapshot, Some(ScanMode::Forward), true);
        gc(
            &mut txn,
            &mut reader,
            Key::from_raw(key),
            vec![safe_point.into()],
        )
        .unwrap();
        write(engine, &Context::default(), txn.into_modifies());
    }

    #[cfg(test)]
    fn test_gc_imp<F>(k: &[u8], v1: &[u8], v2: &[u8], v3: &[u8], v4: &[u8], gc: F)
    where
        F: Fn(&RocksEngine, &[u8], u64),
    {
        let engine = TestEngineBuilder::new().build().unwrap();

        must_prewrite_put(&engine, k, v1, k, 5);
        must_commit(&engine, k, 5, 10);
        must_prewrite_put(&engine, k, v2, k, 15);
        must_commit(&engine, k, 15, 20);
        must_prewrite_delete(&engine, k, k, 25);
        must_commit(&engine, k, 25, 30);
        must_prewrite_put(&engine, k, v3, k, 35);
        must_commit(&engine, k, 35, 40);
        must_prewrite_lock(&engine, k, k, 45);
        must_commit(&engine, k, 45, 50);
        must_prewrite_put(&engine, k, v4, k, 55);
        must_rollback(&engine, k, 55, false);

        // Transactions:
        // startTS commitTS Command
        // --
        // 55      -        PUT "x55" (Rollback)
        // 45      50       LOCK
        // 35      40       PUT "x35"
        // 25      30       DELETE
        // 15      20       PUT "x15"
        //  5      10       PUT "x5"

        // CF data layout:
        // ts CFDefault   CFWrite
        // --
        // 55             Rollback(PUT,50)
        // 50             Commit(LOCK,45)
        // 45
        // 40             Commit(PUT,35)
        // 35   x35
        // 30             Commit(Delete,25)
        // 25
        // 20             Commit(PUT,15)
        // 15   x15
        // 10             Commit(PUT,5)
        // 5    x5

        gc(&engine, k, 12);
        must_get(&engine, k, 12, v1);

        gc(&engine, k, 22);
        must_get(&engine, k, 22, v2);
        must_get_none(&engine, k, 12);

        gc(&engine, k, 32);
        must_get_none(&engine, k, 22);
        must_get_none(&engine, k, 35);

        gc(&engine, k, 60);
        must_get(&engine, k, 62, v3);
    }

    #[test]
    fn test_gc() {
        test_gc_imp(b"k1", b"v1", b"v2", b"v3", b"v4", must_succeed);

        let v1 = "x".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        let v2 = "y".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        let v3 = "z".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        let v4 = "v".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        test_gc_imp(b"k2", &v1, &v2, &v3, &v4, must_succeed);
    }

    #[test]
    fn test_gc_with_compaction_filter() {
        use crate::server::gc_worker::gc_by_compact;

        test_gc_imp(b"zk1", b"v1", b"v2", b"v3", b"v4", gc_by_compact);

        let v1 = "x".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        let v2 = "y".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        let v3 = "z".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        let v4 = "v".repeat(SHORT_VALUE_MAX_LEN + 1).into_bytes();
        test_gc_imp(b"zk2", &v1, &v2, &v3, &v4, gc_by_compact);
    }
}
