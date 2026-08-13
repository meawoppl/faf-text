//! Instance arenas: one growable GPU vertex buffer per instance type, carved
//! into ranges owned by retained blocks.
//!
//! Allocation is a bump pointer plus a free list of released spans. Freed
//! spans are never coalesced with their neighbours; instead, once the free
//! list holds more than [`REPACK_FRAGMENTATION`] of the arena, the renderer
//! repacks it — every live block copied back contiguously and the prefix
//! re-uploaded in one write. Repacking is O(live instances) and only ever runs
//! after a drop or a shrink, which makes coalescing (and its edge cases)
//! unnecessary.
//!
//! The CPU mirror is a `Vec<u32>` rather than `Vec<u8>`: every instance type is
//! plain `f32`/`u32` fields, so word storage keeps the mirror aligned for
//! `bytemuck` casts in both directions. A `Vec<u8>` is only byte-aligned and
//! `cast_slice` would be free to panic on it.

use bytemuck::Pod;

/// Repack an arena once this fraction of its instances sits in the free list.
pub const REPACK_FRAGMENTATION: f32 = 0.5;

/// Extra room, as a fraction, reserved beyond the requested length so that
/// small edits (a caret rect, a typed character) reuse the allocation instead
/// of churning the free list.
const SLACK_NUM: u32 = 1;
const SLACK_DEN: u32 = 4;

/// Capacity to reserve for `len` instances.
fn plan_cap(len: u32) -> u32 {
    (len + len * SLACK_NUM / SLACK_DEN).next_multiple_of(8)
}

/// One block's slice of an arena: `cap` instances reserved starting at
/// `start`, of which the first `len` hold live content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub cap: u32,
    pub len: u32,
}

impl Span {
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A growable instance buffer plus its CPU mirror and allocator.
pub struct Arena {
    label: &'static str,
    /// Words (4 bytes) per instance.
    stride: u32,
    /// CPU mirror of the whole arena, `top * stride` words long.
    words: Vec<u32>,
    buffer: Option<wgpu::Buffer>,
    /// Instances the GPU buffer can hold.
    capacity: u32,
    /// Bump pointer: instances at or above it were never handed out.
    top: u32,
    /// Released spans, `len` always 0.
    free: Vec<Span>,
    /// Instances sitting in `free`.
    freed: u32,
    /// The whole live prefix needs re-uploading (the buffer was recreated or
    /// the arena was repacked).
    full: bool,
}

impl Arena {
    pub fn new(label: &'static str, stride_bytes: usize) -> Self {
        assert!(
            stride_bytes.is_multiple_of(4),
            "instance stride must be a multiple of 4"
        );
        Self {
            label,
            stride: (stride_bytes / 4) as u32,
            words: Vec::new(),
            buffer: None,
            capacity: 0,
            top: 0,
            free: Vec::new(),
            freed: 0,
            full: false,
        }
    }

    pub fn buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref()
    }

    /// Bytes from the start of the buffer to a span's first instance.
    pub fn byte_offset(&self, span: Span) -> u64 {
        span.start as u64 * self.stride as u64 * 4
    }

    /// Instances handed out so far (live plus freed).
    #[cfg(test)]
    pub fn top(&self) -> u32 {
        self.top
    }

    /// Instances sitting in the free list.
    #[cfg(test)]
    pub fn freed(&self) -> u32 {
        self.freed
    }

    /// Fraction of the arena sitting in the free list.
    pub fn fragmentation(&self) -> f32 {
        if self.top == 0 {
            0.0
        } else {
            self.freed as f32 / self.top as f32
        }
    }

    /// Point `span` at room for `len` instances. An allocation that still fits
    /// is kept where it is — the common case when a block's content changes
    /// shape slightly.
    pub fn alloc(&mut self, span: &mut Span, len: u32) {
        if len == 0 {
            self.release(span);
            return;
        }
        if span.cap >= len {
            span.len = len;
            return;
        }
        self.release(span);
        *span = self.take(len);
    }

    /// Return a span's storage to the free list and empty it.
    pub fn release(&mut self, span: &mut Span) {
        if span.cap > 0 {
            self.freed += span.cap;
            self.free.push(Span {
                start: span.start,
                cap: span.cap,
                len: 0,
            });
        }
        *span = Span::default();
    }

    fn take(&mut self, len: u32) -> Span {
        let cap = plan_cap(len);
        if let Some(i) = self.free.iter().position(|hole| hole.cap >= cap) {
            let hole = self.free.swap_remove(i);
            self.freed -= hole.cap;
            if hole.cap > cap {
                self.free.push(Span {
                    start: hole.start + cap,
                    cap: hole.cap - cap,
                    len: 0,
                });
                self.freed += hole.cap - cap;
            }
            return Span {
                start: hole.start,
                cap,
                len,
            };
        }
        let start = self.top;
        self.top += cap;
        self.words.resize((self.top * self.stride) as usize, 0);
        Span { start, cap, len }
    }

    fn words_of(&self, span: Span) -> std::ops::Range<usize> {
        let start = (span.start * self.stride) as usize;
        start..start + (span.len * self.stride) as usize
    }

    /// Copy instances into a span's storage. `data` must match `span.len`.
    pub fn write<T: Pod>(&mut self, span: Span, data: &[T]) {
        debug_assert_eq!(data.len() as u32, span.len);
        if span.is_empty() {
            return;
        }
        let range = self.words_of(span);
        self.words[range].copy_from_slice(bytemuck::cast_slice(data));
    }

    /// A span's instances, for in-place patching (see the curve-relocation
    /// fixup in `renderer.rs`).
    pub fn slice_mut<T: Pod>(&mut self, span: Span) -> &mut [T] {
        let range = self.words_of(span);
        bytemuck::cast_slice_mut(&mut self.words[range])
    }

    /// Start a repack: a buffer for the live instances, in draw order.
    pub fn repack_buffer(&self) -> Vec<u32> {
        Vec::with_capacity(self.words.len())
    }

    /// Append one span's live instances to a repack buffer and return where
    /// they landed.
    pub fn repack_span(&self, packed: &mut Vec<u32>, span: Span) -> Span {
        if span.is_empty() {
            return Span::default();
        }
        let start = (packed.len() / self.stride as usize) as u32;
        packed.extend_from_slice(&self.words[self.words_of(span)]);
        Span {
            start,
            cap: span.len,
            len: span.len,
        }
    }

    /// Adopt a finished repack buffer: the free list is gone and the whole
    /// prefix has to be re-uploaded.
    pub fn repack_finish(&mut self, packed: Vec<u32>) {
        self.top = (packed.len() / self.stride as usize) as u32;
        self.words = packed;
        self.free.clear();
        self.freed = 0;
        self.full = true;
    }

    /// Make sure the GPU buffer holds the whole arena. Recreating it discards
    /// its contents, so this forces a full re-upload.
    pub fn reserve(&mut self, device: &wgpu::Device) {
        if self.top == 0 || (self.top <= self.capacity && self.buffer.is_some()) {
            return;
        }
        let capacity = self.top.next_power_of_two();
        self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(self.label),
            size: capacity as u64 * self.stride as u64 * 4,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.capacity = capacity;
        self.full = true;
    }

    /// Whether the next upload has to cover the whole arena.
    pub fn needs_full_upload(&self) -> bool {
        self.full
    }

    /// Upload the whole live prefix, holes included. Returns the instances
    /// written.
    pub fn upload_all(&mut self, queue: &wgpu::Queue) -> u32 {
        self.full = false;
        let (Some(buffer), true) = (self.buffer.as_ref(), self.top > 0) else {
            return 0;
        };
        let words = (self.top * self.stride) as usize;
        queue.write_buffer(buffer, 0, bytemuck::cast_slice(&self.words[..words]));
        self.top
    }

    /// Upload one span. Returns the instances written.
    pub fn upload(&self, queue: &wgpu::Queue, span: Span) -> u32 {
        let (Some(buffer), false) = (self.buffer.as_ref(), span.is_empty()) else {
            return 0;
        };
        let range = self.words_of(span);
        queue.write_buffer(
            buffer,
            self.byte_offset(span),
            bytemuck::cast_slice(&self.words[range]),
        );
        span.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in instance: one word, so element indices are word indices.
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable, Debug, PartialEq)]
    struct Word(u32);

    fn arena() -> Arena {
        Arena::new("test", std::mem::size_of::<Word>())
    }

    #[test]
    fn allocations_bump_then_reuse_the_free_list() {
        let mut arena = arena();
        let mut a = Span::default();
        let mut b = Span::default();
        arena.alloc(&mut a, 4);
        arena.alloc(&mut b, 4);
        assert_eq!(a.start, 0);
        assert_eq!(b.start, plan_cap(4));
        assert_eq!(arena.freed(), 0);

        arena.release(&mut a);
        assert_eq!(arena.freed(), plan_cap(4));
        let mut c = Span::default();
        arena.alloc(&mut c, 4);
        assert_eq!(c.start, 0, "the hole is reused before the bump pointer");
        assert_eq!(arena.freed(), 0);
    }

    #[test]
    fn a_span_that_still_fits_is_not_reallocated() {
        let mut arena = arena();
        let mut span = Span::default();
        arena.alloc(&mut span, 8);
        let start = span.start;
        arena.alloc(&mut span, 3);
        assert_eq!((span.start, span.len), (start, 3));
        assert_eq!(arena.freed(), 0, "shrinking must not churn the free list");
        arena.alloc(&mut span, 8);
        assert_eq!((span.start, span.len), (start, 8));
        assert_eq!(arena.top(), plan_cap(8));
    }

    #[test]
    fn a_large_hole_is_split_and_the_remainder_stays_free() {
        let mut arena = arena();
        let mut big = Span::default();
        arena.alloc(&mut big, 64);
        let cap = big.cap;
        arena.release(&mut big);

        let mut small = Span::default();
        arena.alloc(&mut small, 1);
        assert_eq!(small.start, 0);
        assert_eq!(arena.freed(), cap - small.cap, "the tail stayed free");
        assert_eq!(arena.top(), cap, "no bump-pointer growth");
    }

    #[test]
    fn repacking_compacts_live_spans_in_order() {
        let mut arena = arena();
        let mut spans = [Span::default(); 3];
        for (i, span) in spans.iter_mut().enumerate() {
            arena.alloc(span, 4);
            arena.write(*span, &[Word(i as u32 * 10); 4]);
        }
        arena.release(&mut spans[1]);
        assert!(arena.fragmentation() > 0.0);

        let mut packed = arena.repack_buffer();
        for span in &mut spans {
            *span = arena.repack_span(&mut packed, *span);
        }
        arena.repack_finish(packed);

        assert_eq!(arena.freed(), 0);
        assert_eq!(arena.top(), 8, "two live spans of four, tightly packed");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[1], Span::default());
        assert_eq!(spans[2].start, 4);
        assert_eq!(arena.slice_mut::<Word>(spans[2]), &[Word(20); 4]);
        assert!(arena.needs_full_upload());
    }
}
