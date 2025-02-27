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

use std::future::Future;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use more_asserts::assert_gt;

use super::{HummockResult, HummockValue};

mod forward_concat;
pub use forward_concat::*;
mod backward_concat;
mod concat_inner;
pub use backward_concat::*;
pub use concat_inner::ConcatIteratorInner;
mod backward_merge;

mod backward_user;
pub use backward_user::*;
mod forward_merge;

pub mod forward_user;
mod merge_inner;
pub use forward_user::*;
pub use merge_inner::{OrderedMergeIteratorInner, UnorderedMergeIteratorInner};
use risingwave_hummock_sdk::key::{FullKey, TableKey, UserKey};

use crate::hummock::iterator::HummockIteratorUnion::{First, Fourth, Second, Third};

mod concat_delete_range_iterator;
mod delete_range_iterator;
mod skip_watermark;
#[cfg(any(test, feature = "test"))]
pub mod test_utils;
pub use delete_range_iterator::{
    DeleteRangeIterator, ForwardMergeRangeIterator, RangeIteratorTyped,
};
use risingwave_common::catalog::TableId;
use risingwave_hummock_sdk::EpochWithGap;
pub use skip_watermark::*;

use crate::monitor::StoreLocalStatistic;

/// `HummockIterator` defines the interface of all iterators, including `SstableIterator`,
/// `MergeIterator`, `UserIterator` and `ConcatIterator`.
///
/// After creating the iterator instance,
/// - if you want to iterate from the beginning, you need to then call its `rewind` method.
/// - if you want to iterate from some specific position, you need to then call its `seek` method.
pub trait HummockIterator: Send + Sync {
    type Direction: HummockIteratorDirection;
    /// Moves a valid iterator to the next key.
    ///
    /// Note:
    /// - Before calling this function, makes sure the iterator `is_valid`.
    /// - After calling this function, you may first check whether the iterator `is_valid` again,
    ///   then get the new data by calling `key` and `value`.
    /// - If the position after calling this is invalid, this function WON'T return an `Err`. You
    ///   should check `is_valid` before continuing the iteration.
    ///
    /// # Panics
    /// This function will panic if the iterator is invalid.
    fn next(&mut self) -> impl Future<Output = HummockResult<()>> + Send + '_;

    /// Retrieves the current key.
    ///
    /// Note:
    /// - Before calling this function, makes sure the iterator `is_valid`.
    /// - This function should be straightforward and return immediately.
    ///
    /// # Panics
    /// This function will panic if the iterator is invalid.
    fn key(&self) -> FullKey<&[u8]>;

    /// Retrieves the current value, decoded as [`HummockValue`].
    ///
    /// Note:
    /// - Before calling this function, makes sure the iterator `is_valid`.
    /// - This function should be straightforward and return immediately.
    ///
    /// # Panics
    /// This function will panic if the iterator is invalid, or the value cannot be decoded into
    /// [`HummockValue`].
    fn value(&self) -> HummockValue<&[u8]>;

    /// Indicates whether the iterator can be used.
    ///
    /// Note:
    /// - ONLY call `key`, `value`, and `next` if `is_valid` returns `true`.
    /// - This function should be straightforward and return immediately.
    fn is_valid(&self) -> bool;

    /// Resets the position of the iterator.
    ///
    /// Note:
    /// - Do not decide whether the position is valid or not by checking the returned error of this
    ///   function. This function WON'T return an `Err` if invalid. You should check `is_valid`
    ///   before starting iteration.
    fn rewind(&mut self) -> impl Future<Output = HummockResult<()>> + Send + '_;

    /// Resets iterator and seeks to the first position where the key >= provided key, or key <=
    /// provided key if this is a backward iterator.
    ///
    /// Note:
    /// - Do not decide whether the position is valid or not by checking the returned error of this
    ///   function. This function WON'T return an `Err` if invalid. You should check `is_valid`
    ///   before starting iteration.
    fn seek<'a>(
        &'a mut self,
        key: FullKey<&'a [u8]>,
    ) -> impl Future<Output = HummockResult<()>> + Send + '_;

    /// take local statistic info from iterator to report metrics.
    fn collect_local_statistic(&self, _stats: &mut StoreLocalStatistic);
}

/// This is a placeholder trait used in `HummockIteratorUnion`
pub struct PhantomHummockIterator<D: HummockIteratorDirection> {
    _phantom: PhantomData<D>,
}

impl<D: HummockIteratorDirection> HummockIterator for PhantomHummockIterator<D> {
    type Direction = D;

    async fn next(&mut self) -> HummockResult<()> {
        unreachable!()
    }

    fn key(&self) -> FullKey<&[u8]> {
        unreachable!()
    }

    fn value(&self) -> HummockValue<&[u8]> {
        unreachable!()
    }

    fn is_valid(&self) -> bool {
        unreachable!()
    }

    async fn rewind(&mut self) -> HummockResult<()> {
        unreachable!()
    }

    async fn seek<'a>(&'a mut self, _key: FullKey<&'a [u8]>) -> HummockResult<()> {
        unreachable!()
    }

    fn collect_local_statistic(&self, _stats: &mut StoreLocalStatistic) {}
}

/// The `HummockIteratorUnion` acts like a wrapper over multiple types of `HummockIterator`, so that
/// the `MergeIterator`, which previously takes multiple different `HummockIterator`s as input
/// through `Box<dyn HummockIterator>`, can now wrap all its underlying `HummockIterator` over such
/// `HummockIteratorUnion`, and the input type of the `MergeIterator` so that the input type of
/// `HummockIterator` can be determined statically at compile time.
///
/// For example, in `ForwardUserIterator`, it accepts inputs from 4 sources for its underlying
/// `MergeIterator`. First, the shared buffer replicated batches, and second, the shared buffer
/// uncommitted data, and third, the overlapping L0 data, and last, the non-L0 non-overlapping
/// concat-able. These sources used to be passed in as `Box<dyn HummockIterator>`. Now if we want
/// the `MergeIterator` to be statically typed, the input type of `MergeIterator` will become the
/// `HummockIteratorUnion` of these 4 sources.
pub enum HummockIteratorUnion<
    D: HummockIteratorDirection,
    I1: HummockIterator<Direction = D>,
    I2: HummockIterator<Direction = D>,
    I3: HummockIterator<Direction = D> = PhantomHummockIterator<D>,
    I4: HummockIterator<Direction = D> = PhantomHummockIterator<D>,
> {
    First(I1),
    Second(I2),
    Third(I3),
    Fourth(I4),
}

impl<
        D: HummockIteratorDirection,
        I1: HummockIterator<Direction = D>,
        I2: HummockIterator<Direction = D>,
        I3: HummockIterator<Direction = D>,
        I4: HummockIterator<Direction = D>,
    > HummockIterator for HummockIteratorUnion<D, I1, I2, I3, I4>
{
    type Direction = D;

    async fn next(&mut self) -> HummockResult<()> {
        match self {
            First(iter) => iter.next().await,
            Second(iter) => iter.next().await,
            Third(iter) => iter.next().await,
            Fourth(iter) => iter.next().await,
        }
    }

    fn key(&self) -> FullKey<&[u8]> {
        match self {
            First(iter) => iter.key(),
            Second(iter) => iter.key(),
            Third(iter) => iter.key(),
            Fourth(iter) => iter.key(),
        }
    }

    fn value(&self) -> HummockValue<&[u8]> {
        match self {
            First(iter) => iter.value(),
            Second(iter) => iter.value(),
            Third(iter) => iter.value(),
            Fourth(iter) => iter.value(),
        }
    }

    fn is_valid(&self) -> bool {
        match self {
            First(iter) => iter.is_valid(),
            Second(iter) => iter.is_valid(),
            Third(iter) => iter.is_valid(),
            Fourth(iter) => iter.is_valid(),
        }
    }

    async fn rewind(&mut self) -> HummockResult<()> {
        match self {
            First(iter) => iter.rewind().await,
            Second(iter) => iter.rewind().await,
            Third(iter) => iter.rewind().await,
            Fourth(iter) => iter.rewind().await,
        }
    }

    async fn seek<'a>(&'a mut self, key: FullKey<&'a [u8]>) -> HummockResult<()> {
        match self {
            First(iter) => iter.seek(key).await,
            Second(iter) => iter.seek(key).await,
            Third(iter) => iter.seek(key).await,
            Fourth(iter) => iter.seek(key).await,
        }
    }

    fn collect_local_statistic(&self, stats: &mut StoreLocalStatistic) {
        match self {
            First(iter) => iter.collect_local_statistic(stats),
            Second(iter) => iter.collect_local_statistic(stats),
            Third(iter) => iter.collect_local_statistic(stats),
            Fourth(iter) => iter.collect_local_statistic(stats),
        }
    }
}

impl<I: HummockIterator> HummockIterator for Box<I> {
    type Direction = I::Direction;

    async fn next(&mut self) -> HummockResult<()> {
        (*self).deref_mut().next().await
    }

    fn key(&self) -> FullKey<&[u8]> {
        (*self).deref().key()
    }

    fn value(&self) -> HummockValue<&[u8]> {
        (*self).deref().value()
    }

    fn is_valid(&self) -> bool {
        (*self).deref().is_valid()
    }

    async fn rewind(&mut self) -> HummockResult<()> {
        (*self).deref_mut().rewind().await
    }

    async fn seek<'a>(&'a mut self, key: FullKey<&'a [u8]>) -> HummockResult<()> {
        (*self).deref_mut().seek(key).await
    }

    fn collect_local_statistic(&self, stats: &mut StoreLocalStatistic) {
        (*self).deref().collect_local_statistic(stats);
    }
}

pub enum RustIteratorOfBuilder<'a, B: RustIteratorBuilder> {
    Seek(B::SeekIter<'a>),
    Rewind(B::RewindIter<'a>),
}

impl<'a, B: RustIteratorBuilder> Iterator for RustIteratorOfBuilder<'a, B> {
    type Item = (TableKey<&'a [u8]>, HummockValue<&'a [u8]>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            RustIteratorOfBuilder::Seek(i) => i.next(),
            RustIteratorOfBuilder::Rewind(i) => i.next(),
        }
    }
}

pub trait RustIteratorBuilder: Send + Sync + 'static {
    type Iterable: Send + Sync;
    type RewindIter<'a>: Iterator<Item = (TableKey<&'a [u8]>, HummockValue<&'a [u8]>)>
        + Send
        + Sync
        + 'a;
    type SeekIter<'a>: Iterator<Item = (TableKey<&'a [u8]>, HummockValue<&'a [u8]>)>
        + Send
        + Sync
        + 'a;

    fn seek<'a>(iterable: &'a Self::Iterable, seek_key: TableKey<&[u8]>) -> Self::SeekIter<'a>;
    fn rewind(iterable: &Self::Iterable) -> Self::RewindIter<'_>;
}

pub struct FromRustIterator<'a, B: RustIteratorBuilder> {
    inner: &'a B::Iterable,
    #[expect(clippy::type_complexity)]
    iter: Option<(
        RustIteratorOfBuilder<'a, B>,
        TableKey<&'a [u8]>,
        HummockValue<&'a [u8]>,
    )>,
    epoch: EpochWithGap,
    table_id: TableId,
}

impl<'a, B: RustIteratorBuilder> FromRustIterator<'a, B> {
    pub fn new(inner: &'a B::Iterable, epoch: EpochWithGap, table_id: TableId) -> Self {
        Self {
            inner,
            iter: None,
            epoch,
            table_id,
        }
    }
}

impl<'a, B: RustIteratorBuilder> HummockIterator for FromRustIterator<'a, B> {
    type Direction = Forward;

    async fn next(&mut self) -> HummockResult<()> {
        let (iter, key, value) = self.iter.as_mut().expect("should be valid");
        if let Some((new_key, new_value)) = iter.next() {
            *key = new_key;
            *value = new_value;
        } else {
            self.iter = None;
        }
        Ok(())
    }

    fn key(&self) -> FullKey<&[u8]> {
        let (_, key, _) = self.iter.as_ref().expect("should be valid");
        FullKey {
            epoch_with_gap: self.epoch,
            user_key: UserKey {
                table_id: self.table_id,
                table_key: *key,
            },
        }
    }

    fn value(&self) -> HummockValue<&[u8]> {
        let (_, _, value) = self.iter.as_ref().expect("should be valid");
        *value
    }

    fn is_valid(&self) -> bool {
        self.iter.is_some()
    }

    async fn rewind(&mut self) -> HummockResult<()> {
        let mut iter = B::rewind(self.inner);
        if let Some((key, value)) = iter.next() {
            self.iter = Some((RustIteratorOfBuilder::Rewind(iter), key, value));
        } else {
            self.iter = None;
        }
        Ok(())
    }

    async fn seek<'b>(&'b mut self, key: FullKey<&'b [u8]>) -> HummockResult<()> {
        if self.table_id < key.user_key.table_id {
            // returns None when the range of self.table_id must not include the given key
            self.iter = None;
            return Ok(());
        }
        if self.table_id > key.user_key.table_id {
            return self.rewind().await;
        }
        let mut iter = B::seek(self.inner, key.user_key.table_key);
        match iter.next() {
            Some((first_key, first_value)) => {
                let first_full_key = FullKey {
                    epoch_with_gap: self.epoch,
                    user_key: UserKey {
                        table_id: self.table_id,
                        table_key: first_key,
                    },
                };
                if first_full_key < key {
                    // The semantic of `seek_fn` will ensure that `first_key` >= table_key of `key`.
                    // At the beginning we have checked that `self.table_id` >= table_id of `key`.
                    // Therefore, when `first_full_key` < `key`, the only possibility is that
                    // `first_key` == table_key of `key`, and `self.table_id` == table_id of `key`,
                    // the `self.epoch` < epoch of `key`.
                    assert_eq!(first_key, key.user_key.table_key);
                    match iter.next() {
                        Some((next_key, next_value)) => {
                            assert_gt!(next_key, first_key);
                            self.iter =
                                Some((RustIteratorOfBuilder::Seek(iter), next_key, next_value));
                        }
                        None => {
                            self.iter = None;
                        }
                    }
                } else {
                    self.iter = Some((RustIteratorOfBuilder::Seek(iter), first_key, first_value));
                }
            }
            None => {
                self.iter = None;
            }
        }
        Ok(())
    }

    fn collect_local_statistic(&self, _stats: &mut StoreLocalStatistic) {}
}

#[derive(PartialEq, Eq, Debug)]
pub enum DirectionEnum {
    Forward,
    Backward,
}

pub trait HummockIteratorDirection: Sync + Send + 'static {
    fn direction() -> DirectionEnum;
}

pub struct Forward;
impl HummockIteratorDirection for Forward {
    #[inline(always)]
    fn direction() -> DirectionEnum {
        DirectionEnum::Forward
    }
}

pub struct Backward;
impl HummockIteratorDirection for Backward {
    #[inline(always)]
    fn direction() -> DirectionEnum {
        DirectionEnum::Backward
    }
}
