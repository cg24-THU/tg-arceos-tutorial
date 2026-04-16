#![no_std]

use axallocator::{AllocResult, BaseAllocator, ByteAllocator, PageAllocator};
use axallocator::AllocError;
use core::alloc::Layout;
use core::ptr::NonNull;

/// Early memory allocator
/// Use it before formal bytes-allocator and pages-allocator can work!
/// This is a double-end memory range:
/// - Alloc bytes forward
/// - Alloc pages backward
///
/// [ bytes-used | avail-area | pages-used ]
/// |            | -->    <-- |            |
/// start       b_pos        p_pos       end
///
/// For bytes area, 'count' records number of allocations.
/// When it goes down to ZERO, free bytes-used area.
/// For pages area, it will never be freed!
///
pub struct EarlyAllocator<const PAGE_SIZE: usize> {
    start: usize,
    end: usize,
    b_pos: usize,
    p_pos: usize,
    count: usize,
}

impl<const PAGE_SIZE: usize> EarlyAllocator<PAGE_SIZE> {
    pub const fn new() -> Self {
        Self {
            start: 0,
            end: 0,
            b_pos: 0,
            p_pos: 0,
            count: 0,
        }
    }

    const fn align_up(pos: usize, align: usize) -> usize {
        (pos + align - 1) & !(align - 1)
    }

    const fn align_down(pos: usize, align: usize) -> usize {
        pos & !(align - 1)
    }

    fn is_initialized(&self) -> bool {
        self.end > self.start
    }

    fn reset_bytes_if_empty(&mut self) {
        if self.count == 0 {
            self.b_pos = self.start;
        }
    }
}

impl<const PAGE_SIZE: usize> BaseAllocator for EarlyAllocator<PAGE_SIZE> {
    fn init(&mut self, start: usize, size: usize) {
        assert!(size > 0);
        self.start = start;
        self.end = start + size;
        self.b_pos = start;
        self.p_pos = self.end;
        self.count = 0;
    }

    fn add_memory(&mut self, start: usize, size: usize) -> AllocResult {
        if size == 0 {
            return Ok(());
        }
        let end = start.checked_add(size).ok_or(AllocError::InvalidParam)?;
        if !self.is_initialized() {
            self.init(start, size);
            return Ok(());
        }
        if end <= self.start {
            if end == self.start {
                self.start = start;
                self.reset_bytes_if_empty();
                return Ok(());
            }
            return Err(AllocError::InvalidParam);
        }
        if start >= self.end {
            if start == self.end {
                self.end = end;
                if self.p_pos == start {
                    self.p_pos = end;
                }
                return Ok(());
            }
            return Err(AllocError::InvalidParam);
        }
        Err(AllocError::MemoryOverlap)
    }
}

impl<const PAGE_SIZE: usize> ByteAllocator for EarlyAllocator<PAGE_SIZE> {
    fn alloc(&mut self, layout: Layout) -> AllocResult<NonNull<u8>> {
        if layout.size() == 0 {
            return Ok(NonNull::dangling());
        }
        let start = Self::align_up(self.b_pos, layout.align());
        let end = start
            .checked_add(layout.size())
            .ok_or(AllocError::NoMemory)?;
        if end > self.p_pos {
            return Err(AllocError::NoMemory);
        }
        self.b_pos = end;
        self.count += 1;
        Ok(NonNull::new(start as *mut u8).unwrap())
    }

    fn dealloc(&mut self, _pos: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 || self.count == 0 {
            return;
        }
        self.count -= 1;
        self.reset_bytes_if_empty();
    }

    fn total_bytes(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    fn used_bytes(&self) -> usize {
        if self.count == 0 {
            0
        } else {
            self.b_pos.saturating_sub(self.start)
        }
    }

    fn available_bytes(&self) -> usize {
        self.p_pos.saturating_sub(self.b_pos)
    }
}

impl<const PAGE_SIZE: usize> PageAllocator for EarlyAllocator<PAGE_SIZE> {
    const PAGE_SIZE: usize = PAGE_SIZE;

    fn alloc_pages(&mut self, num_pages: usize, align_pow2: usize) -> AllocResult<usize> {
        if num_pages == 0 || !align_pow2.is_power_of_two() {
            return Err(AllocError::InvalidParam);
        }
        let size = num_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(AllocError::NoMemory)?;
        let align = align_pow2.max(PAGE_SIZE);
        let start = self.p_pos.checked_sub(size).ok_or(AllocError::NoMemory)?;
        let aligned = Self::align_down(start, align);
        if aligned < self.b_pos {
            return Err(AllocError::NoMemory);
        }
        self.p_pos = aligned;
        Ok(aligned)
    }

    fn dealloc_pages(&mut self, _pos: usize, _num_pages: usize) {
        // This allocator only bumps page allocations backwards and never reuses them.
    }

    fn alloc_pages_at(
        &mut self,
        base: usize,
        num_pages: usize,
        align_pow2: usize,
    ) -> AllocResult<usize> {
        if num_pages == 0 || !align_pow2.is_power_of_two() {
            return Err(AllocError::InvalidParam);
        }
        let size = num_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(AllocError::NoMemory)?;
        let align = align_pow2.max(PAGE_SIZE);
        if base != Self::align_down(base, align) {
            return Err(AllocError::InvalidParam);
        }
        let end = base.checked_add(size).ok_or(AllocError::NoMemory)?;
        if end != self.p_pos || base < self.b_pos {
            return Err(AllocError::NoMemory);
        }
        self.p_pos = base;
        Ok(base)
    }

    fn total_pages(&self) -> usize {
        self.total_bytes() / PAGE_SIZE
    }

    fn used_pages(&self) -> usize {
        self.end.saturating_sub(self.p_pos) / PAGE_SIZE
    }

    fn available_pages(&self) -> usize {
        self.available_bytes() / PAGE_SIZE
    }
}
