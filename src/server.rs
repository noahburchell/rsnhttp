use std::io;
use std::mem;
use std::slice;

use io_uring::{
    CompletionQueue, IoUring, SubmissionQueue, Submitter, cqueue, opcode, squeue, types,
};

use crate::bufring::{self, BufRing};
use crate::http::{self, Meta, Range, Stamp};

const SQ_ENTRIES: u32 = 4096;
const BUF: usize = 65536;
// the request path sits at the tail of a buffer, the response is built at its head
const PATH_AT: usize = BUF - http::MAX_HEAD - 256;
const IDLE_TICKS: u32 = 30;
const NONE: u32 = u32::MAX;

const ACCEPT: u64 = u64::MAX;
const TICK: u64 = u64::MAX - 1;
const IGNORE: u64 = u64::MAX - 2;

static HOW: types::OpenHow = types::OpenHow::new()
    .flags((libc::O_RDONLY | libc::O_CLOEXEC) as u64)
    .resolve(libc::RESOLVE_BENEATH);
static SECOND: types::Timespec = types::Timespec::new().sec(1);

pub type Error = (&'static str, io::Error);

#[derive(Clone, Copy)]
enum State {
    Recv,
    Open,
    Stat,
    Read,
    Send,
}

// exactly one operation is in flight per connection, state names it
struct Conn {
    fd: i32,
    file: i32,
    buf: u32,
    last: u32,
    state: State,
    head: bool,
    keep: bool,
    send_ptr: *const u8,
    send_len: u32,
    fill: u32,
    path_len: u32,
    remaining: u64,
    offset: u64,
    ctype: &'static str,
    range: Range,
    since: Stamp,
    if_range: Stamp,
    pending: Vec<u8>,
    stx: libc::statx,
}

impl Conn {
    fn new(fd: i32, tick: u32) -> Conn {
        Conn {
            fd,
            file: -1,
            buf: NONE,
            last: tick,
            state: State::Recv,
            head: false,
            keep: false,
            send_ptr: std::ptr::null(),
            send_len: 0,
            fill: 0,
            path_len: 0,
            remaining: 0,
            offset: 0,
            ctype: "",
            range: Range::None,
            since: [0; http::STAMP_LEN],
            if_range: [0; http::STAMP_LEN],
            pending: Vec::new(),
            stx: unsafe { mem::zeroed() },
        }
    }
}

struct Server<'r> {
    submitter: Submitter<'r>,
    sq: SubmissionQueue<'r>,
    backlog: Vec<squeue::Entry>,
    recv: BufRing,
    // boxed so that the statx buffer keeps its address when the vector grows
    #[allow(clippy::vec_box)]
    conns: Vec<Box<Conn>>,
    free: Vec<u32>,
    bufs: Vec<Box<[u8]>>,
    free_bufs: Vec<u32>,
    listener: i32,
    root: i32,
    tick: u32,
    accepting: bool,
    date: Stamp,
}

fn now() -> Stamp {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };

    http::stamp(ts.tv_sec)
}

fn open_error(errno: i32) -> (&'static [u8], bool) {
    match errno {
        libc::ENOENT | libc::ENOTDIR | libc::EXDEV | libc::ELOOP | libc::ENAMETOOLONG => {
            (http::R404, true)
        }
        libc::EACCES | libc::EPERM | libc::ENXIO => (http::R403, true),
        libc::EMFILE | libc::ENFILE | libc::ENOMEM => (http::R503, false),
        _ => (http::R500, false),
    }
}

pub fn serve(listener: i32, root: i32) -> Result<(), Error> {
    let mut ring: IoUring = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .setup_cqsize(SQ_ENTRIES * 4)
        .build(SQ_ENTRIES)
        .map_err(|e| ("io_uring_setup", e))?;
    let (submitter, sq, cq) = ring.split();
    let recv = BufRing::new(&submitter).map_err(|e| ("io_uring_register", e))?;
    let mut server = Server {
        submitter,
        sq,
        backlog: Vec::new(),
        recv,
        conns: Vec::new(),
        free: Vec::new(),
        bufs: Vec::new(),
        free_bufs: Vec::new(),
        listener,
        root,
        tick: 0,
        accepting: true,
        date: now(),
    };

    server.arm_accept();
    server.arm_tick();
    server.run(cq).map_err(|e| ("io_uring_enter", e))
}

impl Server<'_> {
    fn run(&mut self, mut cq: CompletionQueue<'_>) -> io::Result<()> {
        loop {
            self.flush_backlog();
            match self.submitter.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EBUSY)) => {}
                Err(e) => return Err(e),
            }

            cq.sync();
            for cqe in cq.by_ref() {
                self.complete(cqe.user_data(), cqe.result(), cqe.flags());
            }
        }
    }

    fn flush_backlog(&mut self) {
        loop {
            let room = self.sq.capacity() - self.sq.len();
            let n = room.min(self.backlog.len());
            for e in self.backlog.drain(..n) {
                let _ = unsafe { self.sq.push(&e) };
            }

            self.sq.sync();
            if self.backlog.is_empty() || self.submitter.submit().is_err() {
                return;
            }

            self.sq.sync();
            if self.sq.is_full() {
                return;
            }
        }
    }

    fn push(&mut self, e: squeue::Entry) {
        // entries point into conns, bufs and statics, all of which outlive the completion
        if unsafe { self.sq.push(&e) }.is_err() {
            self.backlog.push(e);
        }
    }

    fn arm_accept(&mut self) {
        self.push(
            opcode::AcceptMulti::new(types::Fd(self.listener))
                .build()
                .user_data(ACCEPT),
        );
    }

    fn arm_tick(&mut self) {
        self.push(
            opcode::Timeout::new(&SECOND)
                .flags(types::TimeoutFlags::MULTISHOT)
                .build()
                .user_data(TICK),
        );
    }

    fn arm_recv(&mut self, idx: usize) {
        let c = &mut *self.conns[idx];
        c.state = State::Recv;
        let e = opcode::Recv::new(types::Fd(c.fd), std::ptr::null_mut(), 0)
            .buf_group(bufring::GROUP)
            .build()
            .flags(squeue::Flags::BUFFER_SELECT)
            .user_data(idx as u64);
        self.push(e);
    }

    fn complete(&mut self, user_data: u64, res: i32, flags: u32) {
        match user_data {
            IGNORE => {}
            ACCEPT => self.on_accept(res, flags),
            TICK => self.on_tick(flags),
            _ => self.on_conn(user_data as usize, res, flags),
        }
    }

    fn on_accept(&mut self, res: i32, flags: u32) {
        if res >= 0 {
            self.open_conn(res);
        }

        if cqueue::more(flags) {
            return;
        }

        // out of descriptors or memory, the next tick accepts again
        if matches!(
            -res,
            libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM
        ) {
            self.accepting = false;
            return;
        }

        self.arm_accept();
    }

    fn open_conn(&mut self, fd: i32) {
        let conn = Conn::new(fd, self.tick);
        let idx = match self.free.pop() {
            Some(i) => {
                *self.conns[i as usize] = conn;
                i as usize
            }
            None => {
                self.conns.push(Box::new(conn));
                self.conns.len() - 1
            }
        };

        self.arm_recv(idx);
    }

    fn on_tick(&mut self, flags: u32) {
        self.tick = self.tick.wrapping_add(1);
        self.date = now();
        if !self.accepting {
            self.accepting = true;
            self.arm_accept();
        }

        // shutdown completes the pending operation with an error, which closes the connection
        for c in &self.conns {
            if c.fd >= 0 && self.tick.wrapping_sub(c.last) > IDLE_TICKS {
                unsafe { libc::shutdown(c.fd, libc::SHUT_RDWR) };
            }
        }

        if !cqueue::more(flags) {
            self.arm_tick();
        }
    }

    fn on_conn(&mut self, idx: usize, res: i32, flags: u32) {
        let c = &mut *self.conns[idx];
        c.last = self.tick;
        match c.state {
            State::Recv => self.on_recv(idx, res, flags),
            State::Open => self.on_open(idx, res),
            State::Stat => self.on_stat(idx, res),
            State::Read => self.on_read(idx, res),
            State::Send => self.on_send(idx, res),
        }
    }

    fn on_recv(&mut self, idx: usize, res: i32, flags: u32) {
        let bid = cqueue::buffer_select(flags);
        if res > 0
            && let Some(bid) = bid
        {
            // the buffer returns to the ring only after the request is consumed
            let data = unsafe { slice::from_raw_parts(self.recv.get(bid), res as usize) };
            self.on_data(idx, data);
            self.recv.put(bid);
            return;
        }

        if let Some(bid) = bid {
            self.recv.put(bid);
        }

        if res == -libc::ENOBUFS {
            return self.arm_recv(idx);
        }

        self.close(idx);
    }

    fn on_data(&mut self, idx: usize, data: &[u8]) {
        let c = &mut *self.conns[idx];
        if !c.pending.is_empty() {
            c.pending.extend_from_slice(data);
            return self.next_request(idx);
        }

        // common case, a whole request in one buffer is parsed in place
        match http::head_len(data) {
            Some(n) => {
                c.pending.extend_from_slice(&data[n..]);
                self.request(idx, &data[..n]);
            }
            None => {
                c.pending.extend_from_slice(data);
                self.arm_recv(idx);
            }
        }
    }

    // serves the next request buffered in pending, or receives more
    fn next_request(&mut self, idx: usize) {
        let pending = mem::take(&mut self.conns[idx].pending);
        if let Some(n) = http::head_len(&pending) {
            self.conns[idx].pending = pending[n..].to_vec();
            return self.request(idx, &pending[..n]);
        }

        if pending.len() >= http::MAX_HEAD {
            return self.reply(idx, http::R431, false);
        }

        self.conns[idx].pending = pending;
        self.arm_recv(idx);
    }

    fn take_buf(&mut self) -> u32 {
        if let Some(i) = self.free_bufs.pop() {
            return i;
        }

        self.bufs.push(vec![0u8; BUF].into_boxed_slice());

        self.bufs.len() as u32 - 1
    }

    fn request(&mut self, idx: usize, head: &[u8]) {
        if head.len() > http::MAX_HEAD {
            return self.reply(idx, http::R431, false);
        }

        let req = match http::parse(head) {
            Ok(req) => req,
            Err(resp) => return self.reply(idx, resp, false),
        };

        let buf = self.take_buf();
        let c = &mut *self.conns[idx];
        c.buf = buf;
        c.head = req.head;
        c.keep = req.keep;
        c.range = req.range;
        c.since = req.since;
        c.if_range = req.if_range;
        let path = &mut self.bufs[buf as usize][PATH_AT..];
        let Some(len) = http::decode_path(req.target, path) else {
            return self.reply(idx, http::R400, false);
        };

        c.path_len = len as u32;
        c.ctype = http::content_type(&path[..len]);
        c.state = State::Open;
        let e = opcode::OpenAt2::new(types::Fd(self.root), path.as_ptr().cast(), &HOW)
            .build()
            .user_data(idx as u64);
        self.push(e);
    }

    fn on_open(&mut self, idx: usize, res: i32) {
        if res < 0 {
            let (resp, keep) = open_error(-res);
            return self.reply(idx, resp, keep);
        }

        let c = &mut *self.conns[idx];
        c.file = res;
        c.state = State::Stat;
        let e = opcode::Statx::new(types::Fd(res), c"".as_ptr(), (&raw mut c.stx).cast())
            .flags(libc::AT_EMPTY_PATH)
            .mask(libc::STATX_TYPE | libc::STATX_SIZE | libc::STATX_MTIME)
            .build()
            .user_data(idx as u64);
        self.push(e);
    }

    fn on_stat(&mut self, idx: usize, res: i32) {
        if res < 0 {
            return self.reply(idx, http::R500, false);
        }

        let c = &mut *self.conns[idx];
        match c.stx.stx_mode as u32 & libc::S_IFMT {
            libc::S_IFREG => {}
            libc::S_IFDIR => return self.redirect(idx),
            _ => return self.reply(idx, http::R403, true),
        }

        let size = c.stx.stx_size;
        let modified = http::stamp(c.stx.stx_mtime.tv_sec);
        let meta = Meta {
            date: &self.date,
            modified: &modified,
            ctype: c.ctype,
            keep: c.keep,
        };
        let out = &mut self.bufs[c.buf as usize][..PATH_AT];
        let range = if c.if_range[0] != 0 && c.if_range != modified {
            Range::None
        } else {
            c.range
        };
        let (n, first, len) = if c.since == modified {
            (http::head_304(out, &meta), 0, 0)
        } else {
            match range.resolve(size) {
                Ok(None) => (http::head_200(out, &meta, size), 0, size),
                Ok(Some((a, b))) => (http::head_206(out, &meta, a, b, size), a, b - a + 1),
                Err(()) => (http::head_416(out, &meta, size), 0, 0),
            }
        };

        c.offset = first;
        c.remaining = if c.head { 0 } else { len };
        if c.remaining == 0 {
            let ptr = out.as_ptr();
            return self.send(idx, ptr, n);
        }

        self.read_file(idx, n);
    }

    fn redirect(&mut self, idx: usize) {
        let c = &mut *self.conns[idx];
        let (out, path) = self.bufs[c.buf as usize].split_at_mut(PATH_AT);
        let n = http::head_301(out, &self.date, &path[..c.path_len as usize], c.keep);
        c.remaining = 0;
        let ptr = out.as_ptr();
        self.send(idx, ptr, n);
    }

    // reads the next part of the file behind fill bytes of response header
    fn read_file(&mut self, idx: usize, fill: usize) {
        let c = &mut *self.conns[idx];
        let len = c.remaining.min((BUF - fill) as u64) as u32;
        c.fill = fill as u32;
        c.state = State::Read;
        let ptr = unsafe { self.bufs[c.buf as usize].as_mut_ptr().add(fill) };
        let e = opcode::Read::new(types::Fd(c.file), ptr, len)
            .offset(c.offset)
            .build()
            .user_data(idx as u64);
        self.push(e);
    }

    fn on_read(&mut self, idx: usize, res: i32) {
        // the promised length cannot be met when the file shrinks, only close is left
        if res <= 0 {
            return self.close(idx);
        }

        let c = &mut *self.conns[idx];
        c.remaining -= res as u64;
        c.offset += res as u64;
        let ptr = self.bufs[c.buf as usize].as_ptr();
        let len = c.fill as usize + res as usize;
        self.send(idx, ptr, len);
    }

    fn send(&mut self, idx: usize, ptr: *const u8, len: usize) {
        let c = &mut *self.conns[idx];
        c.send_ptr = ptr;
        c.send_len = len as u32;
        c.state = State::Send;
        let e = opcode::Send::new(types::Fd(c.fd), ptr, len as u32)
            .flags(libc::MSG_NOSIGNAL | libc::MSG_WAITALL)
            .build()
            .user_data(idx as u64);
        self.push(e);
    }

    fn reply(&mut self, idx: usize, resp: &'static [u8], keep: bool) {
        self.release(idx);
        let c = &mut *self.conns[idx];
        c.keep &= keep;
        c.remaining = 0;
        self.send(idx, resp.as_ptr(), resp.len());
    }

    fn on_send(&mut self, idx: usize, res: i32) {
        if res <= 0 {
            return self.close(idx);
        }

        let c = &mut *self.conns[idx];
        let sent = res as u32;
        if sent < c.send_len {
            let ptr = unsafe { c.send_ptr.add(sent as usize) };
            let len = (c.send_len - sent) as usize;
            return self.send(idx, ptr, len);
        }

        if c.remaining > 0 {
            return self.read_file(idx, 0);
        }

        self.release(idx);
        let c = &mut *self.conns[idx];
        if !c.keep {
            return self.close(idx);
        }

        if c.pending.is_empty() {
            return self.arm_recv(idx);
        }

        self.next_request(idx);
    }

    fn release(&mut self, idx: usize) {
        let c = &mut *self.conns[idx];
        let file = mem::replace(&mut c.file, -1);
        let buf = mem::replace(&mut c.buf, NONE);
        if buf != NONE {
            self.free_bufs.push(buf);
        }

        if file >= 0 {
            self.push(
                opcode::Close::new(types::Fd(file))
                    .build()
                    .user_data(IGNORE),
            );
        }
    }

    fn close(&mut self, idx: usize) {
        self.release(idx);
        let c = &mut *self.conns[idx];
        let fd = mem::replace(&mut c.fd, -1);
        c.pending = Vec::new();
        self.free.push(idx as u32);
        self.push(opcode::Close::new(types::Fd(fd)).build().user_data(IGNORE));
    }
}
