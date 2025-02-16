// Copyright 2022 The Blaze Authors
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

use std::{io::Write, mem::size_of};

use arrow::record_batch::RecordBatch;
use blaze_jni_bridge::jni_call;
use bytesize::ByteSize;
use count_write::CountWrite;
use datafusion::{common::Result, physical_plan::Partitioning};
use datafusion_ext_commons::{
    array_size::ArraySize,
    compute_suggested_batch_size_for_output,
    ds::rdx_tournament_tree::{KeyForRadixTournamentTree, RadixTournamentTree},
    rdxsort::radix_sort_u16_ranged_by,
    staging_mem_size_for_partial_sort,
};
use jni::objects::GlobalRef;

use crate::{
    common::{batch_selection::interleave_batches, ipc_compression::IpcCompressionWriter},
    shuffle::{evaluate_hashes, evaluate_partition_ids, rss::RssWriter},
};

#[derive(Default)]
pub struct BufferedData {
    staging_batches: Vec<RecordBatch>,
    sorted_batches: Vec<RecordBatch>,
    sorted_partition_indices: Vec<Vec<u32>>,
    num_rows: usize,
    staging_mem_used: usize,
    sorted_mem_used: usize,
}

impl BufferedData {
    pub fn add_batch(&mut self, batch: RecordBatch, partitioning: &Partitioning) -> Result<()> {
        self.num_rows += batch.num_rows();
        self.staging_mem_used += batch.get_array_mem_size();
        self.staging_batches.push(batch);
        if self.staging_mem_used >= staging_mem_size_for_partial_sort() {
            self.flush_staging_batches(partitioning)?;
        }
        Ok(())
    }

    fn flush_staging_batches(&mut self, partitioning: &Partitioning) -> Result<()> {
        log::info!(
            "shuffle buffered data starts partial sort, staging: {}, total: {}, total rows: {}",
            ByteSize(self.staging_mem_used as u64),
            ByteSize(self.mem_used() as u64),
            self.num_rows,
        );
        let staging_batches = std::mem::take(&mut self.staging_batches);
        self.staging_mem_used = 0;

        let (partition_indices, sorted_batch) =
            sort_batches_by_partition_id(staging_batches, partitioning)?;

        self.sorted_mem_used +=
            sorted_batch.get_array_mem_size() + partition_indices.len() * size_of::<u32>();
        self.sorted_batches.push(sorted_batch);
        self.sorted_partition_indices.push(partition_indices);
        Ok(())
    }

    // write buffered data to spill/target file, returns uncompressed size and
    // offsets to each partition
    pub fn write<W: Write>(
        self,
        mut w: W,
        partitioning: &Partitioning,
    ) -> Result<(usize, Vec<u64>)> {
        if self.num_rows == 0 {
            return Ok((0, vec![0; partitioning.partition_count() + 1]));
        }
        let mut offsets = vec![];
        let mut offset = 0;
        let mut iter = self.into_sorted_batches(partitioning)?.peekable();
        let mut uncompressed_size = 0;

        while let Some(&part_id) = iter.peek().map(|(part_id, _)| part_id) {
            while offsets.len() <= part_id as usize {
                offsets.push(offset); // fill offsets of empty partitions
            }

            // write all batches with this part id
            let mut writer = IpcCompressionWriter::new(CountWrite::from(&mut w), true);
            while matches!(iter.peek(), Some((id, _)) if *id == part_id) {
                uncompressed_size += writer.write_batch(iter.next().unwrap().1)?;
            }
            offset += writer.finish_into_inner()?.count();
            offsets.push(offset);
        }
        while offsets.len() <= partitioning.partition_count() {
            offsets.push(offset); // fill offsets of empty partitions
        }
        Ok((uncompressed_size, offsets))
    }

    // write buffered data to rss, returns uncompressed size
    pub fn write_rss(
        self,
        rss_partition_writer: GlobalRef,
        partitioning: &Partitioning,
    ) -> Result<usize> {
        if self.num_rows == 0 {
            return Ok(0);
        }
        let mut iter = self.into_sorted_batches(partitioning)?.peekable();
        let mut uncompressed_size = 0;

        while let Some(&part_id) = iter.peek().map(|(part_id, _)| part_id) {
            let mut writer = IpcCompressionWriter::new(
                RssWriter::new(rss_partition_writer.clone(), part_id as usize),
                true,
            );

            // write all batches with this part id
            while matches!(iter.peek(), Some((id, _)) if *id == part_id) {
                uncompressed_size += writer.write_batch(iter.next().unwrap().1)?;
            }
            writer.finish_into_inner()?;
        }
        jni_call!(BlazeRssPartitionWriterBase(rss_partition_writer.as_obj()).flush() -> ())?;
        Ok(uncompressed_size)
    }

    fn into_sorted_batches(
        mut self,
        partitioning: &Partitioning,
    ) -> Result<impl Iterator<Item = (u32, RecordBatch)>> {
        if !self.staging_batches.is_empty() {
            self.flush_staging_batches(partitioning)?;
        }

        struct Cursor {
            idx: usize,
            partition_indices: Vec<u32>,
            row_idx: usize,
            part_id: u32,
        }

        impl KeyForRadixTournamentTree for Cursor {
            fn rdx(&self) -> usize {
                self.part_id as usize
            }
        }

        struct PartitionedBatchesIterator {
            batches: Vec<RecordBatch>,
            cursors: RadixTournamentTree<Cursor>,
            num_output_rows: usize,
            num_rows: usize,
            batch_size: usize,
        }

        impl Iterator for PartitionedBatchesIterator {
            type Item = (u32, RecordBatch);

            fn next(&mut self) -> Option<Self::Item> {
                if self.num_output_rows >= self.num_rows {
                    return None;
                }
                let cur_batch_size = self.batch_size.min(self.num_rows - self.num_output_rows);
                let cur_part_id = self.cursors.peek().part_id;
                let mut indices = Vec::with_capacity(cur_batch_size);

                // add rows with same parition id under this cursor
                while indices.len() < cur_batch_size {
                    let mut min_cursor = self.cursors.peek_mut();
                    if min_cursor.part_id != cur_part_id {
                        break;
                    }
                    while indices.len() < cur_batch_size && min_cursor.part_id == cur_part_id {
                        indices.push((min_cursor.idx, min_cursor.row_idx));
                        min_cursor.row_idx += 1;
                        min_cursor.part_id = *min_cursor
                            .partition_indices
                            .get(min_cursor.row_idx)
                            .unwrap_or(&u32::MAX);
                    }
                }
                let output_batch =
                    interleave_batches(self.batches[0].schema(), &self.batches, &indices)
                        .expect("error merging sorted batches: interleaving error");
                self.num_output_rows += output_batch.num_rows();
                Some((cur_part_id, output_batch))
            }
        }

        let sub_batch_size =
            compute_suggested_batch_size_for_output(self.mem_used(), self.num_rows);

        Ok(PartitionedBatchesIterator {
            batches: self.sorted_batches.clone(),
            cursors: RadixTournamentTree::new(
                self.sorted_partition_indices
                    .into_iter()
                    .enumerate()
                    .map(|(idx, partition_indices)| Cursor {
                        idx,
                        part_id: partition_indices[0],
                        row_idx: 0,
                        partition_indices,
                    })
                    .collect(),
                partitioning.partition_count(),
            ),
            num_output_rows: 0,
            num_rows: self.num_rows,
            batch_size: sub_batch_size,
        })
    }

    pub fn mem_used(&self) -> usize {
        self.staging_mem_used + self.sorted_mem_used
    }
}

fn sort_batches_by_partition_id(
    batches: Vec<RecordBatch>,
    partitioning: &Partitioning,
) -> Result<(Vec<u32>, RecordBatch)> {
    let num_rows = batches.iter().map(|batch| batch.num_rows()).sum::<usize>();
    let num_partitions = partitioning.partition_count();
    let schema = batches[0].schema();

    let mut indices = batches // partition_id, batch_idx, row_idx
        .iter()
        .enumerate()
        .flat_map(|(batch_idx, batch)| {
            let hashes = evaluate_hashes(partitioning, batch)
                .expect(&format!("error evaluating hashes with {partitioning}"));
            evaluate_partition_ids(&hashes, partitioning.partition_count())
                .into_iter()
                .enumerate()
                .map(move |(row_idx, part_id)| (part_id, batch_idx as u32, row_idx as u32))
        })
        .collect::<Vec<_>>();

    // use quick sort if there are too many partitions or too few rows, otherwise
    // use radix sort
    if num_partitions < 65536 && num_rows >= num_partitions {
        radix_sort_u16_ranged_by(&mut indices, num_partitions, |v| v.0 as u16);
    } else {
        indices.sort_unstable_by_key(|v| v.0);
    }

    // get sorted batches
    let (sorted_partition_indices, sorted_row_indices): (Vec<u32>, Vec<_>) = indices
        .into_iter()
        .map(|(part_id, batch_idx, row_idx)| (part_id, (batch_idx as usize, row_idx as usize)))
        .unzip();
    let sorted_batch = interleave_batches(schema, &batches, &sorted_row_indices)?;
    return Ok((sorted_partition_indices, sorted_batch));
}
