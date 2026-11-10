# rsnhttp

static file server for linux, built on io_uring
serves GET and HEAD from one directory over http/1.1 on port 80 and nothing else

## build

```
cargo build --release
```

the binary is `target/release/rsnhttp`

## requirements

- linux 6.4 or later (multishot timeout, DEFER_TASKRUN, ring mapped buffers, openat2)
- started as root
- a user named `nhttp`
- io_uring not disabled by `kernel.io_uring_disabled`

## usage

```
usage: rsnhttp DIR

      --help display this help and exit

DIR is the document root
rsnhttp starts as root, listens on port 80, changes root to DIR and runs as user nhttp
only GET and HEAD are served
```

exit status is 1 on runtime failure and 2 on usage error
there is no configuration file, no log and no signal handling, SIGTERM terminates the process

## startup

1. one listening socket per cpu in the affinity mask, each with SO_REUSEPORT
2. the socket is ipv6 with IPV6_V6ONLY off and serves ipv4 as well, a kernel without ipv6 gets an ipv4 socket
3. RLIMIT_NOFILE is raised to `fs.nr_open`, or to the hard limit when that fails
4. chroot to DIR, then setgroups, setgid and setuid to `nhttp`
5. one thread per cpu, pinned to that cpu, each with its own ring and listener

threads share nothing but the descriptor of the document root

## requests

| request                          | response                              |
|----------------------------------|---------------------------------------|
| GET or HEAD of a regular file    | 200                                   |
| valid `Range`                    | 206                                   |
| `Range` outside the file         | 416                                   |
| `If-Modified-Since` equal to the modification time | 304                 |
| directory without trailing slash | 301 to the path with a trailing slash |
| path ending in a slash           | `index.html` in that directory        |
| missing file, path leaving the document root | 404                       |
| unreadable file, device, fifo, socket | 403                              |
| malformed request line or path   | 400, connection closed                |
| any other method                 | 405, connection closed                |
| head larger than 8192 bytes      | 431, connection closed                |
| version other than 1.0 or 1.1    | 505, connection closed                |
| out of descriptors or memory     | 503, connection closed                |
| other failure                    | 500, connection closed                |

details:

- the request target is cut at `?` or `#`, percent decoded and opened relative to the
  document root with RESOLVE_BENEATH, `..` above the root and absolute symbolic links give 404
- `%00` and invalid percent escapes give 400
- one byte range per request: `bytes=A-B`, `bytes=A-` and `bytes=-N`
  a request with several ranges or a malformed range gets the whole file
- `If-Range` accepts a date, the range applies when the date equals the modification time
  an entity tag never matches
- `If-Modified-Since` matches on equality with `Last-Modified`, not on order
- http/1.1 connections persist unless the request carries `Connection: close`
  http/1.0 connections close unless the request carries `Connection: keep-alive`
- pipelined requests are served in order
- a request body is not read, its bytes are parsed as the next request
- `Content-Type` follows the file extension, unknown extensions are `application/octet-stream`
- 200, 206, 301, 304 and 416 carry `Date`, the fixed error responses do not
- a connection with no completed operation for 30 s is shut down

## limits

| constant      | file       | value        | meaning                                        |
|---------------|------------|--------------|------------------------------------------------|
| `PORT`        | main.rs    | 80           | listening port                                 |
| `USER`        | main.rs    | nhttp        | account the server runs as                     |
| `BACKLOG`     | main.rs    | 4096         | listen backlog, capped by `net.core.somaxconn` |
| `MAX_HEAD`    | http.rs    | 8192 bytes   | largest request head                           |
| `SQ_ENTRIES`  | server.rs  | 4096         | submission queue size, completion queue is 4 times that |
| `BUF`         | server.rs  | 65536 bytes  | response buffer of a request in flight         |
| `IDLE_TICKS`  | server.rs  | 30 s         | idle timeout                                   |
| `ENTRIES`     | bufring.rs | 1024         | receive buffers per thread                     |
| `BUF_LEN`     | bufring.rs | 4096 bytes   | size of a receive buffer                       |

## source layout

- `main.rs`: arguments, listeners, privilege drop, thread start
- `server.rs`: the event loop and the connection state machine
- `http.rs`: request parsing, path decoding, response heads, dates
- `bufring.rs`: the ring of receive buffers shared with the kernel

## event loop

each thread runs one ring with SINGLE_ISSUER and DEFER_TASKRUN
the loop submits, waits for one completion, then handles every completion in the queue
a submission that does not fit the queue waits in a backlog and goes out on the next turn

two operations are multishot and live for the whole run: accept on the listener and a 1 s timeout
the timeout refreshes the `Date` value, shuts down idle connections and restarts accept
after it stopped for lack of descriptors or memory

a connection has exactly one operation in flight, named by its state:

```
Recv -> Open -> Send                 head only, error, 301, 304, 416
Recv -> Open -> Read -> Send -> Read -> Send ...   file body
```

- `Recv`: one receive into a buffer the kernel picks from the ring
  a head that arrives whole is parsed in place, a partial head is copied to `pending`
- `Open`: openat2 on the decoded path, on completion fstat runs inline and the response head is built
- `Read`: up to 65536 bytes of the file behind the head, later reads fill the whole buffer
- `Send`: the buffer, with MSG_WAITALL

after the last send the file is closed, the buffer returns to the pool and the connection
receives again or serves the next pipelined request

## memory

- an idle connection costs one `Conn`, one 8 byte `Slot`, one descriptor and one pending receive
- a request in flight adds one 65536 byte buffer and one file descriptor
  buffers are pooled per thread and never freed, the pool is as large as the peak of requests in flight
- each thread maps 1024 receive buffers of 4096 bytes, about 4 MiB once all are touched

## tuning

the number of connections is bounded by descriptors and kernel socket memory:

- `fs.nr_open`: per process descriptor limit, rsnhttp raises RLIMIT_NOFILE to it
- `fs.file-max`: system wide limit on open files
- `net.core.somaxconn`, `net.ipv4.tcp_max_syn_backlog`: accept queue under connection bursts
- `net.ipv4.tcp_mem`: total tcp memory

## limitations

- a fifo in the document root blocks its open until a writer appears, the idle timeout does not cancel it
- no tls, no compression, no directory listing, no entity tags, no access log
- a file that shrinks during a response closes the connection
