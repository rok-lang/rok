//! The mutator TLS block `rok_tls`: slot offsets the
//! compiler and the runtime agree on. Plain constants only: `rokc_mir`
//! includes this file by path.

/// A slot `words` pointers plus `bytes` bytes into the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub words: u32,
    pub bytes: u32,
}

impl Slot {
    /// Byte offset at pointer width `ptr` (bytes).
    #[must_use]
    pub const fn offset(self, ptr: u32) -> u32 {
        self.words * ptr + self.bytes
    }
}

const fn at(words: u32, bytes: u32) -> Slot {
    Slot { words, bytes }
}

pub const SHADOW_STACK_TOP: Slot = at(0, 0);
pub const TLAB_CURSOR: Slot = at(1, 0);
pub const TLAB_LIMIT: Slot = at(2, 0);
pub const WB_LOG_CURSOR: Slot = at(3, 0);
pub const WB_LOG_LIMIT: Slot = at(4, 0);
pub const SATB_CURSOR: Slot = at(5, 0);
pub const SATB_LIMIT: Slot = at(6, 0);
pub const TASK_WORKER: Slot = at(7, 0);
pub const STACK_GUARD: Slot = at(8, 0);
pub const WORKER_ID: Slot = at(10, 0);
pub const YIELD_FLAG: Slot = at(10, 8);
pub const MUTATOR_STATE: Slot = at(10, 9);
/// Implementation-defined slots (outside the ABI) follow the ABI part.
/// Depth of the `#[no_safepoint]` calls a debug rokc brackets (`u8`).
pub const NO_SAFEPOINT: Slot = at(10, 10);
/// How the thread joined the runtime (`u8`, `tls::ATTACHED_*`; 0: not).
pub const ATTACHED: Slot = at(10, 11);
pub const TASK_LANE: Slot = at(10, 16);
pub const STATIC_INITS: Slot = at(11, 16);
pub const HELD_LOCKS: Slot = at(12, 16);
/// The thread's name in reports, `{ptr, len}` (null: unnamed).
pub const THREAD_NAME: Slot = at(13, 16);
/// Size of the block; it is aligned to 8.
pub const SIZE: Slot = at(15, 16);
pub const ALIGN: u32 = 8;
