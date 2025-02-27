// Copyright 2023 RisingWave Labs
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

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use itertools::Itertools;
use risingwave_common::catalog::TableId;
use risingwave_pb::hummock::group_delta::DeltaType;
use risingwave_pb::hummock::hummock_version::Levels;
use risingwave_pb::hummock::hummock_version_delta::GroupDeltas;
use risingwave_pb::hummock::{
    CompactionConfig, CompatibilityVersion, GroupConstruct, GroupDestroy, GroupMetaChange,
    GroupTableChange, HummockVersion, HummockVersionDelta, Level, LevelType, OverlappingLevel,
    PbLevelType, SstableInfo,
};
use tracing::warn;

use super::StateTableId;
use crate::compaction_group::StaticCompactionGroupId;
use crate::key_range::KeyRangeCommon;
use crate::prost_key_range::KeyRangeExt;
use crate::table_watermark::PbTableWatermarksExt;
use crate::{can_concat, CompactionGroupId, HummockSstableId, HummockSstableObjectId};

pub struct GroupDeltasSummary {
    pub delete_sst_levels: Vec<u32>,
    pub delete_sst_ids_set: HashSet<u64>,
    pub insert_sst_level_id: u32,
    pub insert_sub_level_id: u64,
    pub insert_table_infos: Vec<SstableInfo>,
    pub group_construct: Option<GroupConstruct>,
    pub group_destroy: Option<GroupDestroy>,
    pub group_meta_changes: Vec<GroupMetaChange>,
    pub group_table_change: Option<GroupTableChange>,
}

pub fn summarize_group_deltas(group_deltas: &GroupDeltas) -> GroupDeltasSummary {
    let mut delete_sst_levels = Vec::with_capacity(group_deltas.group_deltas.len());
    let mut delete_sst_ids_set = HashSet::new();
    let mut insert_sst_level_id = u32::MAX;
    let mut insert_sub_level_id = u64::MAX;
    let mut insert_table_infos = vec![];
    let mut group_construct = None;
    let mut group_destroy = None;
    let mut group_meta_changes = vec![];
    let mut group_table_change = None;

    for group_delta in &group_deltas.group_deltas {
        match group_delta.get_delta_type().unwrap() {
            DeltaType::IntraLevel(intra_level) => {
                if !intra_level.removed_table_ids.is_empty() {
                    delete_sst_levels.push(intra_level.level_idx);
                    delete_sst_ids_set.extend(intra_level.removed_table_ids.iter().clone());
                }
                if !intra_level.inserted_table_infos.is_empty() {
                    insert_sst_level_id = intra_level.level_idx;
                    insert_sub_level_id = intra_level.l0_sub_level_id;
                    insert_table_infos.extend(intra_level.inserted_table_infos.iter().cloned());
                }
            }
            DeltaType::GroupConstruct(construct_delta) => {
                assert!(group_construct.is_none());
                group_construct = Some(construct_delta.clone());
            }
            DeltaType::GroupDestroy(destroy_delta) => {
                assert!(group_destroy.is_none());
                group_destroy = Some(destroy_delta.clone());
            }
            DeltaType::GroupMetaChange(meta_delta) => {
                group_meta_changes.push(meta_delta.clone());
            }
            DeltaType::GroupTableChange(meta_delta) => {
                group_table_change = Some(meta_delta.clone());
            }
        }
    }

    delete_sst_levels.sort();
    delete_sst_levels.dedup();

    GroupDeltasSummary {
        delete_sst_levels,
        delete_sst_ids_set,
        insert_sst_level_id,
        insert_sub_level_id,
        insert_table_infos,
        group_construct,
        group_destroy,
        group_meta_changes,
        group_table_change,
    }
}

#[derive(Clone, Default)]
pub struct TableGroupInfo {
    pub group_id: CompactionGroupId,
    pub group_size: u64,
    pub table_statistic: HashMap<StateTableId, u64>,
    pub split_by_table: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SstDeltaInfo {
    pub insert_sst_level: u32,
    pub insert_sst_infos: Vec<SstableInfo>,
    pub delete_sst_object_ids: Vec<HummockSstableObjectId>,
}

pub type BranchedSstInfo = HashMap<CompactionGroupId, /* SST Id */ HummockSstableId>;

#[easy_ext::ext(HummockVersionExt)]
impl HummockVersion {
    pub fn get_compaction_group_levels(&self, compaction_group_id: CompactionGroupId) -> &Levels {
        self.levels
            .get(&compaction_group_id)
            .unwrap_or_else(|| panic!("compaction group {} does not exist", compaction_group_id))
    }

    pub fn get_compaction_group_levels_mut(
        &mut self,
        compaction_group_id: CompactionGroupId,
    ) -> &mut Levels {
        self.levels
            .get_mut(&compaction_group_id)
            .unwrap_or_else(|| panic!("compaction group {} does not exist", compaction_group_id))
    }

    pub fn get_combined_levels(&self) -> impl Iterator<Item = &'_ Level> + '_ {
        self.levels.values().flat_map(|level| {
            level
                .l0
                .as_ref()
                .unwrap()
                .sub_levels
                .iter()
                .rev()
                .chain(level.levels.iter())
        })
    }

    /// This function does NOT dedup.
    pub fn get_object_ids(&self) -> Vec<u64> {
        self.get_combined_levels()
            .flat_map(|level| {
                level
                    .table_infos
                    .iter()
                    .map(|table_info| table_info.get_object_id())
            })
            .collect_vec()
    }

    pub fn level_iter<F: FnMut(&Level) -> bool>(
        &self,
        compaction_group_id: CompactionGroupId,
        mut f: F,
    ) {
        if let Some(levels) = self.levels.get(&compaction_group_id) {
            for sub_level in &levels.l0.as_ref().unwrap().sub_levels {
                if !f(sub_level) {
                    return;
                }
            }
            for level in &levels.levels {
                if !f(level) {
                    return;
                }
            }
        }
    }

    pub fn num_levels(&self, compaction_group_id: CompactionGroupId) -> usize {
        // l0 is currently separated from all levels
        self.levels
            .get(&compaction_group_id)
            .map(|group| group.levels.len() + 1)
            .unwrap_or(0)
    }
}

pub type SstSplitInfo = (
    // Object id.
    HummockSstableObjectId,
    // SST id.
    HummockSstableId,
    // Old SST id in parent group.
    HummockSstableId,
    // New SST id in parent group.
    HummockSstableId,
);

#[easy_ext::ext(HummockVersionUpdateExt)]
impl HummockVersion {
    pub fn count_new_ssts_in_group_split(
        &self,
        parent_group_id: CompactionGroupId,
        member_table_ids: HashSet<StateTableId>,
    ) -> u64 {
        self.levels
            .get(&parent_group_id)
            .map_or(0, |parent_levels| {
                parent_levels
                    .l0
                    .iter()
                    .flat_map(|l0| l0.get_sub_levels())
                    .chain(parent_levels.get_levels().iter())
                    .flat_map(|level| level.get_table_infos())
                    .map(|sst_info| {
                        // `sst_info.table_ids` will never be empty.
                        for table_id in sst_info.get_table_ids() {
                            if member_table_ids.contains(table_id) {
                                return 2;
                            }
                        }
                        0
                    })
                    .sum()
            })
    }

    pub fn init_with_parent_group(
        &mut self,
        parent_group_id: CompactionGroupId,
        group_id: CompactionGroupId,
        member_table_ids: HashSet<StateTableId>,
        new_sst_start_id: u64,
        allow_trivial_split: bool,
    ) -> Vec<SstSplitInfo> {
        let mut new_sst_id = new_sst_start_id;
        let mut split_id_vers = vec![];
        if parent_group_id == StaticCompactionGroupId::NewCompactionGroup as CompactionGroupId
            || !self.levels.contains_key(&parent_group_id)
        {
            return split_id_vers;
        }
        let [parent_levels, cur_levels] = self
            .levels
            .get_many_mut([&parent_group_id, &group_id])
            .unwrap();
        if let Some(ref mut l0) = parent_levels.l0 {
            for sub_level in &mut l0.sub_levels {
                let target_l0 = cur_levels.l0.as_mut().unwrap();
                // When `insert_hint` is `Ok(idx)`, it means that the sub level `idx` in `target_l0`
                // will extend these SSTs. When `insert_hint` is `Err(idx)`, it
                // means that we will add a new sub level `idx` into `target_l0`.
                let mut insert_hint = Err(target_l0.sub_levels.len());
                for (idx, other) in target_l0.sub_levels.iter_mut().enumerate() {
                    match other.sub_level_id.cmp(&sub_level.sub_level_id) {
                        Ordering::Less => {}
                        Ordering::Equal => {
                            insert_hint = Ok(idx);
                            break;
                        }
                        Ordering::Greater => {
                            insert_hint = Err(idx);
                            break;
                        }
                    }
                }
                // Remove SST from sub level may result in empty sub level. It will be purged
                // whenever another compaction task is finished.
                let insert_table_infos = split_sst_info_for_level(
                    &member_table_ids,
                    allow_trivial_split,
                    sub_level,
                    &mut split_id_vers,
                    &mut new_sst_id,
                );
                sub_level
                    .table_infos
                    .extract_if(|sst_info| sst_info.table_ids.is_empty())
                    .for_each(|sst_info| {
                        sub_level.total_file_size -= sst_info.file_size;
                        sub_level.uncompressed_file_size -= sst_info.uncompressed_file_size;
                        l0.total_file_size -= sst_info.file_size;
                        l0.uncompressed_file_size -= sst_info.uncompressed_file_size;
                    });
                if insert_table_infos.is_empty() {
                    continue;
                }
                match insert_hint {
                    Ok(idx) => {
                        add_ssts_to_sub_level(target_l0, idx, insert_table_infos);
                    }
                    Err(idx) => {
                        insert_new_sub_level(
                            target_l0,
                            sub_level.sub_level_id,
                            sub_level.level_type(),
                            insert_table_infos,
                            Some(idx),
                        );
                    }
                }
            }
        }
        for (idx, level) in parent_levels.levels.iter_mut().enumerate() {
            let insert_table_infos = split_sst_info_for_level(
                &member_table_ids,
                allow_trivial_split,
                level,
                &mut split_id_vers,
                &mut new_sst_id,
            );
            cur_levels.levels[idx].total_file_size += insert_table_infos
                .iter()
                .map(|sst| sst.file_size)
                .sum::<u64>();
            cur_levels.levels[idx].uncompressed_file_size += insert_table_infos
                .iter()
                .map(|sst| sst.uncompressed_file_size)
                .sum::<u64>();
            cur_levels.levels[idx]
                .table_infos
                .extend(insert_table_infos);
            cur_levels.levels[idx].table_infos.sort_by(|sst1, sst2| {
                let a = sst1.key_range.as_ref().unwrap();
                let b = sst2.key_range.as_ref().unwrap();
                a.compare(b)
            });
            assert!(can_concat(&cur_levels.levels[idx].table_infos));
            level
                .table_infos
                .extract_if(|sst_info| sst_info.table_ids.is_empty())
                .for_each(|sst_info| {
                    level.total_file_size -= sst_info.file_size;
                    level.uncompressed_file_size -= sst_info.uncompressed_file_size;
                });
        }
        split_id_vers
    }

    pub fn build_sst_delta_infos(&self, version_delta: &HummockVersionDelta) -> Vec<SstDeltaInfo> {
        let mut infos = vec![];

        for (group_id, group_deltas) in &version_delta.group_deltas {
            let mut info = SstDeltaInfo::default();

            let mut removed_l0_ssts: BTreeSet<u64> = BTreeSet::new();
            let mut removed_ssts: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();

            // Build only if all deltas are intra level deltas.
            if !group_deltas
                .group_deltas
                .iter()
                .all(|delta| matches!(delta.get_delta_type().unwrap(), DeltaType::IntraLevel(..)))
            {
                continue;
            }

            // TODO(MrCroxx): At most one insert delta is allowed here. It's okay for now with the
            // current `hummock::manager::gen_version_delta` implementation. Better refactor the
            // struct to reduce conventions.
            for group_delta in &group_deltas.group_deltas {
                if let DeltaType::IntraLevel(delta) = group_delta.get_delta_type().unwrap() {
                    if !delta.inserted_table_infos.is_empty() {
                        info.insert_sst_level = delta.level_idx;
                        info.insert_sst_infos
                            .extend(delta.inserted_table_infos.iter().cloned());
                    }
                    if !delta.removed_table_ids.is_empty() {
                        for id in &delta.removed_table_ids {
                            if delta.level_idx == 0 {
                                removed_l0_ssts.insert(*id);
                            } else {
                                removed_ssts.entry(delta.level_idx).or_default().insert(*id);
                            }
                        }
                    }
                }
            }

            let group = self.levels.get(group_id).unwrap();
            for l0_sub_level in &group.get_level0().sub_levels {
                for sst_info in &l0_sub_level.table_infos {
                    if removed_l0_ssts.remove(&sst_info.sst_id) {
                        info.delete_sst_object_ids.push(sst_info.object_id);
                    }
                }
            }
            for level in group.get_levels() {
                if let Some(mut removed_level_ssts) = removed_ssts.remove(&level.level_idx) {
                    for sst_info in &level.table_infos {
                        if removed_level_ssts.remove(&sst_info.sst_id) {
                            info.delete_sst_object_ids.push(sst_info.object_id);
                        }
                    }
                    if !removed_level_ssts.is_empty() {
                        tracing::error!(
                            "removed_level_ssts is not empty: {:?}",
                            removed_level_ssts,
                        );
                    }
                    debug_assert!(removed_level_ssts.is_empty());
                }
            }

            if !removed_l0_ssts.is_empty() || !removed_ssts.is_empty() {
                tracing::error!(
                    "not empty removed_l0_ssts: {:?}, removed_ssts: {:?}",
                    removed_l0_ssts,
                    removed_ssts
                );
            }
            debug_assert!(removed_l0_ssts.is_empty());
            debug_assert!(removed_ssts.is_empty());

            infos.push(info);
        }

        infos
    }

    pub fn apply_version_delta(
        &mut self,
        version_delta: &HummockVersionDelta,
    ) -> Vec<SstSplitInfo> {
        let mut sst_split_info = vec![];
        for (compaction_group_id, group_deltas) in &version_delta.group_deltas {
            let summary = summarize_group_deltas(group_deltas);
            if let Some(group_construct) = &summary.group_construct {
                let mut new_levels = build_initial_compaction_group_levels(
                    *compaction_group_id,
                    group_construct.get_group_config().unwrap(),
                );
                let parent_group_id = group_construct.parent_group_id;
                new_levels.parent_group_id = parent_group_id;
                new_levels.member_table_ids = group_construct.table_ids.clone();
                self.levels.insert(*compaction_group_id, new_levels);
                sst_split_info.extend(self.init_with_parent_group(
                    parent_group_id,
                    *compaction_group_id,
                    HashSet::from_iter(group_construct.table_ids.clone()),
                    group_construct.get_new_sst_start_id(),
                    group_construct.version() == CompatibilityVersion::VersionUnspecified,
                ));
            } else if let Some(group_change) = &summary.group_table_change {
                sst_split_info.extend(self.init_with_parent_group(
                    group_change.origin_group_id,
                    group_change.target_group_id,
                    HashSet::from_iter(group_change.table_ids.clone()),
                    group_change.new_sst_start_id,
                    group_change.version() == CompatibilityVersion::VersionUnspecified,
                ));

                let levels = self
                    .levels
                    .get_mut(&group_change.origin_group_id)
                    .expect("compaction group should exist");
                let mut moving_tables = levels
                    .member_table_ids
                    .extract_if(|t| group_change.table_ids.contains(t))
                    .collect_vec();
                self.levels
                    .get_mut(compaction_group_id)
                    .expect("compaction group should exist")
                    .member_table_ids
                    .append(&mut moving_tables);
            }
            let has_destroy = summary.group_destroy.is_some();
            let levels = self
                .levels
                .get_mut(compaction_group_id)
                .expect("compaction group should exist");

            for group_meta_delta in &summary.group_meta_changes {
                levels
                    .member_table_ids
                    .extend(group_meta_delta.table_ids_add.clone());
                levels
                    .member_table_ids
                    .retain(|t| !group_meta_delta.table_ids_remove.contains(t));
                levels.member_table_ids.sort();
            }

            assert!(
                self.max_committed_epoch <= version_delta.max_committed_epoch,
                "new max commit epoch {} is older than the current max commit epoch {}",
                version_delta.max_committed_epoch,
                self.max_committed_epoch
            );
            if self.max_committed_epoch < version_delta.max_committed_epoch {
                // `max_committed_epoch` increases. It must be a `commit_epoch`
                let GroupDeltasSummary {
                    delete_sst_levels,
                    delete_sst_ids_set,
                    insert_sst_level_id,
                    insert_sub_level_id,
                    insert_table_infos,
                    ..
                } = summary;
                assert!(
                    insert_sst_level_id == 0 || insert_table_infos.is_empty(),
                    "we should only add to L0 when we commit an epoch. Inserting into {} {:?}",
                    insert_sst_level_id,
                    insert_table_infos
                );
                assert!(
                    delete_sst_levels.is_empty() && delete_sst_ids_set.is_empty() || has_destroy,
                    "no sst should be deleted when committing an epoch"
                );
                if !insert_table_infos.is_empty() {
                    insert_new_sub_level(
                        levels.l0.as_mut().unwrap(),
                        insert_sub_level_id,
                        LevelType::Overlapping,
                        insert_table_infos,
                        None,
                    );
                }
            } else {
                // `max_committed_epoch` is not changed. The delta is caused by compaction.
                levels.apply_compact_ssts(summary);
            }
            if has_destroy {
                self.levels.remove(compaction_group_id);
            }
        }
        self.id = version_delta.id;
        self.max_committed_epoch = version_delta.max_committed_epoch;
        for (table_id, table_watermarks) in &version_delta.new_table_watermarks {
            match self.table_watermarks.entry(*table_id) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().apply_new_table_watermarks(table_watermarks);
                }
                Entry::Vacant(entry) => {
                    entry.insert(table_watermarks.clone());
                }
            }
        }
        if version_delta.safe_epoch != self.safe_epoch {
            assert!(version_delta.safe_epoch > self.safe_epoch);
            self.table_watermarks
                .values_mut()
                .for_each(|table_watermarks| {
                    table_watermarks.clear_stale_epoch_watermark(version_delta.safe_epoch)
                });
            self.safe_epoch = version_delta.safe_epoch;
        }
        sst_split_info
    }

    pub fn build_compaction_group_info(&self) -> HashMap<TableId, CompactionGroupId> {
        let mut ret = HashMap::new();
        for (compaction_group_id, levels) in &self.levels {
            for table_id in &levels.member_table_ids {
                ret.insert(TableId::new(*table_id), *compaction_group_id);
            }
        }
        ret
    }

    pub fn build_branched_sst_info(&self) -> BTreeMap<HummockSstableObjectId, BranchedSstInfo> {
        let mut ret: BTreeMap<_, _> = BTreeMap::new();
        for (compaction_group_id, group) in &self.levels {
            let mut levels = vec![];
            levels.extend(group.l0.as_ref().unwrap().sub_levels.iter());
            levels.extend(group.levels.iter());
            for level in levels {
                for table_info in &level.table_infos {
                    if table_info.sst_id == table_info.object_id {
                        continue;
                    }
                    let object_id = table_info.get_object_id();
                    let entry: &mut BranchedSstInfo = ret.entry(object_id).or_default();
                    if let Some(exist_sst_id) = entry.get(compaction_group_id) {
                        panic!("we do not allow more than one sst with the same object id in one grou. object-id: {}, duplicated sst id: {:?} and {}", object_id, exist_sst_id, table_info.sst_id);
                    }
                    entry.insert(*compaction_group_id, table_info.sst_id);
                }
            }
        }
        ret
    }
}

#[easy_ext::ext(HummockLevelsExt)]
impl Levels {
    pub fn get_level0(&self) -> &OverlappingLevel {
        self.l0.as_ref().unwrap()
    }

    pub fn get_level(&self, level_idx: usize) -> &Level {
        &self.levels[level_idx - 1]
    }

    pub fn get_level_mut(&mut self, level_idx: usize) -> &mut Level {
        &mut self.levels[level_idx - 1]
    }

    pub fn count_ssts(&self) -> usize {
        self.get_level0()
            .get_sub_levels()
            .iter()
            .chain(self.get_levels().iter())
            .map(|level| level.get_table_infos().len())
            .sum()
    }

    pub fn apply_compact_ssts(&mut self, summary: GroupDeltasSummary) {
        let GroupDeltasSummary {
            delete_sst_levels,
            delete_sst_ids_set,
            insert_sst_level_id,
            insert_sub_level_id,
            insert_table_infos,
            ..
        } = summary;

        if !self.check_deleted_sst_exist(&delete_sst_levels, delete_sst_ids_set.clone()) {
            warn!(
                "This VersionDelta may be committed by an expired compact task. Please check it. \n
                    delete_sst_levels: {:?}\n,
                    delete_sst_ids_set: {:?}\n,
                    insert_sst_level_id: {}\n,
                    insert_sub_level_id: {}\n,
                    insert_table_infos: {:?}\n",
                delete_sst_levels,
                delete_sst_ids_set,
                insert_sst_level_id,
                insert_sub_level_id,
                insert_table_infos
                    .iter()
                    .map(|sst| (sst.sst_id, sst.object_id))
                    .collect_vec()
            );
            return;
        }
        for level_idx in &delete_sst_levels {
            if *level_idx == 0 {
                for level in &mut self.l0.as_mut().unwrap().sub_levels {
                    level_delete_ssts(level, &delete_sst_ids_set);
                }
            } else {
                let idx = *level_idx as usize - 1;
                level_delete_ssts(&mut self.levels[idx], &delete_sst_ids_set);
            }
        }

        if !insert_table_infos.is_empty() {
            if insert_sst_level_id == 0 {
                let l0 = self.l0.as_mut().unwrap();
                let index = l0
                    .sub_levels
                    .partition_point(|level| level.sub_level_id < insert_sub_level_id);
                assert!(
                    index < l0.sub_levels.len() && l0.sub_levels[index].sub_level_id == insert_sub_level_id,
                    "should find the level to insert into when applying compaction generated delta. sub level idx: {},  removed sst ids: {:?}, sub levels: {:?},",
                    insert_sub_level_id, delete_sst_ids_set, l0.sub_levels.iter().map(|level| level.sub_level_id).collect_vec()
                );
                level_insert_ssts(&mut l0.sub_levels[index], insert_table_infos);
            } else {
                let idx = insert_sst_level_id as usize - 1;
                level_insert_ssts(&mut self.levels[idx], insert_table_infos);
            }
        }
        if delete_sst_levels.iter().any(|level_id| *level_id == 0) {
            self.l0
                .as_mut()
                .unwrap()
                .sub_levels
                .retain(|level| !level.table_infos.is_empty());
            self.l0.as_mut().unwrap().total_file_size = self
                .l0
                .as_mut()
                .unwrap()
                .sub_levels
                .iter()
                .map(|level| level.total_file_size)
                .sum::<u64>();
            self.l0.as_mut().unwrap().uncompressed_file_size = self
                .l0
                .as_mut()
                .unwrap()
                .sub_levels
                .iter()
                .map(|level| level.uncompressed_file_size)
                .sum::<u64>();
        }
    }

    pub fn check_deleted_sst_exist(
        &self,
        delete_sst_levels: &[u32],
        mut delete_sst_ids_set: HashSet<u64>,
    ) -> bool {
        for level_idx in delete_sst_levels {
            if *level_idx == 0 {
                for level in &self.l0.as_ref().unwrap().sub_levels {
                    level.table_infos.iter().for_each(|table| {
                        delete_sst_ids_set.remove(&table.sst_id);
                    });
                }
            } else {
                let idx = *level_idx as usize - 1;
                self.levels[idx].table_infos.iter().for_each(|table| {
                    delete_sst_ids_set.remove(&table.sst_id);
                });
            }
        }
        delete_sst_ids_set.is_empty()
    }
}

pub fn build_initial_compaction_group_levels(
    group_id: CompactionGroupId,
    compaction_config: &CompactionConfig,
) -> Levels {
    let mut levels = vec![];
    for l in 0..compaction_config.get_max_level() {
        levels.push(Level {
            level_idx: (l + 1) as u32,
            level_type: LevelType::Nonoverlapping as i32,
            table_infos: vec![],
            total_file_size: 0,
            sub_level_id: 0,
            uncompressed_file_size: 0,
        });
    }
    Levels {
        levels,
        l0: Some(OverlappingLevel {
            sub_levels: vec![],
            total_file_size: 0,
            uncompressed_file_size: 0,
        }),
        group_id,
        parent_group_id: StaticCompactionGroupId::NewCompactionGroup as _,
        member_table_ids: vec![],
    }
}

fn split_sst_info_for_level(
    member_table_ids: &HashSet<u32>,
    allow_trivial_split: bool,
    level: &mut Level,
    split_id_vers: &mut Vec<SstSplitInfo>,
    new_sst_id: &mut u64,
) -> Vec<SstableInfo> {
    // Remove SST from sub level may result in empty sub level. It will be purged
    // whenever another compaction task is finished.
    let mut insert_table_infos = vec![];
    for sst_info in &mut level.table_infos {
        let removed_table_ids = sst_info
            .table_ids
            .iter()
            .filter(|table_id| member_table_ids.contains(table_id))
            .cloned()
            .collect_vec();
        if !removed_table_ids.is_empty() {
            let is_trivial =
                allow_trivial_split && removed_table_ids.len() == sst_info.table_ids.len();
            let mut branch_table_info = sst_info.clone();
            branch_table_info.sst_id = *new_sst_id;
            *new_sst_id += 1;
            let parent_old_sst_id = sst_info.get_sst_id();
            if is_trivial {
                // This is a compatibility design. we only clear the table-ids for files which would
                // be removed in later code. In the version-delta generated by new
                // version meta-service, there will be no trivial split, and we will create
                // a reference for every sstable split to two groups.
                sst_info.table_ids.clear();
            } else {
                sst_info.sst_id = *new_sst_id;
                *new_sst_id += 1;
            }
            split_id_vers.push((
                branch_table_info.object_id,
                branch_table_info.sst_id,
                parent_old_sst_id,
                sst_info.sst_id,
            ));
            insert_table_infos.push(branch_table_info);
        }
    }
    insert_table_infos
}

pub fn try_get_compaction_group_id_by_table_id(
    version: &HummockVersion,
    table_id: StateTableId,
) -> Option<CompactionGroupId> {
    for (group_id, levels) in &version.levels {
        if levels.member_table_ids.contains(&table_id) {
            return Some(*group_id);
        }
    }
    None
}

/// Gets all compaction group ids.
pub fn get_compaction_group_ids(
    version: &HummockVersion,
) -> impl Iterator<Item = CompactionGroupId> + '_ {
    version.levels.keys().cloned()
}

/// Gets all member table ids.
pub fn get_member_table_ids(version: &HummockVersion) -> HashSet<StateTableId> {
    version
        .levels
        .iter()
        .flat_map(|(_, levels)| levels.member_table_ids.clone())
        .collect()
}

pub fn get_table_compaction_group_id_mapping(
    version: &HummockVersion,
) -> HashMap<StateTableId, CompactionGroupId> {
    version
        .levels
        .iter()
        .flat_map(|(group_id, levels)| {
            levels
                .member_table_ids
                .iter()
                .map(|table_id| (*table_id, *group_id))
        })
        .collect()
}

/// Gets all SSTs in `group_id`
pub fn get_compaction_group_ssts(
    version: &HummockVersion,
    group_id: CompactionGroupId,
) -> impl Iterator<Item = (HummockSstableObjectId, HummockSstableId)> + '_ {
    let group_levels = version.get_compaction_group_levels(group_id);
    group_levels
        .l0
        .as_ref()
        .unwrap()
        .sub_levels
        .iter()
        .rev()
        .chain(group_levels.levels.iter())
        .flat_map(|level| {
            level
                .table_infos
                .iter()
                .map(|table_info| (table_info.get_object_id(), table_info.get_sst_id()))
        })
}

pub fn new_sub_level(
    sub_level_id: u64,
    level_type: LevelType,
    table_infos: Vec<SstableInfo>,
) -> Level {
    if level_type == LevelType::Nonoverlapping {
        debug_assert!(
            can_concat(&table_infos),
            "sst of non-overlapping level is not concat-able: {:?}",
            table_infos
        );
    }
    let total_file_size = table_infos.iter().map(|table| table.file_size).sum();
    let uncompressed_file_size = table_infos
        .iter()
        .map(|table| table.uncompressed_file_size)
        .sum();
    Level {
        level_idx: 0,
        level_type: level_type as i32,
        table_infos,
        total_file_size,
        sub_level_id,
        uncompressed_file_size,
    }
}

pub fn add_ssts_to_sub_level(
    l0: &mut OverlappingLevel,
    sub_level_idx: usize,
    insert_table_infos: Vec<SstableInfo>,
) {
    insert_table_infos.iter().for_each(|sst| {
        l0.sub_levels[sub_level_idx].total_file_size += sst.file_size;
        l0.sub_levels[sub_level_idx].uncompressed_file_size += sst.uncompressed_file_size;
        l0.total_file_size += sst.file_size;
        l0.uncompressed_file_size += sst.uncompressed_file_size;
    });
    l0.sub_levels[sub_level_idx]
        .table_infos
        .extend(insert_table_infos);
    if l0.sub_levels[sub_level_idx].level_type == LevelType::Nonoverlapping as i32 {
        l0.sub_levels[sub_level_idx]
            .table_infos
            .sort_by(|sst1, sst2| {
                let a = sst1.key_range.as_ref().unwrap();
                let b = sst2.key_range.as_ref().unwrap();
                a.compare(b)
            });
        assert!(
            can_concat(&l0.sub_levels[sub_level_idx].table_infos),
            "sstable ids: {:?}",
            l0.sub_levels[sub_level_idx]
                .table_infos
                .iter()
                .map(|sst| sst.sst_id)
                .collect_vec()
        );
    }
}

/// `None` value of `sub_level_insert_hint` means append.
pub fn insert_new_sub_level(
    l0: &mut OverlappingLevel,
    insert_sub_level_id: u64,
    level_type: LevelType,
    insert_table_infos: Vec<SstableInfo>,
    sub_level_insert_hint: Option<usize>,
) {
    if insert_sub_level_id == u64::MAX {
        return;
    }
    let insert_pos = if let Some(insert_pos) = sub_level_insert_hint {
        insert_pos
    } else {
        if let Some(newest_level) = l0.sub_levels.last() {
            assert!(
                newest_level.sub_level_id < insert_sub_level_id,
                "inserted new level is not the newest: prev newest: {}, insert: {}. L0: {:?}",
                newest_level.sub_level_id,
                insert_sub_level_id,
                l0,
            );
        }
        l0.sub_levels.len()
    };
    #[cfg(debug_assertions)]
    {
        if insert_pos > 0 {
            if let Some(smaller_level) = l0.sub_levels.get(insert_pos - 1) {
                debug_assert!(smaller_level.get_sub_level_id() < insert_sub_level_id);
            }
        }
        if let Some(larger_level) = l0.sub_levels.get(insert_pos) {
            debug_assert!(larger_level.get_sub_level_id() > insert_sub_level_id);
        }
    }
    // All files will be committed in one new Overlapping sub-level and become
    // Nonoverlapping  after at least one compaction.
    let level = new_sub_level(insert_sub_level_id, level_type, insert_table_infos);
    l0.total_file_size += level.total_file_size;
    l0.uncompressed_file_size += level.uncompressed_file_size;
    l0.sub_levels.insert(insert_pos, level);
}

pub fn build_version_delta_after_version(version: &HummockVersion) -> HummockVersionDelta {
    HummockVersionDelta {
        id: version.id + 1,
        prev_id: version.id,
        safe_epoch: version.safe_epoch,
        trivial_move: false,
        max_committed_epoch: version.max_committed_epoch,
        group_deltas: Default::default(),
        gc_object_ids: vec![],
        new_table_watermarks: HashMap::new(),
    }
}

/// Delete sstables if the table id is in the id set.
///
/// Return `true` if some sst is deleted, and `false` is the deletion is trivial
fn level_delete_ssts(
    operand: &mut Level,
    delete_sst_ids_superset: &HashSet<HummockSstableId>,
) -> bool {
    let original_len = operand.table_infos.len();
    operand
        .table_infos
        .retain(|table| !delete_sst_ids_superset.contains(&table.sst_id));
    operand.total_file_size = operand
        .table_infos
        .iter()
        .map(|table| table.file_size)
        .sum::<u64>();
    operand.uncompressed_file_size = operand
        .table_infos
        .iter()
        .map(|table| table.uncompressed_file_size)
        .sum::<u64>();
    original_len != operand.table_infos.len()
}

fn level_insert_ssts(operand: &mut Level, insert_table_infos: Vec<SstableInfo>) {
    operand.total_file_size += insert_table_infos
        .iter()
        .map(|sst| sst.file_size)
        .sum::<u64>();
    operand.uncompressed_file_size += insert_table_infos
        .iter()
        .map(|sst| sst.uncompressed_file_size)
        .sum::<u64>();
    operand.table_infos.extend(insert_table_infos);
    operand.table_infos.sort_by(|sst1, sst2| {
        let a = sst1.key_range.as_ref().unwrap();
        let b = sst2.key_range.as_ref().unwrap();
        a.compare(b)
    });
    if operand.level_type == LevelType::Overlapping as i32 {
        operand.level_type = LevelType::Nonoverlapping as i32;
    }
    assert!(
        can_concat(&operand.table_infos),
        "sstable ids: {:?}",
        operand
            .table_infos
            .iter()
            .map(|sst| sst.sst_id)
            .collect_vec()
    );
}

pub fn object_size_map(version: &HummockVersion) -> HashMap<HummockSstableObjectId, u64> {
    version
        .levels
        .values()
        .flat_map(|cg| {
            cg.get_level0()
                .get_sub_levels()
                .iter()
                .chain(cg.get_levels().iter())
                .flat_map(|level| {
                    level
                        .get_table_infos()
                        .iter()
                        .map(|t| (t.object_id, t.file_size))
                })
        })
        .collect()
}

/// Verify the validity of a `HummockVersion` and return a list of violations if any.
/// Currently this method is only used by risectl validate-version.
pub fn validate_version(version: &HummockVersion) -> Vec<String> {
    let mut res = Vec::new();

    // Ensure safe_epoch <= max_committed_epoch
    if version.safe_epoch > version.max_committed_epoch {
        res.push(format!(
            "VERSION: safe_epoch {} > max_committed_epoch {}",
            version.safe_epoch, version.max_committed_epoch
        ));
    }

    let mut table_to_group = HashMap::new();
    // Ensure each table maps to only one compaction group
    for (group_id, levels) in &version.levels {
        // Ensure compaction group id matches
        if levels.group_id != *group_id {
            res.push(format!(
                "GROUP {}: inconsistent group id {} in Levels",
                group_id, levels.group_id
            ));
        }

        // Ensure table id is sorted
        if !levels.member_table_ids.is_sorted() {
            res.push(format!(
                "GROUP {}: memtable_table_ids is not sorted: {:?}",
                group_id, levels.member_table_ids
            ));
        }

        // Ensure table id is unique
        for table_id in &levels.member_table_ids {
            match table_to_group.entry(table_id) {
                Entry::Occupied(e) => {
                    res.push(format!(
                        "GROUP {}: Duplicated table_id {}. First found in group {}",
                        group_id,
                        table_id,
                        e.get()
                    ));
                }
                Entry::Vacant(e) => {
                    e.insert(group_id);
                }
            }
        }

        let validate_level = |group: CompactionGroupId,
                              expected_level_idx: u32,
                              level: &Level,
                              res: &mut Vec<String>| {
            let mut level_identifier = format!("GROUP {} LEVEL {}", group, level.level_idx);
            if level.level_idx == 0 {
                level_identifier.push_str(format!("SUBLEVEL {}", level.sub_level_id).as_str());
                // Ensure sub-level is not empty
                if level.table_infos.is_empty() {
                    res.push(format!("{}: empty level", level_identifier));
                }
            } else if level.level_type() != PbLevelType::Nonoverlapping {
                // Ensure non-L0 level is non-overlapping level
                res.push(format!(
                    "{}: level type {:?} is not non-overlapping",
                    level_identifier,
                    level.level_type()
                ));
            }

            // Ensure level idx matches
            if level.level_idx != expected_level_idx {
                res.push(format!(
                    "{}: mismatched level idx {}",
                    level_identifier, expected_level_idx
                ));
            }

            let mut prev_table_info: Option<&SstableInfo> = None;
            for table_info in &level.table_infos {
                // Ensure table_ids are sorted and unique
                if !table_info.table_ids.is_sorted_by(|a, b| {
                    if a < b {
                        Some(Ordering::Less)
                    } else {
                        Some(Ordering::Greater)
                    }
                }) {
                    res.push(format!(
                        "{} SST {}: table_ids not sorted",
                        level_identifier, table_info.object_id
                    ));
                }

                // Ensure SSTs in non-overlapping level have non-overlapping key range
                if level.level_type() == PbLevelType::Nonoverlapping {
                    if let Some(prev) = prev_table_info.take() {
                        if prev
                            .key_range
                            .as_ref()
                            .unwrap()
                            .compare_right_with(&table_info.key_range.as_ref().unwrap().left)
                            != Ordering::Less
                        {
                            res.push(format!(
                                "{} SST {}: key range should not overlap. prev={:?}, cur={:?}",
                                level_identifier, table_info.object_id, prev, table_info
                            ));
                        }
                    }
                    let _ = prev_table_info.insert(table_info);
                }
            }
        };

        if let Some(l0) = &levels.l0 {
            let mut prev_sub_level_id = u64::MAX;
            for sub_level in &l0.sub_levels {
                // Ensure sub_level_id is sorted and unique
                if sub_level.sub_level_id >= prev_sub_level_id {
                    res.push(format!(
                        "GROUP {} LEVEL 0: sub_level_id {} >= prev_sub_level {}",
                        group_id, sub_level.level_idx, prev_sub_level_id
                    ));
                }
                prev_sub_level_id = sub_level.sub_level_id;

                validate_level(*group_id, 0, sub_level, &mut res);
            }
        } else {
            res.push(format!("GROUP {}: level0 not exist", group_id));
        }

        for idx in 1..=levels.levels.len() {
            validate_level(*group_id, idx as u32, levels.get_level(idx), &mut res);
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use risingwave_pb::hummock::group_delta::DeltaType;
    use risingwave_pb::hummock::hummock_version::Levels;
    use risingwave_pb::hummock::hummock_version_delta::GroupDeltas;
    use risingwave_pb::hummock::{
        CompactionConfig, GroupConstruct, GroupDelta, GroupDestroy, HummockVersion,
        HummockVersionDelta, IntraLevelDelta, Level, LevelType, OverlappingLevel, SstableInfo,
    };

    use crate::compaction_group::hummock_version_ext::{
        build_initial_compaction_group_levels, HummockVersionExt, HummockVersionUpdateExt,
    };

    #[test]
    fn test_get_sst_object_ids() {
        let mut version = HummockVersion {
            id: 0,
            levels: HashMap::from_iter([(
                0,
                Levels {
                    levels: vec![],
                    l0: Some(OverlappingLevel {
                        sub_levels: vec![],
                        total_file_size: 0,
                        uncompressed_file_size: 0,
                    }),
                    ..Default::default()
                },
            )]),
            max_committed_epoch: 0,
            safe_epoch: 0,
            table_watermarks: HashMap::new(),
        };
        assert_eq!(version.get_object_ids().len(), 0);

        // Add to sub level
        version
            .levels
            .get_mut(&0)
            .unwrap()
            .l0
            .as_mut()
            .unwrap()
            .sub_levels
            .push(Level {
                table_infos: vec![SstableInfo {
                    object_id: 11,
                    sst_id: 11,
                    ..Default::default()
                }],
                ..Default::default()
            });
        assert_eq!(version.get_object_ids().len(), 1);

        // Add to non sub level
        version.levels.get_mut(&0).unwrap().levels.push(Level {
            table_infos: vec![SstableInfo {
                object_id: 22,
                sst_id: 22,
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(version.get_object_ids().len(), 2);
    }

    #[test]
    fn test_apply_version_delta() {
        let mut version = HummockVersion {
            id: 0,
            levels: HashMap::from_iter([
                (
                    0,
                    build_initial_compaction_group_levels(
                        0,
                        &CompactionConfig {
                            max_level: 6,
                            ..Default::default()
                        },
                    ),
                ),
                (
                    1,
                    build_initial_compaction_group_levels(
                        1,
                        &CompactionConfig {
                            max_level: 6,
                            ..Default::default()
                        },
                    ),
                ),
            ]),
            max_committed_epoch: 0,
            safe_epoch: 0,
            table_watermarks: HashMap::new(),
        };
        let version_delta = HummockVersionDelta {
            id: 1,
            group_deltas: HashMap::from_iter([
                (
                    2,
                    GroupDeltas {
                        group_deltas: vec![GroupDelta {
                            delta_type: Some(DeltaType::GroupConstruct(GroupConstruct {
                                group_config: Some(CompactionConfig {
                                    max_level: 6,
                                    ..Default::default()
                                }),
                                ..Default::default()
                            })),
                        }],
                    },
                ),
                (
                    0,
                    GroupDeltas {
                        group_deltas: vec![GroupDelta {
                            delta_type: Some(DeltaType::GroupDestroy(GroupDestroy {})),
                        }],
                    },
                ),
                (
                    1,
                    GroupDeltas {
                        group_deltas: vec![GroupDelta {
                            delta_type: Some(DeltaType::IntraLevel(IntraLevelDelta {
                                level_idx: 1,
                                inserted_table_infos: vec![SstableInfo {
                                    object_id: 1,
                                    sst_id: 1,
                                    ..Default::default()
                                }],
                                ..Default::default()
                            })),
                        }],
                    },
                ),
            ]),
            ..Default::default()
        };
        version.apply_version_delta(&version_delta);
        let mut cg1 = build_initial_compaction_group_levels(
            1,
            &CompactionConfig {
                max_level: 6,
                ..Default::default()
            },
        );
        cg1.levels[0] = Level {
            level_idx: 1,
            level_type: LevelType::Nonoverlapping as i32,
            table_infos: vec![SstableInfo {
                object_id: 1,
                sst_id: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            version,
            HummockVersion {
                id: 1,
                levels: HashMap::from_iter([
                    (
                        2,
                        build_initial_compaction_group_levels(
                            2,
                            &CompactionConfig {
                                max_level: 6,
                                ..Default::default()
                            }
                        ),
                    ),
                    (1, cg1,),
                ]),
                max_committed_epoch: 0,
                safe_epoch: 0,
                table_watermarks: HashMap::new(),
            }
        );
    }
}
