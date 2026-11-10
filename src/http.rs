use memchr::{memchr, memchr_iter, memchr2, memmem, memrchr};

pub const MAX_HEAD: usize = 8192;
pub const STAMP_LEN: usize = 29;

// an imf-fixdate, all zero when absent
pub type Stamp = [u8; STAMP_LEN];

pub const R400: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const R403: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
pub const R404: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
pub const R405: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nAllow: GET, HEAD\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const R431: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const R500: &[u8] =
    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const R503: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
pub const R505: &[u8] =
    b"HTTP/1.1 505 HTTP Version Not Supported\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

#[derive(Clone, Copy)]
pub enum Range {
    None,
    // first and last byte, last is u64::MAX when open ended
    Span(u64, u64),
    Suffix(u64),
}

pub struct Request<'a> {
    pub head: bool,
    pub keep: bool,
    pub target: &'a [u8],
    pub range: Range,
    pub since: Stamp,
    pub if_range: Stamp,
}

pub fn head_len(data: &[u8]) -> Option<usize> {
    memmem::find(data, b"\r\n\r\n").map(|i| i + 4)
}

fn number(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }

    let mut v = 0u64;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }

        v = v.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }

    Some(v)
}

fn parse_range(value: &[u8]) -> Range {
    let Some(spec) = value.strip_prefix(b"bytes=") else {
        return Range::None;
    };

    let Some(dash) = memchr(b'-', spec) else {
        return Range::None;
    };

    let (first, last) = (&spec[..dash], &spec[dash + 1..]);
    if first.is_empty() {
        return number(last).map_or(Range::None, Range::Suffix);
    }

    let Some(a) = number(first) else {
        return Range::None;
    };

    if last.is_empty() {
        return Range::Span(a, u64::MAX);
    }

    match number(last) {
        Some(b) if b >= a => Range::Span(a, b),
        _ => Range::None,
    }
}

impl Range {
    // Ok(None) serves the whole file, Err is 416
    pub fn resolve(self, size: u64) -> Result<Option<(u64, u64)>, ()> {
        match self {
            Range::None => Ok(None),
            Range::Span(a, _) if a >= size => Err(()),
            Range::Span(a, b) => Ok(Some((a, b.min(size - 1)))),
            Range::Suffix(n) if n == 0 || size == 0 => Err(()),
            Range::Suffix(n) => Ok(Some((size.saturating_sub(n), size - 1))),
        }
    }
}

fn stamp_of(value: &[u8]) -> Option<Stamp> {
    value.try_into().ok()
}

fn contains_token(value: &[u8], token: &[u8]) -> bool {
    value
        .windows(token.len())
        .any(|w| w.eq_ignore_ascii_case(token))
}

fn header(req: &mut Request, line: &[u8]) {
    let Some(colon) = memchr(b':', line) else {
        return;
    };

    let name = &line[..colon];
    let value = line[colon + 1..].trim_ascii();
    match name.len() {
        5 if name.eq_ignore_ascii_case(b"range") => req.range = parse_range(value),
        8 if name.eq_ignore_ascii_case(b"if-range") => match stamp_of(value) {
            Some(s) => req.if_range = s,
            // an entity tag never matches, 0xff forces the full response
            None => req.if_range = [0xff; STAMP_LEN],
        },
        10 if name.eq_ignore_ascii_case(b"connection") => {
            if contains_token(value, b"close") {
                req.keep = false;
            } else if contains_token(value, b"keep-alive") {
                req.keep = true;
            }
        }
        17 if name.eq_ignore_ascii_case(b"if-modified-since") => {
            req.since = stamp_of(value).unwrap_or([0; STAMP_LEN])
        }
        _ => {}
    }
}

// head ends with an empty line, Err carries the response to send
pub fn parse(head: &[u8]) -> Result<Request<'_>, &'static [u8]> {
    let eol = memchr(b'\n', head).ok_or(R400)?;
    let line = head[..eol].strip_suffix(b"\r").ok_or(R400)?;
    let sp = memchr(b' ', line).ok_or(R400)?;
    let (method, rest) = (&line[..sp], &line[sp + 1..]);
    let sp = memrchr(b' ', rest).ok_or(R400)?;
    let (target, version) = (&rest[..sp], &rest[sp + 1..]);
    let keep = match version {
        b"HTTP/1.1" => true,
        b"HTTP/1.0" => false,
        v if v.starts_with(b"HTTP/") => return Err(R505),
        _ => return Err(R400),
    };

    let is_head = match method {
        b"GET" => false,
        b"HEAD" => true,
        _ => return Err(R405),
    };

    let mut req = Request {
        head: is_head,
        keep,
        target,
        range: Range::None,
        since: [0; STAMP_LEN],
        if_range: [0; STAMP_LEN],
    };
    let mut start = eol + 1;
    for nl in memchr_iter(b'\n', &head[start..]).map(|i| i + eol + 1) {
        header(&mut req, &head[start..nl]);
        start = nl + 1;
    }

    Ok(req)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// writes the NUL terminated path relative to the document root, returns its length
pub fn decode_path(target: &[u8], out: &mut [u8]) -> Option<usize> {
    let end = memchr2(b'?', b'#', target).unwrap_or(target.len());
    let mut src = &target[..end];
    if src.first() != Some(&b'/') {
        return None;
    }

    while let [b'/', rest @ ..] = src {
        src = rest;
    }

    if src.len() + 11 > out.len() {
        return None;
    }

    let mut n = 0;
    let mut i = 0;
    while i < src.len() {
        let mut b = src[i];
        i += 1;
        if b == b'%' {
            b = hex(*src.get(i)?)? << 4 | hex(*src.get(i + 1)?)?;
            i += 2;
        }

        if b == 0 {
            return None;
        }

        out[n] = b;
        n += 1;
    }

    if n == 0 || out[n - 1] == b'/' {
        out[n..n + 10].copy_from_slice(b"index.html");
        n += 10;
    }

    out[n] = 0;

    Some(n)
}

pub fn content_type(path: &[u8]) -> &'static str {
    let name = &path[memrchr(b'/', path).map_or(0, |i| i + 1)..];
    let Some(dot) = memrchr(b'.', name) else {
        return "application/octet-stream";
    };

    let ext = &name[dot + 1..];
    let mut lower = [0u8; 8];
    if ext.len() > lower.len() {
        return "application/octet-stream";
    }

    let lower = &mut lower[..ext.len()];
    lower.copy_from_slice(ext);
    lower.make_ascii_lowercase();
    match &*lower {
        b"html" | b"htm" => "text/html; charset=utf-8",
        b"css" => "text/css; charset=utf-8",
        b"js" | b"mjs" => "text/javascript; charset=utf-8",
        b"json" | b"map" => "application/json",
        b"txt" => "text/plain; charset=utf-8",
        b"xml" => "application/xml",
        b"svg" => "image/svg+xml",
        b"png" => "image/png",
        b"jpg" | b"jpeg" => "image/jpeg",
        b"gif" => "image/gif",
        b"webp" => "image/webp",
        b"avif" => "image/avif",
        b"ico" => "image/x-icon",
        b"woff" => "font/woff",
        b"woff2" => "font/woff2",
        b"ttf" => "font/ttf",
        b"otf" => "font/otf",
        b"pdf" => "application/pdf",
        b"wasm" => "application/wasm",
        b"mp4" => "video/mp4",
        b"webm" => "video/webm",
        b"mp3" => "audio/mpeg",
        b"ogg" => "audio/ogg",
        b"wav" => "audio/wav",
        b"zip" => "application/zip",
        b"gz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

fn two(out: &mut [u8], at: usize, v: i64) {
    out[at] = b'0' + (v / 10) as u8;
    out[at + 1] = b'0' + (v % 10) as u8;
}

// seconds since the epoch to an imf-fixdate, days to civil date after howard hinnant
pub fn stamp(secs: i64) -> Stamp {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (yoe + era * 400 + (month <= 2) as i64).clamp(0, 9999);
    let weekday = (days + 4).rem_euclid(7) as usize;
    let mut out = *b"Sun, 00 Jan 0000 00:00:00 GMT";
    out[..3].copy_from_slice(&b"SunMonTueWedThuFriSat"[weekday * 3..weekday * 3 + 3]);
    two(&mut out, 5, day);
    out[8..11].copy_from_slice(
        &b"JanFebMarAprMayJunJulAugSepOctNovDec"[(month as usize - 1) * 3..month as usize * 3],
    );
    two(&mut out, 12, year / 100);
    two(&mut out, 14, year % 100);
    two(&mut out, 17, rem / 3600);
    two(&mut out, 20, rem / 60 % 60);
    two(&mut out, 23, rem % 60);

    out
}

pub struct Meta<'a> {
    pub date: &'a Stamp,
    pub modified: &'a Stamp,
    pub ctype: &'a str,
    pub keep: bool,
}

struct Out<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl Out<'_> {
    fn put(&mut self, s: &[u8]) {
        self.buf[self.n..self.n + s.len()].copy_from_slice(s);
        self.n += s.len();
    }

    fn num(&mut self, mut v: u64) {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        loop {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }

        self.put(&tmp[i..]);
    }

    fn start<'a>(buf: &'a mut [u8], status: &[u8], date: &Stamp) -> Out<'a> {
        let mut o = Out { buf, n: 0 };
        o.put(status);
        o.put(b"\r\nDate: ");
        o.put(date);
        o
    }

    fn entity(&mut self, m: &Meta) {
        self.put(b"\r\nLast-Modified: ");
        self.put(m.modified);
        self.put(b"\r\nAccept-Ranges: bytes\r\nContent-Type: ");
        self.put(m.ctype.as_bytes());
    }

    fn finish(mut self, keep: bool) -> usize {
        self.put(if keep {
            b"\r\n\r\n"
        } else {
            b"\r\nConnection: close\r\n\r\n"
        });
        self.n
    }
}

pub fn head_200(buf: &mut [u8], m: &Meta, size: u64) -> usize {
    let mut o = Out::start(buf, b"HTTP/1.1 200 OK", m.date);
    o.entity(m);
    o.put(b"\r\nContent-Length: ");
    o.num(size);
    o.finish(m.keep)
}

pub fn head_206(buf: &mut [u8], m: &Meta, first: u64, last: u64, size: u64) -> usize {
    let mut o = Out::start(buf, b"HTTP/1.1 206 Partial Content", m.date);
    o.entity(m);
    o.put(b"\r\nContent-Range: bytes ");
    o.num(first);
    o.put(b"-");
    o.num(last);
    o.put(b"/");
    o.num(size);
    o.put(b"\r\nContent-Length: ");
    o.num(last - first + 1);
    o.finish(m.keep)
}

pub fn head_304(buf: &mut [u8], m: &Meta) -> usize {
    let mut o = Out::start(buf, b"HTTP/1.1 304 Not Modified", m.date);
    o.put(b"\r\nLast-Modified: ");
    o.put(m.modified);
    o.finish(m.keep)
}

pub fn head_416(buf: &mut [u8], m: &Meta, size: u64) -> usize {
    let mut o = Out::start(buf, b"HTTP/1.1 416 Range Not Satisfiable", m.date);
    o.put(b"\r\nContent-Range: bytes */");
    o.num(size);
    o.put(b"\r\nContent-Length: 0");
    o.finish(m.keep)
}

// redirects a directory path to the same path with a trailing slash
pub fn head_301(buf: &mut [u8], date: &Stamp, path: &[u8], keep: bool) -> usize {
    let mut o = Out::start(buf, b"HTTP/1.1 301 Moved Permanently", date);
    o.put(b"\r\nLocation: /");
    for &b in path {
        if b.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&b) {
            o.put(&[b]);
            continue;
        }

        o.put(&[
            b'%',
            b"0123456789ABCDEF"[(b >> 4) as usize],
            b"0123456789ABCDEF"[(b & 15) as usize],
        ]);
    }

    o.put(b"/\r\nContent-Length: 0");
    o.finish(keep)
}
