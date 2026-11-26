//! Unit tests for the soft-MMU page table (mmu.c): the operations on-demand filling uses.

use std::ptr::NonNull;

use appine_engine::{MapError, Mmu, PAGE_SIZE, VA_BITS, prot};

#[repr(C, align(4096))]
struct Page([u8; 4096]);

const RW: u32 = prot::READ | prot::WRITE;
/// The range one leaf table covers.
const LEAF: u64 = PAGE_SIZE << 9;

#[test]
fn page_watch_expires_on_replacement_even_if_the_host_address_is_reused() {
    let mut mem = pages(2);
    let mut mmu = Mmu::new();
    let src = LEAF;
    let dst = src + PAGE_SIZE;
    let backing = host(&mut mem, 0);
    unsafe { mmu.map(src, backing, 2 * PAGE_SIZE, RW).unwrap() };
    let original = mmu.watch_readable_page(src).unwrap();
    let destination = mmu.watch_readable_page(dst).unwrap();
    mmu.protect(src, PAGE_SIZE, 0).unwrap();
    assert!(mmu.watch_readable_page(src).is_none());
    mmu.protect(src, PAGE_SIZE, RW).unwrap();
    unsafe { mmu.fill_shared(src, backing, PAGE_SIZE, RW).unwrap() };
    assert!(original.is_current());

    unsafe { mmu.map_shared(src, backing, PAGE_SIZE, RW).unwrap() };
    let replacement = mmu.watch_readable_page(src).unwrap();
    assert!(!original.is_current());
    assert!(destination.is_current());
    assert!(mmu.move_entries(src, src, PAGE_SIZE).is_err());
    assert!(replacement.is_current());
    mmu.move_entries(src, dst, PAGE_SIZE).unwrap();
    assert!(!replacement.is_current());
    assert!(!destination.is_current());
}

#[test]
fn fill_only_adds_missing_entries() {
    let mut mem = pages(4);
    let mmu = Mmu::new();
    let a = 0x10_0000;
    // Page 1 is first mapped elsewhere, and page 2 keeps an entry with permission 0 (as left by
    // copy-on-write or PROT_NONE)
    unsafe {
        mmu.map_shared(a + PAGE_SIZE, host(&mut mem, 3), PAGE_SIZE, prot::READ)
            .unwrap();
        mmu.map_shared(a + 2 * PAGE_SIZE, host(&mut mem, 2), PAGE_SIZE, 0)
            .unwrap();
        mmu.fill_shared(a, host(&mut mem, 0), 3 * PAGE_SIZE, RW)
            .unwrap();
    }
    assert_eq!(mmu.translate(a, prot::WRITE), Some(host(&mut mem, 0)));
    assert_eq!(mmu.translate(a + PAGE_SIZE, 0), Some(host(&mut mem, 3)));
    assert!(mmu.translate(a + PAGE_SIZE, prot::WRITE).is_none());
    // An entry with permission 0 cannot be accessed and does not count as mapped even when 0 is
    // passed; restoring a permission makes it usable again
    assert!(mmu.translate(a + 2 * PAGE_SIZE, 0).is_none());
    assert_eq!(
        mmu.next_present(a + 2 * PAGE_SIZE, a + LEAF),
        a + 2 * PAGE_SIZE
    );
    mmu.protect_shared(a + 2 * PAGE_SIZE, PAGE_SIZE, RW)
        .unwrap();
    assert_eq!(
        mmu.translate(a + 2 * PAGE_SIZE, RW),
        Some(host(&mut mem, 2))
    );
}

#[test]
fn ranges_skip_missing_tables_at_every_level() {
    let mut mem = pages(2);
    let mut mmu = Mmu::new();
    let top = 1u64 << 48;
    let (lo, hi) = (0x7000_0000, top - 3 * LEAF);
    unsafe {
        mmu.map(lo, host(&mut mem, 0), PAGE_SIZE, RW).unwrap();
        mmu.map(hi, host(&mut mem, 1), PAGE_SIZE, prot::READ)
            .unwrap();
    }
    // Only two pages in the whole address space: lookup, unmap and protect all skip the places
    // without tables
    assert_eq!(mmu.next_present(0, top), lo);
    assert_eq!(mmu.next_present(lo + PAGE_SIZE, top), hi);
    assert_eq!(mmu.next_present(hi + PAGE_SIZE, top), top);
    mmu.protect(0, top, prot::READ).unwrap();
    assert!(mmu.translate(lo, prot::WRITE).is_none());
    assert!(mmu.translate(lo, prot::READ).is_some());
    mmu.unmap(PAGE_SIZE, top - PAGE_SIZE).unwrap();
    assert_eq!(mmu.next_present(0, top), top);
}

#[test]
fn move_carries_entries_and_their_permissions() {
    let mut mem = pages(3);
    let mut mmu = Mmu::new();
    let (from, to) = (LEAF - PAGE_SIZE, 0x40_0000_0000);
    unsafe {
        mmu.map(from, host(&mut mem, 0), PAGE_SIZE, RW).unwrap();
        // Crossing leaf tables; the middle page has no entry, and the third page has permission 0
        mmu.map(from + 2 * PAGE_SIZE, host(&mut mem, 1), PAGE_SIZE, 0)
            .unwrap();
    }
    mmu.move_entries(from, to, 3 * PAGE_SIZE).unwrap();
    assert_eq!(
        mmu.next_present(from, from + 3 * PAGE_SIZE),
        from + 3 * PAGE_SIZE
    );
    assert_eq!(mmu.translate(to, prot::WRITE), Some(host(&mut mem, 0)));
    assert_eq!(
        mmu.next_present(to + PAGE_SIZE, to + 3 * PAGE_SIZE),
        to + 2 * PAGE_SIZE
    );
    assert!(mmu.translate(to + 2 * PAGE_SIZE, 0).is_none());
    // Two overlapping ranges are not allowed
    assert!(mmu.move_entries(to, to + PAGE_SIZE, 2 * PAGE_SIZE).is_err());
}

#[test]
fn superpages_map_protect_unmap_demote_and_move() {
    // Allocate 2 MiB + 4 extra pages so we can test non-2 MiB-aligned host addresses and CoW
    // single-page replacements.
    let mut mem = pages(512 + 4);
    let mut mmu = Mmu::new();
    let base = 0x2000_0000u64; // 2 MiB-aligned
    assert_eq!(base % LEAF, 0);

    // 1. Full 2 MiB fill with 4 KiB-aligned (not 2 MiB-aligned) host pointer -> L2 superpage.
    let h0 = host(&mut mem, 1);
    unsafe {
        mmu.fill_shared(base, h0, LEAF, RW).unwrap();
    }
    assert_eq!(mmu.translate(base, RW), Some(h0));
    assert_eq!(
        mmu.translate(base + 123 * PAGE_SIZE + 0x456, RW),
        Some(NonNull::new(unsafe { h0.as_ptr().add(123 * 4096 + 0x456) }).unwrap())
    );
    assert_eq!(
        mmu.translate(base + 511 * PAGE_SIZE, RW),
        Some(NonNull::new(unsafe { h0.as_ptr().add(511 * 4096) }).unwrap())
    );
    assert_eq!(mmu.next_present(0, base + 2 * LEAF), base);
    assert_eq!(
        mmu.next_present(base + 77 * PAGE_SIZE, base + LEAF),
        base + 77 * PAGE_SIZE
    );
    assert_eq!(
        mmu.next_present(base + LEAF, base + 2 * LEAF),
        base + 2 * LEAF
    );

    // 2. fill_shared over an existing superpage (full or partial) leaves existing mapping intact.
    let alt = host(&mut mem, 513);
    unsafe {
        mmu.fill_shared(base, alt, PAGE_SIZE, prot::READ).unwrap();
        mmu.fill_shared(base, h0, LEAF, prot::READ).unwrap();
    }
    assert_eq!(mmu.translate(base, RW), Some(h0));

    // 3. Whole-2 MiB protect RW -> PROT_NONE -> READ -> RW stays consistent.
    mmu.protect(base, LEAF, 0).unwrap();
    assert!(mmu.translate(base, 0).is_none());
    assert!(mmu.translate(base + 200 * PAGE_SIZE, 0).is_none());
    assert_eq!(
        mmu.next_present(base + 200 * PAGE_SIZE, base + LEAF),
        base + 200 * PAGE_SIZE
    );
    mmu.protect(base, LEAF, prot::READ).unwrap();
    assert!(mmu.translate(base, prot::WRITE).is_none());
    assert_eq!(
        mmu.translate(base + 200 * PAGE_SIZE, prot::READ),
        Some(host(&mut mem, 201))
    );
    mmu.protect(base, LEAF, RW).unwrap();

    // 4. Whole-2 MiB aligned move preserves superpage and clears source.
    let dst = 0x2040_0000u64;
    mmu.move_entries(base, dst, LEAF).unwrap();
    assert_eq!(mmu.next_present(base, base + LEAF), base + LEAF);
    assert_eq!(
        mmu.translate(dst + 500 * PAGE_SIZE, RW),
        Some(host(&mut mem, 501))
    );

    // 5. Partial protect demotes superpage: only protected slice changes.
    mmu.protect(dst + 10 * PAGE_SIZE, 2 * PAGE_SIZE, prot::READ)
        .unwrap();
    assert!(mmu.translate(dst + 9 * PAGE_SIZE, prot::WRITE).is_some());
    assert!(mmu.translate(dst + 10 * PAGE_SIZE, prot::WRITE).is_none());
    assert!(mmu.translate(dst + 10 * PAGE_SIZE, prot::READ).is_some());
    assert!(mmu.translate(dst + 11 * PAGE_SIZE, prot::WRITE).is_none());
    assert!(mmu.translate(dst + 12 * PAGE_SIZE, prot::WRITE).is_some());

    // 6. Full-2 MiB unmap on a fresh superpage clears everything cleanly.
    unsafe {
        mmu.map(base, h0, LEAF, RW).unwrap();
    }
    mmu.unmap(base, LEAF).unwrap();
    assert_eq!(mmu.next_present(base, base + LEAF), base + LEAF);

    // 7. Partial unmap & single-page CoW map_shared demote superpage cleanly.
    unsafe {
        mmu.map(base, h0, LEAF, RW).unwrap();
    }
    mmu.unmap(base + 5 * PAGE_SIZE, 3 * PAGE_SIZE).unwrap();
    assert_eq!(
        mmu.translate(base + 4 * PAGE_SIZE, RW),
        Some(host(&mut mem, 5))
    );
    assert!(mmu.translate(base + 5 * PAGE_SIZE, 0).is_none());
    assert!(mmu.translate(base + 7 * PAGE_SIZE, 0).is_none());
    assert_eq!(
        mmu.next_present(base + 5 * PAGE_SIZE, base + LEAF),
        base + 8 * PAGE_SIZE
    );
    assert_eq!(
        mmu.translate(base + 8 * PAGE_SIZE, RW),
        Some(host(&mut mem, 9))
    );

    // CoW single-page replacement inside a fresh superpage.
    let base2 = 0x2080_0000u64;
    unsafe {
        mmu.map(base2, h0, LEAF, RW).unwrap();
        mmu.map_shared(base2 + 300 * PAGE_SIZE, alt, PAGE_SIZE, prot::READ)
            .unwrap();
    }
    assert_eq!(
        mmu.translate(base2 + 299 * PAGE_SIZE, RW),
        Some(host(&mut mem, 300))
    );
    assert_eq!(
        mmu.translate(base2 + 300 * PAGE_SIZE, prot::READ),
        Some(alt)
    );
    assert!(
        mmu.translate(base2 + 300 * PAGE_SIZE, prot::WRITE)
            .is_none()
    );
    assert_eq!(
        mmu.translate(base2 + 301 * PAGE_SIZE, RW),
        Some(host(&mut mem, 302))
    );

    // 8. Unaligned move from a fresh superpage demotes and carries all 512 pages.
    let base3 = 0x20c0_0000u64;
    let unaligned_dst = 0x2100_1000u64;
    unsafe {
        mmu.map(base3, h0, LEAF, RW).unwrap();
    }
    mmu.move_entries(base3, unaligned_dst, LEAF).unwrap();
    assert_eq!(mmu.next_present(base3, base3 + LEAF), base3 + LEAF);
    assert_eq!(mmu.translate(unaligned_dst, RW), Some(h0));
    assert_eq!(
        mmu.translate(unaligned_dst + 511 * PAGE_SIZE, RW),
        Some(host(&mut mem, 512))
    );

    // 9. A small mapping followed by unmap leaves an empty L3 table; a subsequent full-2 MiB fill
    //    at the same 2 MiB VA must still promote cleanly to a superpage (retiring the empty L3).
    let base4 = 0x2140_0000u64;
    unsafe {
        mmu.map(base4, alt, PAGE_SIZE, RW).unwrap();
    }
    mmu.unmap(base4, PAGE_SIZE).unwrap();
    unsafe {
        mmu.fill_shared(base4, h0, LEAF, RW).unwrap();
    }
    assert_eq!(mmu.translate(base4, RW), Some(h0));
    assert_eq!(
        mmu.translate(base4 + 511 * PAGE_SIZE, RW),
        Some(host(&mut mem, 512))
    );
    // Partial protect after promotion must demote cleanly again without touching the retired L3.
    mmu.protect(base4 + PAGE_SIZE, PAGE_SIZE, prot::READ)
        .unwrap();
    assert!(mmu.translate(base4, prot::WRITE).is_some());
    assert!(mmu.translate(base4 + PAGE_SIZE, prot::WRITE).is_none());
    assert!(mmu.translate(base4 + 2 * PAGE_SIZE, prot::WRITE).is_some());
}

#[test]
fn prepare_unmap_demotes_only_partial_boundaries_and_preserves_watches() {
    let p = PAGE_SIZE;
    for (span, lo, hi, tables) in [
        (LEAF, 0, LEAF, 0),
        (LEAF, 0, p, 1),
        (LEAF, LEAF - p, LEAF, 1),
        (LEAF, p, 3 * p, 1),
        (3 * LEAF, p, 3 * LEAF - p, 2),
        (2 * LEAF, 0, LEAF + p, 1),
    ] {
        let mut mem = pages((span / p) as usize);
        let mut mmu = Mmu::new();
        let base = LEAF;
        unsafe { mmu.map(base, host(&mut mem, 0), span, RW).unwrap() };
        let before = mmu.table_bytes();
        let watches: Vec<_> = (0..mem.len())
            .map(|i| mmu.watch_readable_page(base + i as u64 * p).unwrap())
            .collect();
        mmu.prepare_unmap(base + lo, hi - lo).unwrap();
        assert_eq!(mmu.table_bytes(), before + tables * p);
        for (i, watch) in watches.iter().enumerate() {
            let va = base + i as u64 * p;
            assert!(watch.is_current());
            assert_eq!(mmu.translate(va, RW), Some(host(&mut mem, i)));
            assert!(mmu.translate(va, prot::EXEC).is_none());
        }
        mmu.unmap(base + lo, hi - lo).unwrap();
        assert_eq!(mmu.table_bytes(), before + tables * p);
        for (i, watch) in watches.iter().enumerate() {
            let offset = i as u64 * p;
            let va = base + offset;
            let kept = !(lo..hi).contains(&offset);
            assert_eq!(watch.is_current(), kept);
            assert_eq!(mmu.next_present(va, va + p) == va, kept);
            assert_eq!(mmu.translate(va, RW), kept.then(|| host(&mut mem, i)));
        }
    }
}

#[test]
fn prepare_unmap_validates_the_whole_range_before_demotion() {
    let mut mem = pages(512);
    let mut mmu = Mmu::new();
    unsafe { mmu.map(0, host(&mut mem, 0), LEAF, RW).unwrap() };
    let watch = mmu.watch_readable_page(PAGE_SIZE).unwrap();
    let before = mmu.table_bytes();
    let limit = 1 << VA_BITS;
    for (addr, len) in [
        (1, PAGE_SIZE),
        (0, LEAF - 1),
        (PAGE_SIZE, limit),
        (limit + PAGE_SIZE, 0),
        (PAGE_SIZE, u64::MAX - PAGE_SIZE + 1),
    ] {
        assert_eq!(mmu.prepare_unmap(addr, len), Err(MapError::InvalidArgument));
        assert_eq!(mmu.unmap(addr, len), Err(MapError::InvalidArgument));
        assert_eq!(mmu.unmap_noalloc(addr, len), Err(MapError::InvalidArgument));
    }
    mmu.prepare_unmap(PAGE_SIZE, 0).unwrap();
    mmu.prepare_unmap(limit, 0).unwrap();
    assert_eq!(mmu.unmap_noalloc(PAGE_SIZE, 0), Ok(PAGE_SIZE..PAGE_SIZE));
    assert_eq!(mmu.unmap_noalloc(limit, 0), Ok(limit..limit));
    assert_eq!(mmu.table_bytes(), before);
    assert!(watch.is_current());
    assert_eq!(mmu.translate(PAGE_SIZE, RW), Some(host(&mut mem, 1)));
}

#[test]
fn unmap_noalloc_widens_only_partial_superpages_without_allocating() {
    let p = PAGE_SIZE;
    let mut mem = pages(3 * 512);
    let mut mmu = Mmu::new();
    let base = LEAF;
    unsafe { mmu.map(base, host(&mut mem, 0), 3 * LEAF, RW).unwrap() };

    // Demote only the first superpage into an L3 table; superpages 1 and 2 remain L2 superpages.
    mmu.prepare_unmap(base, p).unwrap();
    let before = mmu.table_bytes();

    let kept_l3 = mmu.watch_readable_page(base).unwrap();
    let dropped_l3 = mmu.watch_readable_page(base + LEAF - p).unwrap();
    let mid_super = mmu.watch_readable_page(base + LEAF).unwrap();
    let tail_super_dropped = mmu.watch_readable_page(base + 2 * LEAF).unwrap();
    let tail_super_widened = mmu.watch_readable_page(base + 3 * LEAF - p).unwrap();

    // Unmap [base + LEAF - p, base + 2 * LEAF + p):
    // - first boundary is an L3 table -> must NOT widen below base + LEAF - p
    // - middle slot is a full L2 superpage -> cleared whole
    // - last boundary is a partial L2 superpage -> cleared whole, widening end to base + 3 * LEAF
    let actual = mmu.unmap_noalloc(base + LEAF - p, LEAF + 2 * p).unwrap();
    assert_eq!(actual, (base + LEAF - p)..(base + 3 * LEAF));
    assert_eq!(mmu.table_bytes(), before);

    assert!(kept_l3.is_current());
    assert_eq!(mmu.translate(base, RW), Some(host(&mut mem, 0)));
    assert_eq!(
        mmu.translate(base + LEAF - 2 * p, RW),
        Some(host(&mut mem, 510))
    );
    assert!(!dropped_l3.is_current());
    assert!(!mid_super.is_current());
    assert!(!tail_super_dropped.is_current());
    assert!(!tail_super_widened.is_current());
    assert_eq!(
        mmu.next_present(base + LEAF - p, base + 3 * LEAF),
        base + 3 * LEAF
    );

    // An interior slice of a superpage widens both boundaries without allocating.
    unsafe { mmu.map(base + LEAF, host(&mut mem, 0), LEAF, RW).unwrap() };
    assert_eq!(
        mmu.unmap_noalloc(base + LEAF + p, p),
        Ok((base + LEAF)..(base + 2 * LEAF))
    );
    assert_eq!(mmu.table_bytes(), before);
    assert_eq!(
        mmu.next_present(base + LEAF, base + 2 * LEAF),
        base + 2 * LEAF
    );
}

#[test]
fn table_bytes_tracks_allocated_tables_and_superpages() {
    let mut mem = pages(512 + 2);
    let mut mmu = Mmu::new();
    // Fresh Mmu has only the L0 root table (512 * sizeof(uintptr_t) = 4 KiB on 64-bit).
    let table_size = 512 * std::mem::size_of::<usize>() as u64;
    assert_eq!(mmu.table_bytes(), table_size);
    assert_eq!(mmu.table_pages(), table_size / PAGE_SIZE);

    // Aligned 2 MiB superpage allocates L1 + L2, without an L3 leaf table.
    let super_va = 0x2000_0000u64;
    unsafe {
        mmu.fill_shared(super_va, host(&mut mem, 0), LEAF, RW)
            .unwrap();
    }
    assert_eq!(mmu.table_bytes(), 3 * table_size);

    // Partial protect demotes the superpage into a newly allocated L3 table.
    mmu.protect(super_va + PAGE_SIZE, PAGE_SIZE, prot::READ)
        .unwrap();
    assert_eq!(mmu.table_bytes(), 4 * table_size);

    // Mapping a single 4 KiB page in a fresh 2 MiB slot under the same L2 allocates one L3 table.
    let small_va = super_va + LEAF;
    unsafe {
        mmu.map(small_va, host(&mut mem, 512), PAGE_SIZE, RW)
            .unwrap();
    }
    assert_eq!(mmu.table_bytes(), 5 * table_size);

    // Unmapping and re-promoting retires the empty L3 (still allocated until Mmu::drop), so
    // table_bytes stays monotonic, and a later demotion allocates a fresh L3 table.
    mmu.unmap(small_va, PAGE_SIZE).unwrap();
    unsafe {
        mmu.fill_shared(small_va, host(&mut mem, 0), LEAF, RW)
            .unwrap();
    }
    assert_eq!(mmu.table_bytes(), 5 * table_size);
    mmu.unmap(small_va, PAGE_SIZE).unwrap();
    assert_eq!(mmu.table_bytes(), 6 * table_size);
}

fn pages(n: usize) -> Box<[Page]> {
    (0..n).map(|_| Page([0; 4096])).collect()
}

fn host(p: &mut [Page], i: usize) -> NonNull<u8> {
    NonNull::new(p[i..].as_mut_ptr().cast::<u8>()).unwrap()
}
