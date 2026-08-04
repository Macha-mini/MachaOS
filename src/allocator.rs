use core::alloc::{GlobalAlloc, Layout};
use core::mem::size_of;
use core::ptr;

use crate::sync::SpinLock;

const MIN_ALIGN: usize = 16;
pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Block {
    size: usize,
    next: *mut Block,
}

pub struct FreeListAllocator {
    head: *mut Block,
    allocated: usize,
    total: usize,
}

unsafe impl Send for FreeListAllocator {}

impl FreeListAllocator {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            allocated: 0,
            total: 0,
        }
    }

    /// Initializes the free list over the half-open range [start, end).
    pub unsafe fn init(&mut self, start: usize, end: usize) {
        let total = end - start;
        let block = start as *mut Block;
        (*block).size = total - size_of::<Block>();
        (*block).next = ptr::null_mut();
        self.head = block;
        self.allocated = 0;
        self.total = total;
    }

    unsafe fn alloc(&mut self, size: usize) -> *mut u8 {
        let size = align_up(size, MIN_ALIGN);
        let mut previous: *mut Block = ptr::null_mut();
        let mut current = self.head;

        while !current.is_null() {
            let current_size = (*current).size;
            if current_size >= size {
                if current_size >= size + size_of::<Block>() + MIN_ALIGN {
                    // Split the block
                    let remaining = (current as usize + size_of::<Block>() + size) as *mut Block;
                    (*remaining).size = current_size - size - size_of::<Block>();
                    (*remaining).next = (*current).next;
                    (*current).size = size;
                    (*current).next = remaining;
                }
                if previous.is_null() {
                    self.head = (*current).next;
                } else {
                    (*previous).next = (*current).next;
                }
                self.allocated += size;
                return (current as usize + size_of::<Block>()) as *mut u8;
            }
            previous = current;
            current = (*current).next;
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        let block = (ptr as usize - size_of::<Block>()) as *mut Block;
        self.allocated -= (*block).size;

        // Insert sorted by address
        let mut previous: *mut Block = ptr::null_mut();
        let mut current = self.head;
        while !current.is_null() && (current as usize) < (block as usize) {
            previous = current;
            current = (*current).next;
        }
        (*block).next = current;
        if previous.is_null() {
            self.head = block;
        } else {
            (*previous).next = block;
        }

        // Coalesce with the next block
        if !current.is_null()
            && (block as usize + size_of::<Block>() + (*block).size) == current as usize
        {
            (*block).size += size_of::<Block>() + (*current).size;
            (*block).next = (*current).next;
        }
        // Coalesce with the previous block
        if !previous.is_null()
            && (previous as usize + size_of::<Block>() + (*previous).size) == block as usize
        {
            (*previous).size += size_of::<Block>() + (*block).size;
            (*previous).next = (*block).next;
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    pub fn free_bytes(&self) -> usize {
        self.total - self.allocated
    }

    pub fn total_bytes(&self) -> usize {
        self.total
    }
}

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

// `MIN_ALIGN`-aligned so every allocation this allocator can ever hand out
// (bounded by `MIN_ALIGN` in `alloc()` below) starts from a heap base that's
// actually aligned; a plain `[u8; N]` has no alignment guarantee beyond 1.
#[repr(align(16))]
struct Heap([u8; HEAP_SIZE]);

static mut HEAP: Heap = Heap([0; HEAP_SIZE]);

#[global_allocator]
static ALLOCATOR: SpinLock<FreeListAllocator> = SpinLock::new(FreeListAllocator::new());

unsafe impl GlobalAlloc for SpinLock<FreeListAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > MIN_ALIGN {
            return ptr::null_mut();
        }
        self.lock().alloc(layout.size())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if !ptr.is_null() {
            self.lock().dealloc(ptr);
        }
    }
}

pub fn init() {
    unsafe {
        let start = core::ptr::addr_of!(HEAP.0) as usize;
        let end = start + HEAP_SIZE;
        ALLOCATOR.lock().init(start, end);
    }
}

pub fn allocated_bytes() -> usize {
    ALLOCATOR.lock().allocated_bytes()
}

pub fn free_bytes() -> usize {
    ALLOCATOR.lock().free_bytes()
}

pub fn total_bytes() -> usize {
    ALLOCATOR.lock().total_bytes()
}
