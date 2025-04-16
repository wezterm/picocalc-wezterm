use core::alloc::{GlobalAlloc, Layout};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use embedded_alloc::LlffHeap as Heap;

#[global_allocator]
pub static HEAP: DualHeap = DualHeap::empty();
const HEAP_SIZE: usize = 64 * 1024;
static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
static mut HEAP_TWO: [MaybeUninit<u8>; 1024] = [MaybeUninit::uninit(); 1024];

struct Region {
    start: AtomicUsize,
    size: AtomicUsize,
}

impl Region {
    const fn default() -> Self {
        Self {
            start: AtomicUsize::new(0),
            size: AtomicUsize::new(0),
        }
    }

    fn contains(&self, address: usize) -> bool {
        let start = self.start.load(Ordering::Relaxed);
        let end = self.start.load(Ordering::Relaxed);
        (start..start + end).contains(&address)
    }

    fn new(start: usize, size: usize) -> Self {
        Self {
            start: AtomicUsize::new(start),
            size: AtomicUsize::new(size),
        }
    }
}

/// This is an allocator that combines two regions of memory.
/// The intent is to use some of the directly connected RAM
/// for this, and if we find some XIP capable PSRAM, add that
/// as a secondary region.
/// Allocation from the primary region is always preferred,
/// as it is expected to be a bit faster than PSRAM.
/// FIXME: PSRAM-allocated memory isn't compatible with
/// CAS atomics, so we might need a bit of a think about this!
pub struct DualHeap {
    primary: Heap,
    primary_region: Region,
    secondary: Heap,
}

impl DualHeap {
    pub const fn empty() -> Self {
        Self {
            primary: Heap::empty(),
            primary_region: Region::default(),
            secondary: Heap::empty(),
        }
    }

    unsafe fn add_primary(&self, region: Region) {
        let start = region.start.load(Ordering::SeqCst);
        let size = region.size.load(Ordering::SeqCst);
        unsafe {
            self.primary.init(start, size);
        }
        self.primary_region.start.store(start, Ordering::SeqCst);
        self.primary_region.size.store(size, Ordering::SeqCst);
    }

    unsafe fn add_secondary(&self, region: Region) {
        let start = region.start.load(Ordering::SeqCst);
        let size = region.size.load(Ordering::SeqCst);
        unsafe {
            self.secondary.init(start, size);
        }
    }

    pub fn used(&self) -> usize {
        self.primary.used() + self.secondary.used()
    }

    pub fn free(&self) -> usize {
        self.primary.free() + self.secondary.free()
    }
}

unsafe impl GlobalAlloc for DualHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            let ptr = self.primary.alloc(layout);
            if !ptr.is_null() {
                return ptr;
            }
            // start using secondary area when primary heap is full
            self.secondary.alloc(layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            let ptr_usize = ptr as usize;
            if self.primary_region.contains(ptr_usize) {
                self.primary.dealloc(ptr, layout);
            } else {
                self.secondary.dealloc(ptr, layout);
            }
        }
    }
}

pub fn init_heap() {
    let primary_start = &raw mut HEAP_MEM as usize;
    unsafe { HEAP.add_primary(Region::new(primary_start, HEAP_SIZE)) }

    // The idea is that internal PSRAM would get added as the secondary.
    // This is just a proof of concept that we can make an aggregating
    // heap allocator
    let secondary_start = &raw mut HEAP_TWO as usize;
    unsafe { HEAP.add_secondary(Region::new(secondary_start, 1024)) }
}

pub async fn free_command(_args: &[&str]) {
    print!(
        "{:<10} {:>10} {:>10} {:>10}\r\n",
        "", "TOTAL", "USED", "FREE"
    );

    let ram_used = HEAP.primary.used();
    let ram_free = HEAP.primary.free();
    let ram_total = ram_used + ram_free;
    print!(
        "{:<10} {ram_total:>10} {ram_used:>10} {ram_free:>10}\r\n",
        "RAM"
    );

    let xip_used = HEAP.secondary.used();
    let xip_free = HEAP.secondary.free();
    let xip_total = xip_used + xip_free;
    print!(
        "{:<10} {xip_total:>10} {xip_used:>10} {xip_free:>10}\r\n",
        "XIP"
    );
}
