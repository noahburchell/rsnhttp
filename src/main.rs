mod bufring;
mod http;
mod server;

use std::ffi::{CStr, CString, OsString};
use std::fmt::Display;
use std::io;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::process::exit;
use std::thread;

const PROG: &str = "rsnhttp";
const USER: &CStr = c"nhttp";
const PORT: u16 = 80;
const BACKLOG: i32 = 4096;

const USAGE: &str = "\
usage: rsnhttp DIR

      --help display this help and exit

DIR is the document root
rsnhttp starts as root, listens on port 80, changes root to DIR and runs as user nhttp
only GET and HEAD are served
";

fn strerror(errno: i32) -> String {
    unsafe { CStr::from_ptr(libc::strerror(errno)) }
        .to_string_lossy()
        .into_owned()
}

fn fail(context: impl Display) -> ! {
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    eprintln!("{PROG}: {context}: {}", strerror(errno));
    exit(1);
}

fn usage_error(message: impl Display) -> ! {
    eprintln!("{PROG}: {message}");
    eprintln!("try '{PROG} --help' for more information");
    exit(2);
}

fn operand() -> OsString {
    let mut args = std::env::args_os().skip(1);
    let Some(dir) = args.next() else {
        usage_error("missing operand");
    };

    if dir == "--help" {
        print!("{USAGE}");
        exit(0);
    }

    if dir.as_bytes().starts_with(b"-") {
        usage_error(format_args!("unknown option '{}'", dir.display()));
    }

    if let Some(extra) = args.next() {
        usage_error(format_args!("extra operand '{}'", extra.display()));
    }

    dir
}

fn cpus() -> Vec<usize> {
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } < 0 {
        fail("sched_getaffinity");
    }

    (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
        .collect()
}

fn pin(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
    }
}

fn set_option(fd: i32, level: i32, name: i32, value: i32) {
    if unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&raw const value).cast(),
            size_of::<i32>() as u32,
        )
    } < 0
    {
        fail("setsockopt");
    }
}

fn bind_any(fd: i32, family: i32) -> i32 {
    let mut v6: libc::sockaddr_in6 = unsafe { mem::zeroed() };
    let mut v4: libc::sockaddr_in = unsafe { mem::zeroed() };
    v6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    v6.sin6_port = PORT.to_be();
    v4.sin_family = libc::AF_INET as libc::sa_family_t;
    v4.sin_port = PORT.to_be();
    if family == libc::AF_INET6 {
        return unsafe {
            libc::bind(
                fd,
                (&raw const v6).cast(),
                size_of::<libc::sockaddr_in6>() as u32,
            )
        };
    }

    unsafe {
        libc::bind(
            fd,
            (&raw const v4).cast(),
            size_of::<libc::sockaddr_in>() as u32,
        )
    }
}

// one listener per thread, the kernel spreads connections over them
// a kernel without ipv6 gets an ipv4 listener, otherwise one socket serves both
fn listener() -> i32 {
    let mut family = libc::AF_INET6;
    let mut fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EAFNOSUPPORT) {
        family = libc::AF_INET;
        fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    }

    if fd < 0 {
        fail("socket");
    }

    set_option(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1);
    set_option(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1);
    set_option(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
    if family == libc::AF_INET6 {
        set_option(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 0);
    }

    if bind_any(fd, family) < 0 {
        fail(format_args!("bind port {PORT}"));
    }

    if unsafe { libc::listen(fd, BACKLOG) } < 0 {
        fail("listen");
    }

    fd
}

fn account() -> (libc::uid_t, libc::gid_t) {
    let pw = unsafe { libc::getpwnam(USER.as_ptr()) };
    if pw.is_null() {
        eprintln!("{PROG}: user {} does not exist", USER.to_string_lossy());
        exit(1);
    }

    unsafe { ((*pw).pw_uid, (*pw).pw_gid) }
}

fn raise_nofile() {
    unsafe {
        let mut lim: libc::rlimit = mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) < 0 {
            return;
        }

        lim.rlim_cur = lim.rlim_max;
        libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
    }
}

// returns the descriptor of the document root, which is / from here on
fn confine(dir: &OsString, uid: libc::uid_t, gid: libc::gid_t) -> i32 {
    let Ok(path) = CString::new(dir.as_bytes()) else {
        usage_error(format_args!("invalid operand '{}'", dir.display()));
    };

    unsafe {
        if libc::chroot(path.as_ptr()) < 0 {
            fail(format_args!("chroot {}", dir.display()));
        }

        if libc::chdir(c"/".as_ptr()) < 0 {
            fail("chdir /");
        }

        if libc::setgroups(1, &gid) < 0 {
            fail("setgroups");
        }

        if libc::setgid(gid) < 0 {
            fail("setgid");
        }

        if libc::setuid(uid) < 0 {
            fail("setuid");
        }

        let root = libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        );
        if root < 0 {
            fail("open /");
        }

        root
    }
}

fn worker(cpu: usize, listener: i32, root: i32) {
    pin(cpu);
    if let Err((call, e)) = server::serve(listener, root) {
        eprintln!(
            "{PROG}: {call}: {}",
            strerror(e.raw_os_error().unwrap_or(0))
        );
        exit(1);
    }
}

fn main() {
    let dir = operand();
    let cpus = cpus();
    let listeners: Vec<i32> = cpus.iter().map(|_| listener()).collect();
    let (uid, gid) = account();
    raise_nofile();
    let root = confine(&dir, uid, gid);
    let threads: Vec<_> = cpus
        .into_iter()
        .zip(listeners)
        .map(|(cpu, fd)| thread::spawn(move || worker(cpu, fd, root)))
        .collect();
    for t in threads {
        let _ = t.join();
    }
}
