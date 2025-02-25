// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        errors::FxfsError,
        lsm_tree::types::ItemRef,
        object_handle::{ObjectHandle, ObjectProperties},
        object_store::{
            record::{
                ExtentKey, ExtentValue, ObjectKey, ObjectValue, Timestamp,
                DEFAULT_DATA_ATTRIBUTE_ID,
            },
            transaction::{self, Transaction},
        },
    },
    anyhow::{bail, Error},
    async_trait::async_trait,
    interval_tree::utils::RangeOps,
    std::{cmp::min, ops::Range, sync::Arc},
    storage_device::{
        buffer::{Buffer, BufferRef, MutableBufferRef},
        Device,
    },
};

// To read the super-block and journal, we use a special reader since we cannot use the object-store
// reader until we've replayed the whole journal.  Clients must supply the extents to be used.
pub struct Handle {
    object_id: u64,
    device: Arc<dyn Device>,
    start_offset: u64,
    extents: Vec<Range<u64>>,
    size: u64,
}

impl Handle {
    pub fn new(object_id: u64, device: Arc<dyn Device>) -> Self {
        Self { object_id, device, start_offset: 0, extents: Vec::new(), size: 0 }
    }

    pub fn push_extent(&mut self, r: Range<u64>) {
        self.extents.push(r);
    }

    pub fn try_push_extent_from_object_item(
        &mut self,
        item: ItemRef<'_, ObjectKey, ObjectValue>,
    ) -> Result<bool, Error> {
        match item.into() {
            Some((
                object_id,
                DEFAULT_DATA_ATTRIBUTE_ID,
                ExtentKey { range },
                ExtentValue { device_offset: Some((device_offset, _)) },
            )) if object_id == self.object_id => {
                if self.extents.is_empty() {
                    self.start_offset = range.start;
                } else if range.start != self.size {
                    bail!(FxfsError::Inconsistent);
                }
                self.extents.push(*device_offset..*device_offset + range.length());
                self.size = range.end;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

// TODO(csuter): This doesn't need to be ObjectHandle any more and we could integrate this into
// JournalReader.
#[async_trait]
impl ObjectHandle for Handle {
    fn object_id(&self) -> u64 {
        self.object_id
    }

    fn allocate_buffer(&self, size: usize) -> Buffer<'_> {
        self.device.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        self.device.block_size()
    }

    async fn read(&self, mut offset: u64, mut buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        assert!(offset >= self.start_offset);
        let len = buf.len();
        let mut buf_offset = 0;
        let mut file_offset = self.start_offset;
        for extent in &self.extents {
            let extent_len = extent.end - extent.start;
            if offset < file_offset + extent_len {
                let device_offset = extent.start + offset - file_offset;
                let to_read = min(extent.end - device_offset, (len - buf_offset) as u64) as usize;
                assert!(buf_offset % self.device.block_size() as usize == 0);
                self.device
                    .read(
                        device_offset,
                        buf.reborrow().subslice_mut(buf_offset..buf_offset + to_read),
                    )
                    .await?;
                buf_offset += to_read;
                if buf_offset == len {
                    break;
                }
                offset += to_read as u64;
            }
            file_offset += extent_len;
        }
        Ok(len)
    }

    async fn txn_write<'a>(
        &'a self,
        _transaction: &mut Transaction<'a>,
        _offset: u64,
        _buf: BufferRef<'_>,
    ) -> Result<(), Error> {
        unreachable!();
    }

    async fn overwrite(&self, _offset: u64, _buf: BufferRef<'_>) -> Result<(), Error> {
        unreachable!();
    }

    fn get_size(&self) -> u64 {
        self.size
    }

    async fn truncate<'a>(
        &'a self,
        _transaction: &mut Transaction<'a>,
        _length: u64,
    ) -> Result<(), Error> {
        unreachable!();
    }

    async fn preallocate_range<'a>(
        &'a self,
        _transaction: &mut Transaction<'a>,
        _range: Range<u64>,
    ) -> Result<Vec<Range<u64>>, Error> {
        unreachable!();
    }

    async fn update_timestamps<'a>(
        &'a self,
        _transaction: Option<&mut Transaction<'a>>,
        _ctime: Option<Timestamp>,
        _mtime: Option<Timestamp>,
    ) -> Result<(), Error> {
        unreachable!();
    }

    async fn get_properties(&self) -> Result<ObjectProperties, Error> {
        unreachable!();
    }

    async fn new_transaction_with_options<'a>(
        &self,
        _options: transaction::Options<'a>,
    ) -> Result<Transaction<'a>, Error> {
        unreachable!();
    }
}
