use alloc::collections::vec_deque::VecDeque;
use alloc::sync::Arc;
use core::ops::Deref;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::cmd::Command;
use crate::device::{Doorbell, DoorbellHelper, Namespace};
use crate::error::{Error, Result};
use crate::memory::{Allocator, PrpManager, PrpResult};
use crate::queues::{CompQueue, SubQueue};

/// A unique identifier for an I/O queue.
///
/// It self-increments starting from 1 and add each time
/// a new queue is created. The 0 is reserved for the admin queue pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IoQueueId(u16);

impl Deref for IoQueueId {
    type Target = u16;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[allow(clippy::new_without_default)]
impl IoQueueId {
    pub fn new() -> Self {
        static NEXT_ID: AtomicU16 = AtomicU16::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// A queue pair for handling NVMe I/O operations.
///
/// All your I/O operations should be done through this queue pair.
pub struct IoQueuePair<A: Allocator> {
    id: IoQueueId,
    allocator: Arc<A>,
    namespace: Namespace,
    doorbell_helper: DoorbellHelper,
    sub_queue: SubQueue,
    comp_queue: CompQueue,
    prp_manager: PrpManager,
    max_transfer_size: usize,
    submitted: VecDeque<PrpResult>,
}

impl<A: Allocator> IoQueuePair<A> {
    pub(crate) fn new(
        id: IoQueueId,
        namespace: Namespace,
        doorbell_helper: DoorbellHelper,
        sub_queue: SubQueue,
        comp_queue: CompQueue,
        allocator: Arc<A>,
        max_transfer_size: usize,
    ) -> Self {
        Self {
            id,
            namespace,
            doorbell_helper,
            sub_queue,
            comp_queue,
            prp_manager: Default::default(),
            allocator,
            max_transfer_size,
            submitted: Default::default(),
        }
    }
}

impl<A: Allocator> IoQueuePair<A> {
    fn submit_and_track(
        &mut self,
        bytes: usize,
        lba: u64,
        address: usize,
        write: bool,
    ) -> Result<()> {
        if bytes > self.max_transfer_size {
            return Err(Error::IoSizeExceedsMdts);
        }
        if bytes as u64 % self.namespace.block_size() != 0 {
            return Err(Error::InvalidBufferSize);
        }

        let prp_result = self
            .prp_manager
            .create(self.allocator.as_ref(), address, bytes)?;

        let prp = prp_result.get_prp();
        let blocks = bytes as u64 / self.namespace.block_size();

        let command = Command::read_write(
            self.sub_queue.tail as u16,
            self.namespace.id(),
            lba,
            blocks as u16 - 1,
            [prp.0 as u64, prp.1 as u64],
            write,
        );

        match self.sub_queue.try_push(command) {
            Ok(new_tail) => {
                self.doorbell_helper
                    .write(Doorbell::SubTail(*self.id), new_tail as u32);
                self.submitted.push_back(prp_result);
                Ok(())
            }
            Err(err) => {
                self.prp_manager
                    .release(prp_result, self.allocator.as_ref());
                Err(err)
            }
        }
    }
}

impl<A: Allocator> IoQueuePair<A> {
    /// Waits for all in-flight I/O operations to complete.
    ///
    /// This function will block until every command submitted via
    /// `read` or `write` has been completed by the device. It also handles
    /// resource cleanup for the completed requests.
    pub fn flush(&mut self) -> Result<()> {
        let num_to_complete = self.submitted.len();

        if num_to_complete == 0 {
            return Ok(());
        }

        let (tail, entry) = self.comp_queue.pop_n(num_to_complete);
        let doorbell = Doorbell::CompHead(*self.id);
        self.doorbell_helper.write(doorbell, tail as u32);

        while let Some(prp_result) = self.submitted.pop_front() {
            self.prp_manager
                .release(prp_result, self.allocator.as_ref());
        }

        let status = (entry.status >> 1) & 0xff;
        if status != 0 {
            return Err(Error::CommandFailed(status));
        }
        self.sub_queue.head = entry.sq_head as usize;

        Ok(())
    }
}

impl<A: Allocator> IoQueuePair<A> {
    /// Returns the queue pair ID.
    ///
    /// This ID is globally unique as it is a static counter.
    pub fn id(&self) -> IoQueueId {
        self.id
    }

    /// Submits a read request to the queue without blocking.
    ///
    /// This function adds a read command to the submission queue and returns immediately.
    /// The actual I/O operation happens in the background.
    /// Call `flush()` to wait for all submitted requests to complete.
    ///
    /// Returns an error if the submission queue is full.
    pub fn read(&mut self, dest: *mut u8, bytes: usize, lba: u64) -> Result<()> {
        self.submit_and_track(bytes, lba, dest as usize, false)
    }

    /// Submits a write request to the queue without blocking.
    ///
    /// See `read` for more details.
    pub fn write(&mut self, src: *const u8, bytes: usize, lba: u64) -> Result<()> {
        self.submit_and_track(bytes, lba, src as usize, true)
    }
}
