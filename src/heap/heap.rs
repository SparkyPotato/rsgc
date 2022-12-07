use std::{
    mem::{size_of, MaybeUninit},
    ptr::null_mut,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use crate::{
    formatted_size,
    object::HeapObjectHeader,
    traits::{Visitor, WeakProcessor},
};

use super::{
    bitmap::HeapBitmap,
    controller::ControlThread,
    free_set::RegionFreeSet,
    region::{HeapArguments, HeapOptions, HeapRegion},
    safepoint,
    shared_vars::SharedFlag,
    virtual_memory::{self, VirtualMemory},
    AllocRequest, concurrent_thread::ConcurrentGCThread,
};
use parking_lot::{lock_api::RawMutex, RawMutex as Lock};

pub trait GlobalRoot {
    fn walk(&self, visitor: &mut dyn Visitor) -> bool;
}

pub struct Heap {
    pub(crate) lock: Lock,
    mem: Box<VirtualMemory>,
    regions_storage: Box<VirtualMemory>,
    regions: Vec<*mut HeapRegion>,
    opts: HeapOptions,
    free_set: RegionFreeSet,
    used: AtomicUsize,
    bytes_allocated_since_gc_start: AtomicUsize,
    last_cycle_end: Instant,
    controller_thread: Option<&'static mut ControlThread>,
    progress_last_gc: SharedFlag,
    object_start_bitmap: HeapBitmap<16>,

    weak_refs: Vec<*mut HeapObjectHeader>,
    global_roots: Vec<Box<dyn GlobalRoot>>,
    scoped_pool: scoped_thread_pool::Pool,
}

impl Heap {
    pub fn free_set(&self) -> &RegionFreeSet {
        &self.free_set
    }

    pub fn free_set_mut(&mut self) -> &mut RegionFreeSet {
        &mut self.free_set
    }

    pub fn options(&self) -> &HeapOptions {
        &self.opts
    }

    pub fn new(args: HeapArguments) -> &'static mut Self {
        safepoint::init();
        let opts = HeapRegion::setup_sizes(args);

        let mem = VirtualMemory::allocate_aligned(
            opts.max_heap_size,
            opts.region_size_bytes,
            false,
            false,
            "heap",
        )
        .unwrap();

        let regions_mem = VirtualMemory::allocate_aligned(
            opts.region_count * size_of::<HeapRegion>(),
            virtual_memory::page_size(),
            false,
            false,
            "heap regions",
        )
        .unwrap();
        let free_set = RegionFreeSet::new(&opts);

        let this = Box::leak(Box::new(Self {
            object_start_bitmap: HeapBitmap::new(mem.start(), mem.size()),
            regions: vec![null_mut(); opts.region_count],
            regions_storage: regions_mem,
            mem,
            opts,
            lock: Lock::INIT,
            free_set,
            used: AtomicUsize::new(0),
            bytes_allocated_since_gc_start: AtomicUsize::new(0),
            last_cycle_end: Instant::now(),
            controller_thread: None,
            progress_last_gc: SharedFlag::new(),
            weak_refs: vec![],
            global_roots: vec![],
            scoped_pool: scoped_thread_pool::Pool::new(opts.parallel_gc_threads),
        }));

        let ptr = this as *mut Heap;
        for i in 0..opts.region_count {
            let start = this.mem.start() + this.opts.region_size_bytes * i;
            let loc = this.regions_storage.start() + i * size_of::<HeapRegion>();

            unsafe {
                let region = HeapRegion::new(loc, i, start, &this.opts);
                this.regions[i] = region;
            }
        }

        this.free_set.set_heap(ptr);
        this.free_set.rebuild();

        unsafe {
            HEAP = MaybeUninit::new(HeapRef(&mut *(this as *mut Self)));

            heap().controller_thread = Some(ControlThread::new());
            
        }
        this
    }

    pub fn controller_thread<'a>(&'a self) -> &'a ControlThread {
        self.controller_thread.as_ref().unwrap()
    }

    pub fn object_start_bitmap(&self) -> &HeapBitmap<16> {
        &self.object_start_bitmap
    }

    pub fn object_start_bitmap_mut(&mut self) -> &mut HeapBitmap<16> {
        &mut self.object_start_bitmap
    }

    pub fn increase_used(&self, bytes: usize) {
        self.used
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn decrease_used(&self, bytes: usize) {
        self.used
            .fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_used(&self, bytes: usize) {
        self.used.store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn increase_allocated(&self, bytes: usize) {
        self.bytes_allocated_since_gc_start
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn decrease_allocated(&self, bytes: usize) {
        self.bytes_allocated_since_gc_start
            .fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_allocated(&self, bytes: usize) {
        self.bytes_allocated_since_gc_start
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn request_gc(&self) {
        self.controller_thread().handle_requested_gc();
    }

    pub fn notify_mutator_alloc(&self, bytes: usize, waste: bool) {
        if !waste {
            self.increase_used(bytes);
        }

        self.increase_allocated(bytes);
    }

    pub fn notify_gc_progress(&self) {
        self.progress_last_gc.set();
    }

    pub fn notify_no_gc_progress(&self) {
        self.progress_last_gc.unset();
    }

    pub fn region_index(&self, object: *mut u8) -> usize {
        let offset = object as usize - self.mem.start();
        offset >> self.opts.region_size_bytes_shift
    }

    pub fn mem_start(&self) -> usize {
        self.mem.start()
    }

    pub fn mem_end(&self) -> usize {
        self.mem.end()
    }

    pub fn num_regions(&self) -> usize {
        self.opts.region_count
    }

    pub fn get_region(&self, index: usize) -> *mut HeapRegion {
        self.regions[index]
    }

    pub fn add_global_root(&mut self, root: Box<dyn GlobalRoot>) {
        self.lock.lock();
        self.global_roots.push(root);
        unsafe {
            self.lock.unlock();
        }
    }

    pub fn add_weak_ref(&mut self, weak_ref: *mut HeapObjectHeader) {
        self.lock.lock();
        self.weak_refs.push(weak_ref);
        unsafe {
            self.lock.unlock();
        }
    }

    pub unsafe fn walk_global_roots(&mut self, vis: &mut dyn Visitor) {
        self.global_roots.retain(|root| {
            root.walk(vis)
        });
    }

    pub unsafe fn process_weak_refs(&mut self) {
        struct SimpleWeakProcessor {}

        impl WeakProcessor for SimpleWeakProcessor {
            fn process(&mut self, object: *const u8) -> *const u8 {
                let header =
                    (object as usize - size_of::<HeapObjectHeader>()) as *const HeapObjectHeader;
                unsafe {
                    if (*header).is_marked() {
                        header.add(1) as _
                    } else {
                        null_mut()
                    }
                }
            }
        }

        self.weak_refs.retain(|obj| {
            let obj = *obj;
            if !(*obj).is_marked() {
                return false;
            }

            let vt = (*obj).vtable();

            (vt.weak_process)(obj.add(1).cast(), &mut SimpleWeakProcessor {});

            true
        })
    }

    pub fn max_capacity(&self) -> usize {
        self.regions.len() * self.options().region_size_bytes
    }

    pub unsafe fn max_tlab_alloc(&self) -> usize {
        if self.options().elastic_tlab {
            self.options().max_tlab_size
        } else {
            self.free_set
                .unsafe_peek_free()
                .min(self.options().max_tlab_size)
        }
    }

    pub fn should_start_gc(&self) -> bool {
        let max_capacity = self.max_capacity();
        let available = self.free_set().available();

        let threshold_available = max_capacity / 100 * self.options().min_free_threshold;
        let threshold_bytes_allocated = max_capacity / 100 * self.options().allocation_threshold;
        if available < threshold_available {
            log::info!(target: "gc",
                "Trigger: Free ({}) is below minimum threshold ({})",
                formatted_size(available),
                formatted_size(threshold_available)
            );
            return true;
        }

        let bytes_allocated = self.bytes_allocated_since_gc_start.load(Ordering::Relaxed);

        if bytes_allocated > threshold_bytes_allocated {
            log::info!(target: "gc",
                "Trigger: Allocated since last cycle ({}) is larger than allocation threshold ({})",
                formatted_size(bytes_allocated),
                formatted_size(threshold_bytes_allocated)
            );
            return true;
        }

        if self.options().guaranteed_gc_interval > 0 {
            let last_time_ms = self.last_cycle_end.elapsed().as_millis();

            if last_time_ms > self.options().guaranteed_gc_interval as _ {
                log::info!(target: "gc",
                    "Trigger: Time since last GC ({} ms) is larger than guaranteed interval ({} ms)",
                    last_time_ms,
                    self.options().guaranteed_gc_interval
                );
                return true;
            }
        }

        false
    }

    fn allocate_memory_under_lock(
        &mut self,
        req: &mut AllocRequest,
        in_new_region: &mut bool,
    ) -> *mut u8 {
        self.lock.lock();
        let mem = self.free_set.allocate(req, in_new_region);
        unsafe {
            self.lock.unlock();
        }
        mem
    }

    pub fn allocate_memory(&mut self, req: &mut AllocRequest) -> *mut u8 {
        let mut in_new_region = false;
        let mut result;

        result = self.allocate_memory_under_lock(req, &mut in_new_region);

        // Allocation failed, block until control thread reacted, then retry allocation.
        //
        // It might happen that one of the threads requesting allocation would unblock
        // way later after GC happened, only to fail the second allocation, because
        // other threads have already depleted the free storage. In this case, a better
        // strategy is to try again, as long as GC makes progress.
        //
        // Then, we need to make sure the allocation was retried after at least one
        // Full GC, which means we want to try more than 3 times.
        let mut tries = 0;

        while result.is_null() && self.progress_last_gc.is_set() {
            tries += 1;
            self.controller_thread().handle_alloc_failure_gc(req);
            result = self.allocate_memory_under_lock(req, &mut in_new_region);
        }

        while result.is_null() && tries <= 3 {
            tries += 1;
            self.controller_thread().handle_alloc_failure_gc(req);
            result = self.allocate_memory_under_lock(req, &mut in_new_region);
        }

        if in_new_region {
            self.controller_thread().notify_heap_changed();
        }

        if !result.is_null() {
            let requested = req.size();
            let actual = req.actual_size();

            assert!(
                req.for_lab() || requested == actual,
                "Only LAB allocations are elastic, requested {}, actual = {}",
                formatted_size(requested),
                formatted_size(actual)
            );

            self.notify_mutator_alloc(actual, false);
        }
        result
    }

    pub fn rebuild_free_set(&mut self, concurrent: bool) {
        let _ = concurrent;
        self.free_set.rebuild();
    }

    pub fn heap_region_iterate(&self, blk: &dyn HeapRegionClosure) {
        for i in 0..self.num_regions() {
            let current = self.get_region(i);
            blk.heap_region_do(current);
        }
    }

    pub(crate) unsafe fn trash_humongous_region_at(&self, start: *mut HeapRegion) {
        let humongous_obj = (*start).bottom() as *mut HeapObjectHeader;
        let sz = (*humongous_obj).heap_size();
        let required_regions = self.options().required_regions(sz);

        let mut index = (*start).index() + required_regions - 1;

        for _ in 0..required_regions {
            let region = self.get_region(index);
            index -= 1;

            (*region).make_trash();
        }
    }

    /// Mark address as live object.
    ///
    /// # Safety
    ///
    /// Caller must ensure that `addr` points to valid object.
    pub unsafe fn mark_live(&self, addr: *mut u8) {
        unsafe {
            let index = self.region_index(addr);
            let region = self.get_region(index);

            if (*region).is_humongous_start() {
                // no-op
                return;
            }

            if (*region).is_humongous_cont() {
                debug_assert!(
                    false,
                    "Humongous object should be marked as live by its start region"
                );
            }

            let bitmap = &(*region).object_start_bitmap;
            bitmap.set_atomic(addr as usize);
        }
    }

    pub fn is_live(&self, addr: *mut u8) -> bool {
        if self.mem.contains(addr as _) {
            return false;
        }
        let index = self.region_index(addr);
        let region = self.get_region(index);

        unsafe {
            if (*region).is_humongous_start() {
                return true;
            }

            if (*region).is_humongous_cont() {
                return true;
            }

            let bitmap = &(*region).object_start_bitmap;
            bitmap.check_bit(addr as usize)
        }
    }

    pub unsafe fn get_humongous_start(&self, mut r: *mut HeapRegion) -> *mut HeapRegion {
        let mut i = (*r).index();

        while !(*r).is_humongous_start() {
            i -= 1;
            r = self.get_region(i);
        }

        r
    }

    pub fn is_in(&self, addr: *mut u8) -> bool {
        self.mem.contains(addr as _)
    }

    pub fn object_start(&self, addr: *mut u8) -> *mut HeapObjectHeader {
        if !self.mem.contains(addr as _) {
            return null_mut();
        }
        let index = self.region_index(addr);
        let region = self.get_region(index);

        unsafe {
            if (*region).is_humongous_start() {
                return (*region).bottom() as *mut HeapObjectHeader;
            }

            if (*region).is_humongous_cont() {
                let start = self.get_humongous_start(region);
                return (*start).bottom() as *mut HeapObjectHeader;
            }

            let bitmap = &(*region).object_start_bitmap;
            let start = bitmap.find_object_start(addr as _);
            start as *mut HeapObjectHeader
        }
    }

    pub fn tlab_capacity(&self) -> usize {
        self.free_set().capacity()
    }

    pub unsafe fn stop(&mut self) {
        let _ = Box::from_raw(self as *mut Self);
        log::debug!("Heap stopped");
    }
}

struct HeapRef(&'static mut Heap);

unsafe impl Send for HeapRef {}
unsafe impl Sync for HeapRef {}

static mut HEAP: MaybeUninit<HeapRef> = MaybeUninit::uninit();

pub fn heap() -> &'static mut Heap {
    unsafe { HEAP.assume_init_mut().0 }
}

pub trait HeapRegionClosure: Send {
    fn heap_region_do(&self, r: *mut HeapRegion);
}

impl Drop for Heap {
    fn drop(&mut self) {
        self.controller_thread.as_mut().unwrap().stop();
    }
}
