use std::io;
use std::ptr;
use std::sync::atomic::{AtomicU16, Ordering};

use io_uring::Submitter;
use io_uring::types::BufRingEntry;

pub const GROUP: u16 = 0;
pub const BUF_LEN: usize = 4096;
const ENTRIES: u16 = 256;

pub struct BufRing {
    ring: *mut BufRingEntry,
    bufs: *mut u8,
    tail: u16,
}

fn map(len: usize) -> io::Result<*mut u8> {
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    Ok(p.cast())
}

impl BufRing {
    pub fn new(submitter: &Submitter) -> io::Result<Self> {
        let ring = map(ENTRIES as usize * size_of::<BufRingEntry>())?.cast();
        let bufs = map(ENTRIES as usize * BUF_LEN)?;
        unsafe { submitter.register_buf_ring_with_flags(ring as u64, ENTRIES, GROUP, 0)? };
        let mut r = BufRing {
            ring,
            bufs,
            tail: 0,
        };
        for bid in 0..ENTRIES {
            r.put(bid);
        }

        Ok(r)
    }

    pub fn get(&self, bid: u16) -> *const u8 {
        unsafe { self.bufs.add(bid as usize * BUF_LEN) }
    }

    pub fn put(&mut self, bid: u16) {
        unsafe {
            let e = &mut *self.ring.add((self.tail & (ENTRIES - 1)) as usize);
            e.set_addr(self.get(bid) as u64);
            e.set_len(BUF_LEN as u32);
            e.set_bid(bid);
            self.tail = self.tail.wrapping_add(1);
            (*BufRingEntry::tail(self.ring).cast::<AtomicU16>())
                .store(self.tail, Ordering::Release);
        }
    }
}
