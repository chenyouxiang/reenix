// TODO Copyright Header

//! A last ditch allocator.

use core::prelude::*;
use super::page;
use core::cmp;
use core::mem::{size_of, transmute};
use core::ptr::{write_bytes, write};
use core::fmt;

const FREE_FILL : u8 = 0xF7;
const ALOC_FILL : u8 = 0x7F;

/// This is a free list allocator. It allocates in two ways. A best fit allocator from the front
/// for small objects and a best fit allocator from the back for > PAGE_SIZE objects. It does this
/// to try to prevent fragmentation. This is implemented as an extreemly simple free list
/// allocator. Boundary tags are simple extant tags, with the low bit being if it is allocated or
/// not. All tags are on size_of::<Tag> alignments. Allocations > PAGE_SIZE are always aligned on
/// page boundaries.
pub struct BackupAllocator {
    buf             : *mut u8,
    pages           : usize,
    largest_space   : usize, // The largest continuous page aligned space in number of pages
    threshold_pages : usize, // The size below which we will consider space low in pages.
    //next_allocator  : *mut BackupAllocator,
}

const DEFAULT_BACKUP_PAGES : usize = 128;

#[cfg(not(TEST_LOW_MEMORY))]
const DEFAULT_THRESHOLD    : usize = 16;

#[cfg(TEST_LOW_MEMORY)]
const DEFAULT_THRESHOLD    : usize = 120;

pub const DEFAULT_BACKUP_ALLOCATOR : BackupAllocator = BackupAllocator {
    buf             : 0 as *mut u8,
    pages           : 0,
    largest_space   : 0,
    threshold_pages : 0,
    //next_allocator  : 0 as *mut BackupAllocator,
};

/// Number of pages it would take to hold that many bytes.
#[inline] fn pg_size(u: usize) -> usize { unsafe { page::addr_to_num(page::const_align_up(u as *const u8)) } }

/// A tag is usizeptr bits of length. The LSB is true if this has been allocated, false otherwise.
struct Tag(usize);

impl Tag {
    pub fn new(size : usize) -> Tag {
        assert!((size & 0x1) == 0, "size of {} is illegal", size);
        Tag(size)
    }

    pub fn get_tag_ptr(&self) -> *mut Tag { unsafe { transmute(self as *const Tag) } }

    pub fn get_start(&self) -> *mut u8 { unsafe { transmute(self.get_tag_ptr().offset(1)) } }

    pub fn is_allocated(&self) -> bool { (self.0 & 0x1) != 0 }
    pub fn is_free(&self) -> bool { !self.is_allocated() }

    pub fn set_allocated(&mut self) { *self = Tag(self.0 | 0x1); }
    pub fn set_free(&mut self) { *self = Tag(self.size()); }

    pub fn size(&self) -> usize { (self.0) & (!0x1) }

    pub fn set_size(&mut self, size: usize) { *self = Tag(size | if self.is_allocated() { 0x1 } else { 0x0 }) }

    pub fn next(&self) -> *mut Tag { unsafe { transmute(self.get_start().offset(self.size() as isize)) } }

    pub fn get_page_aligned_part(&self, requested_pages: usize) -> Option<(*mut Tag, *mut Tag)> {
        // mem is CTAG........[:::::::::::::::::::::::::::::::::::::::::]....CTAG => GOOD
        // mem is         CTAG[:::::::::::::::::::::::::::::::::::::::::]....CTAG => GOOD
        // mem is CTAG........[:::::::::::::::::::::::::::::::::::::::::]CTAG     => GOOD
        // mem is CTAG........[:::::::::::::::::::::::::::::::::::::CTAG]         => BAD
        //                    ^ ----------- Page Boundarys -------------^
        // How many bytes do we need.
        let nbytes = unsafe { page::num_to_addr::<u8>(requested_pages) as usize };
        // minimum bytes between start of tagged region and start of page region.
        let pre_bytes = (page::SIZE - page::offset::<u8>(self.get_start() as *const u8)) & page::MASK;

        // Will this work?
        assert!(self.is_free());
        if self.size() < nbytes + pre_bytes {
            // No
            None
        } else {
            // We want to put this as far back as possible, prevent fragmentation with smaller allocs
            // in the front. Transitivity says this is okay.
            let end = unsafe { page::align_down(self.next()) };
            let start = unsafe { page::num_to_addr::<Tag>(page::addr_to_num(end as *const Tag) - requested_pages).offset(-1) };
            bassert!(start as usize >= self.get_tag_ptr() as usize);
            assert!(page::aligned(unsafe {  (start as *const Tag).offset(1) }));
            Some((start, end))
        }
    }
}

impl BackupAllocator {
    /// Creates a new backup allocator with 'size' pages of memory and which will consider itself
    /// having low memory when fewer then 'threshold' continuous pages are availible.
    pub fn new(size : usize, threshold : usize) -> BackupAllocator {
        let mut ret = BackupAllocator {
            buf : unsafe {
                page::alloc_n(size).unwrap_or_else(|_| { kpanic!("Unable to allocate space for backup allocator"); })
            },
            pages : size,
            largest_space : size - 1,
            threshold_pages : threshold,
        };
        ret.setup();
        ret
    }

    pub fn is_used(&self) -> bool {
        let start_tag = self.read_tag(self.buf as *mut Tag).expect("shouldn't be null");
        !(start_tag.is_free() && (self.byte_len() as usize) - size_of::<Tag>() == start_tag.size())
    }

    pub fn allocate(&self, size: usize, align: usize) -> *mut u8 {
        // Force everything to be aligned by size_of::<Tag>.
        let req = (size + (size_of::<Tag>() - 1)) & (!(size_of::<Tag>() - 1));
        let res = self.real_allocate(req, align);
        unsafe { transmute::<&BackupAllocator, &mut BackupAllocator>(self).recalculate() };
        if !res.is_null() {
            unsafe { write_bytes(res, ALOC_FILL, size); }
            let recieved_size =  unsafe { (res as *const Tag).offset(-1).as_ref().expect("shouldn't be null").size() };
            dbg!(debug::MM|debug::BACKUP_MM, "allocated {:p}-{:p} which is {} bytes long for request for {}",
                 res, unsafe { res.offset(recieved_size as isize) }, recieved_size, size);
            if self.is_memory_low() {
                dbg!(debug::MM|debug::DANGER, "We are currently low on memory! Largest space is {}", self.largest_space);
            }
        } else {
            dbg!(debug::MM|debug::BACKUP_MM, "unable to allocate {} bytes from backup", size);
        }
        res
    }
    fn real_allocate(&self, size: usize, _align: usize) -> *mut u8 {
        assert!((size % size_of::<Tag>()) == 0, "size of {} is not aligned to {}", size, size_of::<Tag>());
        if pg_size(size) > self.largest_space + 1 {
            dbg!(debug::MM|debug::DANGER, "Unable to allocate {} bytes from backup memory allocator!", size);
            0 as *mut u8
        } else if size >= page::SIZE {
            self.allocate_pages(pg_size(size))
        } else {
            self.allocate_small(size)
        }
    }
    fn allocate_small(&self, req: usize) -> *mut u8 {
        // Make size be even.
        let mut best : Option<*mut Tag> = None;
        let mut c = self.read_tag(self.buf as *mut Tag);
        while c.is_some() {
            let cur = c.expect("Isn't null");
            if cur.is_free() && cur.size() >= req {
                if cur.size() == req || cur.size() == req + size_of::<Tag>() {
                    // Size is an exact match, or close enough that the next split tag would be 0
                    // length, which is good enough. Nothing should break with 0 lenth tags but we
                    // might as well avoid them on principle.
                    cur.set_allocated();
                    return cur.get_start();
                } else if best.clone().map(|t| { unsafe { t.as_mut().expect("not null").size() } }).unwrap_or(::core::usize::MAX) > req {
                    best = Some(cur as *mut Tag);
                }
            }
            c = self.read_tag(cur.next());
        }
        match best {
            Some(t) => {
                let tag = unsafe { t.as_mut().expect("not null") };
                let old_size = tag.size();
                let remaining_size = old_size - size_of::<Tag>() - req;
                tag.set_size(req);
                tag.set_allocated();
                if let Some(new_tag) = self.read_tag(tag.next()) {
                    *new_tag = Tag::new(remaining_size);
                }
                tag.get_start()
            },
            None => {
                dbg!(debug::MM|debug::DANGER, "Unable to allocate {} bytes from backup memory allocator!. No suitable segments", req);
                0 as *mut u8
            }
        }
    }

    fn allocate_pages(&self, pgs: usize) -> *mut u8 {
        let mut best : Option<(*mut Tag, (*mut Tag, *mut Tag))> = None;
        let mut c = self.read_tag(self.buf as *mut Tag);
        while c.is_some() {
            let cur = c.expect("Isn't null");
            if cur.is_free() {
                let cp = cur as *mut Tag;
                best = cur.get_page_aligned_part(pgs).map(|v| { (cp, v) }).or(best);
            }
            c = self.read_tag(cur.next());
        }
        if let Some((tag, (split_low, split_hi))) = best {
            let t = unsafe { tag.as_mut().expect("not null") };
            if t.get_tag_ptr() == split_low && t.next() == split_hi {
                bassert!(pg_size(t.size()) == pgs);
                assert!(page::aligned(t.size() as *const u8));
                t.set_allocated();
                t.get_start()
            } else {
                let new_start_size = (split_low as usize) - t.get_start() as usize;
                assert!(new_start_size % 4 == 0, "start size {} is not 4 byte aligned", new_start_size);
                if split_hi as usize != t.next() as usize {
                    let new_end_size = t.next() as usize - (split_hi as usize + size_of::<Tag>());
                    if let Some(end) = self.read_tag(split_hi) {
                        end.set_size(new_end_size);
                        end.set_free();
                    }
                }
                t.set_size(new_start_size);
                t.set_free();
                let start = self.read_tag(split_low).expect("should never be null");
                start.set_size(unsafe { page::num_to_addr::<u8>(pgs) as usize });
                start.set_allocated();
                start.get_start()
            }
        } else {
            dbg!(debug::MM|debug::DANGER, "Unable to to allocate {} pages from backup allocator!", pgs);
            0 as *mut u8
        }
    }

    pub fn deallocate(&self, ptr: *mut u8, size: usize, align: usize) {
        unsafe { write_bytes(ptr, FREE_FILL, size); }
        dbg!(debug::MM|debug::BACKUP_MM, "Request to deallocate {:p} of size {}", ptr, size);
        let req = (size + (size_of::<Tag>() - 1)) & (!(size_of::<Tag>() - 1));
        self.real_deallocate(ptr, req, align);
        unsafe { transmute::<&BackupAllocator, &mut BackupAllocator>(self).recalculate(); }
    }

    fn real_deallocate(&self, ptr: *mut u8, size: usize, _align: usize) {
        assert!((size % size_of::<Tag>()) == 0, "size of {} is not aligned to {}", size, size_of::<Tag>());
        if size >= page::SIZE {
            self.deallocate_pages(ptr, pg_size(size))
        } else {
            self.deallocate_small(ptr, size)
        }
    }

    fn deallocate_small(&self, ptr: *mut u8, size: usize) {
        let t = unsafe { self.read_tag((ptr as *mut Tag).offset(-1)).expect("should exist") };
        assert!(t.size() == size || t.size() == size + size_of::<Tag>(), "(t.size() = {}) == (size = {}) failed", t.size(), size);
        t.set_free();
    }

    fn deallocate_pages(&self, ptr: *mut u8, pgs: usize) {
        assert!(page::aligned(ptr as *const u8));
        self.deallocate_small(ptr, unsafe { page::num_to_addr::<u8>(pgs) as usize });
    }

    /// Returns true if this ptr needs to be deallocated from the backup
    pub fn contains(&self, ptr: *mut u8) -> bool {
        let v = ptr as usize;
        self.buf as usize <= v && v < unsafe { self.buf.offset(self.byte_len()) as usize }
    }

    pub fn setup(&mut self) {
        unsafe {
            write_bytes::<u8>(self.buf, 0, page::num_to_addr::<u8>(self.pages as usize) as usize);
            write(self.buf as *mut Tag, Tag::new((self.byte_len() as usize) - size_of::<Tag>()));
        }
    }

    fn byte_len(&self) -> isize { unsafe { page::num_to_addr::<u8>(self.pages as usize) as isize } }

    pub fn read_tag<'a>(&'a self, t: *mut Tag) -> Option<&'a mut Tag> {
        unsafe {
            let st = self.buf as usize;
            let nd = self.buf.offset(self.byte_len()) as usize;
            let v = t as usize;
            if st <= v && v < nd { Some(&mut *t) } else { None }
        }
    }

    fn do_recalculate(&mut self) -> usize {
        let mut largest = 0;
        let mut prev = self.read_tag(self.buf as *mut Tag).expect("shouldn't be null");
        if prev.is_free() {
            largest = pg_size(prev.size()) - 1;
        }
        'outer: loop {
            match self.read_tag(prev.next()) {
                Some(cur) => {
                    assert!(cur.size() % size_of::<Tag>() == 0);
                    if prev.is_free() && cur.is_free() {
                        // Coalesce.
                        let psize = prev.size();
                        prev.set_size(psize + cur.size() + size_of::<Tag>());
                        largest = cmp::max(largest, pg_size(prev.size()) - 1);
                    } else if cur.is_free() {
                        largest = cmp::max(largest, pg_size(cur.size()) - 1);
                    }
                    prev = cur;
                },
                None => { break 'outer; }
            }
        };
        largest
    }

    /// Recalculate all the information about the backup allocator.
    fn recalculate(&mut self) {
        self.largest_space = self.do_recalculate();
        dbg!(debug::MM|debug::BACKUP_MM, "largest space is {}", self.largest_space);
    }

    pub fn finish(&mut self) {
        if self.buf == 0 as *mut u8 {
            *self = BackupAllocator::new(DEFAULT_BACKUP_PAGES, DEFAULT_THRESHOLD);
        }
    }

    pub fn is_memory_low(&self) -> bool {
        self.is_used() && self.pages - self.largest_space > self.threshold_pages
    }
    fn calc_total_space(&self) -> usize {
        let mut tot = 0;
        let mut prev = self.read_tag(self.buf as *mut Tag).expect("shouldn't be null");
        if prev.is_free() {
            tot = prev.size();
        }
        'outer: loop {
            match self.read_tag(prev.next()) {
                Some(cur) => {
                    if cur.is_free() {
                        tot += cur.size();
                    }
                    prev = cur;
                },
                None => { break 'outer; }
            }
        };
        tot
    }
}

impl fmt::Debug for BackupAllocator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BackupAllocator {{ used: {}, npages: {}, threshold: {} (pages), largest_space: {} (pages), total_space: {} }}",
               self.is_used(), self.pages, self.threshold_pages, self.largest_space, self.calc_total_space())
    }
}
