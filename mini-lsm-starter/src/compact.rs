// Copyright (c) 2022-2025 Alex Chi Z
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

mod leveled;
mod simple_leveled;
mod tiered;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::iterators::StorageIterator;
use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::key::KeySlice;
use crate::lsm_storage::{LsmStorageInner, LsmStorageState};
use crate::table::{SsTable, SsTableBuilder, SsTableIterator};
use anyhow::Result;
pub use leveled::{LeveledCompactionController, LeveledCompactionOptions, LeveledCompactionTask};
use serde::{Deserialize, Serialize};
pub use simple_leveled::{
    SimpleLeveledCompactionController, SimpleLeveledCompactionOptions, SimpleLeveledCompactionTask,
};
pub use tiered::{TieredCompactionController, TieredCompactionOptions, TieredCompactionTask};

#[derive(Debug, Serialize, Deserialize)]
pub enum CompactionTask {
    Leveled(LeveledCompactionTask),
    Tiered(TieredCompactionTask),
    Simple(SimpleLeveledCompactionTask),
    ForceFullCompaction {
        l0_sstables: Vec<usize>,
        l1_sstables: Vec<usize>,
    },
}

impl CompactionTask {
    fn compact_to_bottom_level(&self) -> bool {
        match self {
            CompactionTask::ForceFullCompaction { .. } => true,
            CompactionTask::Leveled(task) => task.is_lower_level_bottom_level,
            CompactionTask::Simple(task) => task.is_lower_level_bottom_level,
            CompactionTask::Tiered(task) => task.bottom_tier_included,
        }
    }
}

pub(crate) enum CompactionController {
    Leveled(LeveledCompactionController),
    Tiered(TieredCompactionController),
    Simple(SimpleLeveledCompactionController),
    NoCompaction,
}

impl CompactionController {
    pub fn generate_compaction_task(&self, snapshot: &LsmStorageState) -> Option<CompactionTask> {
        match self {
            CompactionController::Leveled(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Leveled),
            CompactionController::Simple(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Simple),
            CompactionController::Tiered(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Tiered),
            CompactionController::NoCompaction => unreachable!(),
        }
    }

    pub fn apply_compaction_result(
        &self,
        snapshot: &LsmStorageState,
        task: &CompactionTask,
        output: &[usize],
        in_recovery: bool,
    ) -> (LsmStorageState, Vec<usize>) {
        match (self, task) {
            (CompactionController::Leveled(ctrl), CompactionTask::Leveled(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output, in_recovery)
            }
            (CompactionController::Simple(ctrl), CompactionTask::Simple(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            (CompactionController::Tiered(ctrl), CompactionTask::Tiered(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            _ => unreachable!(),
        }
    }
}

impl CompactionController {
    pub fn flush_to_l0(&self) -> bool {
        matches!(
            self,
            Self::Leveled(_) | Self::Simple(_) | Self::NoCompaction
        )
    }
}

#[derive(Debug, Clone)]
pub enum CompactionOptions {
    /// Leveled compaction with partial compaction + dynamic level support (= RocksDB's Leveled
    /// Compaction)
    Leveled(LeveledCompactionOptions),
    /// Tiered compaction (= RocksDB's universal compaction)
    Tiered(TieredCompactionOptions),
    /// Simple leveled compaction
    Simple(SimpleLeveledCompactionOptions),
    /// In no compaction mode (week 1), always flush to L0
    NoCompaction,
}

impl LsmStorageInner {
    fn compact(&self, _task: &CompactionTask) -> Result<Vec<Arc<SsTable>>> {
        let snapshot = {
            let guard = self.state.read();
            guard
        };
        let output = match _task {
            CompactionTask::Leveled(_) => {
                unimplemented!()
            }
            CompactionTask::Tiered(_) => {
                unimplemented!()
            }
            CompactionTask::Simple(_) => {
                unimplemented!()
            }
            CompactionTask::ForceFullCompaction {
                l0_sstables,
                l1_sstables,
            } => {
                let mut l0_it = Vec::with_capacity(l0_sstables.len());
                for id in l0_sstables {
                    let sst = snapshot.sstables.get(id).unwrap();
                    l0_it.push(Box::new(SsTableIterator::create_and_seek_to_first(
                        sst.clone(),
                    )?));
                }
                let mut l1_tables = Vec::with_capacity(l1_sstables.len());
                for id in l1_sstables {
                    let sst = snapshot.sstables.get(id).unwrap();
                    l1_tables.push(sst.clone());
                }

                let l1_it = SstConcatIterator::create_and_seek_to_first(l1_tables)?;

                let it = TwoMergeIterator::create(MergeIterator::create(l0_it), l1_it)?;

                self.build_ssts(_task, it)?
            }
        };
        Ok(output)
    }

    fn build_ssts(
        &self,
        _task: &CompactionTask,
        mut it: impl for<'a> StorageIterator<KeyType<'a> = KeySlice<'a>>,
    ) -> Result<Vec<Arc<SsTable>>> {
        let mut builder = None;
        let mut output = Vec::new();

        while it.is_valid() {
            if builder.is_none() {
                builder = Some(SsTableBuilder::new(self.options.block_size));
            }

            let builder_inner = builder.as_mut().unwrap();

            if (_task.compact_to_bottom_level()) {
                if !it.value().is_empty() {
                    builder_inner.add(it.key(), it.value());
                }
            } else {
                builder_inner.add(it.key(), it.value());
            }

            it.next()?;

            if builder_inner.estimated_size() >= self.options.target_sst_size {
                let sst_id = self.next_sst_id();
                let builder = builder.take().unwrap();
                let sst = builder.build(
                    sst_id,
                    Some(self.block_cache.clone()),
                    self.path_of_sst(sst_id),
                )?;
                output.push(Arc::new(sst));
            }
        }
        if let Some(builder) = builder {
            let sst_id = self.next_sst_id();
            let sst = builder.build(
                sst_id,
                Some(self.block_cache.clone()),
                self.path_of_sst(sst_id),
            )?;
            output.push(Arc::new(sst));
        }

        Ok(output)
    }

    pub fn force_full_compaction(&self) -> Result<()> {
        let snapshot = {
            let guard = self.state.read();
            guard.clone()
        };
        let l0 = snapshot.l0_sstables.clone();
        let l1 = snapshot.levels[0].1.clone();

        let compaction_task = CompactionTask::ForceFullCompaction {
            l0_sstables: l0.clone(),
            l1_sstables: l1.clone(),
        };
        println!("force full compaction: {:?}", compaction_task);

        let new_ssts = self.compact(&compaction_task)?;
        let mut ids = Vec::with_capacity(new_ssts.len());

        {
            let state_lock = self.state_lock.lock();
            let mut state = self.state.read().as_ref().clone();

            // remove old sst(s) by id
            for sst_id in l0.iter().chain(l1.iter()) {
                let removed = state.sstables.remove(sst_id);
                assert!(removed.is_some());
            }

            // insert new sst(s)
            for new_sst in new_ssts {
                ids.push(new_sst.sst_id());
                let result = state.sstables.insert(new_sst.sst_id(), new_sst);
                assert!(result.is_none());
            }

            assert_eq!(l1, state.levels[0].1);

            // new sst(s) ids go to level 1
            state.levels[0].1.clone_from(&ids);

            let mut l0_map = l0.iter().copied().collect::<HashSet<_>>();

            // keep l0 sstables we can't remove?
            state.l0_sstables = state
                .l0_sstables
                .iter()
                .filter(|sst_id| !l0_map.remove(sst_id))
                .copied()
                .collect::<Vec<_>>();

            // l0 should be empty
            assert!(l0_map.is_empty());

            *self.state.write() = Arc::new(state);
        }

        for sst in l0.iter().chain(l1.iter()) {
            std::fs::remove_file(self.path_of_sst(*sst))?;
        }

        Ok(())
    }

    fn trigger_compaction(&self) -> Result<()> {
        let total_imm_tables = {
            let guard = self.state.read();
            guard.imm_memtables.len()
        };

        if total_imm_tables >= self.options.num_memtable_limit {
            self.force_flush_next_imm_memtable()?;
        }

        Ok(())
    }

    pub(crate) fn spawn_compaction_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        if let CompactionOptions::Leveled(_)
        | CompactionOptions::Simple(_)
        | CompactionOptions::Tiered(_) = self.options.compaction_options
        {
            let this = self.clone();
            let handle = std::thread::spawn(move || {
                let ticker = crossbeam_channel::tick(Duration::from_millis(50));
                loop {
                    crossbeam_channel::select! {
                        recv(ticker) -> _ => if let Err(e) = this.trigger_compaction() {
                            eprintln!("compaction failed: {}", e);
                        },
                        recv(rx) -> _ => return
                    }
                }
            });
            return Ok(Some(handle));
        }
        Ok(None)
    }

    fn trigger_flush(&self) -> Result<()> {
        self.force_flush_next_imm_memtable()?;
        Ok(())
    }

    pub(crate) fn spawn_flush_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        let this = self.clone();
        let handle = std::thread::spawn(move || {
            let ticker = crossbeam_channel::tick(Duration::from_millis(50));
            loop {
                crossbeam_channel::select! {
                    recv(ticker) -> _ => if let Err(e) = this.trigger_flush() {
                        eprintln!("flush failed: {}", e);
                    },
                    recv(rx) -> _ => return
                }
            }
        });
        Ok(Some(handle))
    }
}
