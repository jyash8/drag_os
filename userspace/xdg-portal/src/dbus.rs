//! D-Bus wire-format marshaling.
//!
//! Implements only what xdg-portal needs:
//!   * method_call / method_return / signal message types
//!   * types: y b u i t x d s o g v a{sv}
//!   * Hello, RequestName, method dispatch, Response signal emission
//!
//! No async runtime, no zbus — just raw bytes over a Unix socket.

use std::ffi::{CStr, CString};
use std::vec::Vec;
use std::string::{String, ToString};

use libc::{c_char, close, connect, recv, send, socket, sockaddr_un, AF_UNIX, SOCK_STREAM};

/// A D-Bus serial number (monotonically increasing per connection).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Serial(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    MethodCall = 1,
    MethodReturn = 2,
    Error = 3,
    Signal = 4,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub msg_type: MessageType,
    pub flags: u8,
    pub serial: Serial,
    pub header_fields: Vec<(u8, Variant)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Variant {
    Byte(u8),
    Boolean(u32),
    Uint32(u32),
    Int32(i32),
    Uint64(u64),
    Int64(i64),
    Double(f64),
    String(String),
    ObjectPath(String),
    Signature(String),
    Variant(Box<Variant>),
    Array(Vec<Variant>),
    DictEntry(Box<(Variant, Variant)>),
    Struct(Vec<Variant>),
}

impl Variant {
    /// Signature byte(s) for this variant.
    pub fn sig(&self) -> String {
        match self {
            Variant::Byte(_) => "y".into(),
            Variant::Boolean(_) => "b".into(),
            Variant::Uint32(_) => "u".into(),
            Variant::Int32(_) => "i".into(),
            Variant::Uint64(_) => "t".into(),
            Variant::Int64(_) => "x".into(),
            Variant::Double(_) => "d".into(),
            Variant::String(_) => "s".into(),
            Variant::ObjectPath(_) => "o".into(),
            Variant::Signature(_) => "g".into(),
            Variant::Variant(_) => "v".into(),
            Variant::Array(v) => {
                if v.is_empty() { "av".into() }
                else { format!("a{}", v[0].sig()) }
            }
            Variant::DictEntry(b) => {
                let (k, v) = b.as_ref();
                format!("a{{{}}}", k.sig() + &v.sig())
            }
            Variant::Struct(f) => {
                let mut s = String::from("(");
                for fld in f { s.push_str(&fld.sig()); }
                s.push(')');
                s
            }
        }
    }
}

/// D-Bus errors.
#[derive(Debug)]
pub enum Error {
    ConnectionClosed,
    Io(String),
    Protocol(String),
    Unknown,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ConnectionClosed => write!(f, "connection closed"),
            Error::Io(s) => write!(f, "io: {}", s),
            Error::Protocol(s) => write!(f, "protocol: {}", s),
            Error::Unknown => write!(f, "unknown error"),
        }
    }
}

/// A connected D-Bus client.
pub struct BusConnection {
    pub fd: i32,
    pub next_serial: u32,
    pub read_buf: Vec<u8>,
}

impl BusConnection {
    pub fn connect(path: &str) -> Result<Self, Error> {
        unsafe {
            let fd = socket(AF_UNIX, SOCK_STREAM, 0);
            if fd < 0 {
                return Err(Error::Io("socket".into()));
            }
            let mut addr: sockaddr_un = std::mem::zeroed();
            addr.sun_family = AF_UNIX as u16;
            let path_bytes = path.as_bytes();
            if path_bytes.len() >= addr.sun_path.len() {
                close(fd);
                return Err(Error::Io("path too long".into()));
            }
            let sun_path = &mut addr.sun_path;
            for (i, &b) in path_bytes.iter().enumerate() {
                sun_path[i] = b as i8;
            }
            let addr_len = std::mem::size_of_val(&addr) as u32;
            let ret = connect(fd, &addr as *const _ as *const _, addr_len);
            if ret < 0 {
                close(fd);
                return Err(Error::Io("connect".into()));
            }

            // ── D-Bus authentication handshake (EXTERNAL mechanism) ─────────
            // After connecting, the server expects an auth negotiation:
            //   1. Client sends NUL to begin
            //   2. Server replies with OK <methods> or REJECTED <reason>
            //   3. Client sends AUTH EXTERNAL <hex-uid>\r\n
            //   4. Server replies with OK <uuid>\r\n
            //   5. Client sends BEGIN\r\n
            //   6. Regular D-Bus protocol begins

            // Step 1: send NUL + AUTH EXTERNAL <uid> without waiting for
            // intermediate response. Some dbus-daemon versions don't respond
            // to bare NUL.
            let uid = unsafe { libc::getuid() };
            let uid_str = format!("{}", uid);
            let mut hex = String::new();
            for b in uid_str.bytes() {
                hex.push_str(&format!("{:02x}", b));
            }
            let auth_cmd = format!("AUTH EXTERNAL {}\r\n", hex);

            send(fd, &0u8 as *const u8 as *const _, 1, 0);
            send(fd, auth_cmd.as_ptr() as *const _, auth_cmd.len(), 0);

            // Step 2: read server's response (OK or REJECTED)
            let mut auth_buf = [0u8; 512];
            let mut auth_len = 0;
            while auth_len < auth_buf.len() {
                let n = recv(fd, auth_buf.as_mut_ptr().add(auth_len) as *mut _, 1, 0);
                if n <= 0 {
                    close(fd);
                    return Err(Error::ConnectionClosed);
                }
                auth_len += 1;
                if auth_buf[auth_len - 1] == b'\n' {
                    break;
                }
            }
            let auth_resp = std::str::from_utf8(&auth_buf[..auth_len]).unwrap_or("");

            if !auth_resp.starts_with("OK ") {
                close(fd);
                return Err(Error::Protocol(format!(
                    "dbus auth failed: {}",
                    auth_resp.trim()
                )));
            }

            // Step 3: negotiate unix fd passing
            let neg = b"NEGOTIATE_UNIX_FD\r\n";
            send(fd, neg.as_ptr() as *const _, neg.len(), 0);

            // Read AGREE_UNIX_FD response
            let mut neg_buf = [0u8; 512];
            let mut neg_len = 0;
            while neg_len < neg_buf.len() {
                let n = recv(fd, neg_buf.as_mut_ptr().add(neg_len) as *mut _, 1, 0);
                if n <= 0 {
                    close(fd);
                    return Err(Error::ConnectionClosed);
                }
                neg_len += 1;
                if neg_buf[neg_len - 1] == b'\n' {
                    break;
                }
            }

            // Don't send BEGIN here - will be sent with first message
            Ok(Self {
                fd,
                next_serial: 1,
                read_buf: Vec::new(),
            })
        }
    }

    pub fn next_serial(&mut self) -> Serial {
        let s = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        Serial(s)
    }
}

impl Drop for BusConnection {
    fn drop(&mut self) { unsafe { close(self.fd); } }
}

pub fn read_exact(fd: i32, buf: &mut [u8]) -> Result<(), Error> {
    let mut read = 0;
    while read < buf.len() {
        unsafe {
            let n = recv(fd, buf.as_mut_ptr().add(read) as *mut core::ffi::c_void, buf.len() - read, 0);
            if n <= 0 { return Err(Error::ConnectionClosed); }
            read += n as usize;
        }
    }
    Ok(())
}




pub fn write_u32(buf: &mut Vec<u8>, v: u32) { buf.extend_from_slice(&v.to_le_bytes()); }
pub fn write_i32(buf: &mut Vec<u8>, v: i32) { buf.extend_from_slice(&v.to_le_bytes()); }
pub fn write_u64(buf: &mut Vec<u8>, v: u64) { buf.extend_from_slice(&v.to_le_bytes()); }
pub fn write_i64(buf: &mut Vec<u8>, v: i64) { buf.extend_from_slice(&v.to_le_bytes()); }
pub fn write_f64(buf: &mut Vec<u8>, v: f64) { buf.extend_from_slice(&v.to_le_bytes()); }
pub fn write_byte(buf: &mut Vec<u8>, v: u8) { buf.push(v); }
pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    write_u32(buf, bytes.len() as u32);
    buf.extend_from_slice(bytes);
    buf.push(0);
}

/// Write a D-Bus signature (1-byte length + chars + NUL).
pub fn write_signature(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
    buf.push(0);
}

pub fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32, Error> {
    if *pos + 4 > buf.len() { return Err(Error::Protocol("truncated u32".into())); }
    let v = u32::from_le_bytes([buf[*pos], buf[*pos+1], buf[*pos+2], buf[*pos+3]]); *pos += 4; Ok(v)
}
pub fn read_i32(buf: &[u8], pos: &mut usize) -> Result<i32, Error> { Ok(read_u32(buf, pos)? as i32) }
pub fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64, Error> {
    if *pos + 8 > buf.len() { return Err(Error::Protocol("truncated u64".into())); }
    let v = u64::from_le_bytes([buf[*pos],buf[*pos+1],buf[*pos+2],buf[*pos+3],buf[*pos+4],buf[*pos+5],buf[*pos+6],buf[*pos+7]]); *pos += 8; Ok(v)
}
pub fn read_i64(buf: &[u8], pos: &mut usize) -> Result<i64, Error> { Ok(read_u64(buf, pos)? as i64) }
pub fn read_f64(buf: &[u8], pos: &mut usize) -> Result<f64, Error> { let b = read_u64(buf, pos)?; Ok(f64::from_bits(b)) }
pub fn read_byte(buf: &[u8], pos: &mut usize) -> Result<u8, Error> { if *pos >= buf.len() { return Err(Error::Protocol("truncated byte".into())); } let v = buf[*pos]; *pos += 1; Ok(v) }
pub fn read_string(buf: &[u8], pos: &mut usize) -> Result<String, Error> {
    let len = read_u32(buf, pos)? as usize;
    if *pos + len + 1 > buf.len() { return Err(Error::Protocol("truncated string".into())); }
    let s = std::str::from_utf8(&buf[*pos..*pos+len]).map_err(|_| Error::Protocol("utf8".into()))?;
    *pos += len + 1;
    Ok(s.to_string())
}

/// Read a D-Bus signature (1-byte length + chars + NUL).
pub fn read_signature(buf: &[u8], pos: &mut usize) -> Result<String, Error> {
    if *pos >= buf.len() { return Err(Error::Protocol("truncated signature".into())); }
    let len = buf[*pos] as usize;
    *pos += 1;
    if *pos + len + 1 > buf.len() { return Err(Error::Protocol("truncated signature".into())); }
    let s = std::str::from_utf8(&buf[*pos..*pos+len]).map_err(|_| Error::Protocol("utf8".into()))?;
    *pos += len + 1;
    Ok(s.to_string())
}

fn read_variant_sig(buf: &[u8], pos: &mut usize, sig: &str) -> Result<Variant, Error> {
    if sig.is_empty() { return Err(Error::Protocol("empty sig".into())); }
    let first = sig.as_bytes()[0];
    match first {
        b'y' => Ok(Variant::Byte(read_byte(buf, pos)?)),
        b'b' => Ok(Variant::Boolean(read_u32(buf, pos)?)),
        b'u' => Ok(Variant::Uint32(read_u32(buf, pos)?)),
        b'i' => Ok(Variant::Int32(read_i32(buf, pos)?)),
        b't' => Ok(Variant::Uint64(read_u64(buf, pos)?)),
        b'x' => Ok(Variant::Int64(read_i64(buf, pos)?)),
        b'd' => Ok(Variant::Double(read_f64(buf, pos)?)),
        b's' => Ok(Variant::String(read_string(buf, pos)?)),
        b'o' => Ok(Variant::ObjectPath(read_string(buf, pos)?)),
        b'g' => Ok(Variant::Signature(read_signature(buf, pos)?)),
        b'v' => { let s = read_string(buf, pos)?; read_variant_sig(buf, pos, &s).map(|v| Variant::Variant(Box::new(v))) }
        b'a' => {
            if sig.len() < 2 { return Err(Error::Protocol("trunc array".into())); }
            let second = sig.as_bytes()[1];
            if second == b'{' {
                // dict entry: find matching }
                let rest = &sig[1..];
                let brace_end = rest.find('}').ok_or(Error::Protocol("no brace".into()))?;
                let inner = &rest[..brace_end];
                let mut key_sig = String::new();
                let mut val_sig = String::new();
                let mut depth = 0;
                let mut in_key = true;
                for ch in inner.bytes() {
                    match ch {
                        b'a' => {
                            depth += 1;
                            if in_key { key_sig.push('a'); } else { val_sig.push('a'); }
                        }
                        b'{' => { depth += 1; if in_key { key_sig.push('{'); } else { val_sig.push('{'); } }
                        b'}' => {
                            depth -= 1;
                            if depth == 0 && in_key { in_key = false; }
                            else if in_key { key_sig.push('}'); } else { val_sig.push('}'); }
                        }
                        b',' => { if depth == 0 && in_key { in_key = false; } else if in_key { key_sig.push(','); } else { val_sig.push(','); } }
                        ch => { if in_key { key_sig.push(ch as char); } else { val_sig.push(ch as char); } }
                    }
                }
                align_to_pos(buf, pos, 8);
                let key = read_variant_sig(buf, pos, &key_sig)?;
                align_to_pos(buf, pos, 8);
                let value = read_variant_sig(buf, pos, &val_sig)?;
                Ok(Variant::DictEntry(Box::new((key, value))))
            } else {
                let elem_sig = &sig[1..];
                align_to_pos(buf, pos, 8);
                let len = read_u32(buf, pos)? as usize;
                let start = *pos;
                let mut items = Vec::new();
                let mut p = start;
                for _ in 0..len {
                    let elem_align = match elem_sig.as_bytes()[0] {
                        b't' | b'x' | b'd' => 8,
                        b'u' | b'i' => 4,
                        _ => 1,
                    };
                    align_to_pos_slice(&buf[start..], &mut (p - start), elem_align);
                    let item = read_variant_sig(buf, &mut p, elem_sig)?;
                    items.push(item);
                }
                *pos = p;
                Ok(Variant::Array(items))
            }
        }
        b'(' => {
            let mut field_sigs: Vec<String> = Vec::new();
            let mut depth: u32 = 0;
            let mut current = String::new();
            for ch in sig[1..].bytes() {
                match ch {
                    b'(' => { depth += 1; current.push(ch as char); }
                    b')' => {
                        if depth == 0 {
                            if !current.is_empty() { field_sigs.push(current); }
                            break;
                        } else {
                            depth -= 1;
                            current.push(ch as char);
                        }
                    }
                    b',' => {
                        if depth == 0 {
                            field_sigs.push(core::mem::take(&mut current));
                        } else {
                            current.push(ch as char);
                        }
                    }
                    ch => current.push(ch as char),
                }
            }
            align_to_pos(buf, pos, 8);
            let mut fields = Vec::new();
            for fsig in &field_sigs {
                let f = read_variant_sig(buf, pos, fsig)?;
                fields.push(f);
            }
            Ok(Variant::Struct(fields))
        }
        _ => Err(Error::Protocol("unknown sig".into())),
    }
}

pub fn align_to_pos(buf: &[u8], pos: &mut usize, align: usize) {
    let pad = (*pos) % align;
    if pad != 0 { *pos += align - pad; }
}

fn align_to_pos_slice(buf: &[u8], pos: &mut usize, align: usize) {
    let pad = (*pos) % align;
    if pad != 0 { *pos += align - pad; }
}

pub fn align_to_vec(buf: &mut Vec<u8>, align: usize) {
    let pad = buf.len() % align;
    if pad != 0 {
        buf.extend(std::iter::repeat(0u8).take(align - pad));
    }
}

pub fn align_to_vec_pub(buf: &mut Vec<u8>, align: usize) { align_to_vec(buf, align) }
pub fn align_to_pos_pub(buf: &[u8], pos: &mut usize, align: usize) { align_to_pos(buf, pos, align); }

/// Write a variant value (without its signature prefix) to `buf`.
pub fn write_variant_val(buf: &mut Vec<u8>, v: &Variant) {
    match v {
        Variant::Byte(x) => buf.push(*x),
        Variant::Boolean(x) => { buf.extend_from_slice(&x.to_le_bytes()); }
        Variant::Uint32(x) => { buf.extend_from_slice(&x.to_le_bytes()); }
        Variant::Int32(x) => { buf.extend_from_slice(&x.to_le_bytes()); }
        Variant::Uint64(x) => { buf.extend_from_slice(&x.to_le_bytes()); }
        Variant::Int64(x) => { buf.extend_from_slice(&x.to_le_bytes()); }
        Variant::Double(x) => { buf.extend_from_slice(&x.to_le_bytes()); }
        Variant::String(s) => {
            write_u32(buf, s.len() as u32);
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        Variant::ObjectPath(s) => {
            write_u32(buf, s.len() as u32);
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        Variant::Signature(s) => {
            buf.push(s.len() as u8);
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        Variant::Variant(inner) => {
            write_signature(buf, &inner.sig());
            write_variant_val(buf, inner);
        }
        Variant::Array(items) => {
            if items.is_empty() {
                write_u32(buf, 0);
                return;
            }
            let elem_align = match items[0].sig().as_bytes()[0] {
                b't' | b'x' | b'd' => 8,
                b'u' | b'i' => 4,
                _ => 1,
            };
            let mut elem_buf = Vec::new();
            for item in items {
                align_to_vec(&mut elem_buf, elem_align);
                write_variant_val(&mut elem_buf, item);
            }
            write_u32(buf, elem_buf.len() as u32);
            buf.extend_from_slice(&elem_buf);
        }
        Variant::DictEntry(b) => {
            let (k, v) = b.as_ref();
            align_to_vec(buf, 8);
            write_variant_val(buf, k);
            align_to_vec(buf, 8);
            write_variant_val(buf, v);
        }
        Variant::Struct(fields) => {
            align_to_vec(buf, 8);
            for f in fields {
                write_variant_val(buf, f);
            }
        }
    }
}

/// Build a D-Bus method_call message.
pub fn build_method_call(serial: Serial, path: &str, interface: &str, member: &str, body: Vec<u8>) -> Vec<u8> {
    let mut header = Vec::new();
    header.push(b'l'); // little endian
    header.push(MessageType::MethodCall as u8);
    header.push(0); // flags
    header.push(1); // version

    // body_length (4 bytes)
    let bl = body.len() as u32;
    header.extend_from_slice(&bl.to_le_bytes());
    // serial (4 bytes)
    header.extend_from_slice(&serial.0.to_le_bytes());

    let mut fields_bytes = Vec::new();
    // path field code=1
    fields_bytes.push(1u8);
    write_signature(&mut fields_bytes, "o");
    write_variant_val(&mut fields_bytes, &Variant::ObjectPath(path.to_string()));
    while fields_bytes.len() % 8 != 0 { fields_bytes.push(0); }
    // interface... wait, interface is field 2, but dbus-send uses order 1,6,2,3
    // Let's match dbus-send order: 1, 6, 2, 3
    // destination field code=6
    fields_bytes.push(6u8);
    write_signature(&mut fields_bytes, "s");
    write_variant_val(&mut fields_bytes, &Variant::String(interface.to_string()));
    while fields_bytes.len() % 8 != 0 { fields_bytes.push(0); }
    // interface field code=2
    fields_bytes.push(2u8);
    write_signature(&mut fields_bytes, "s");
    write_variant_val(&mut fields_bytes, &Variant::String(interface.to_string()));
    while fields_bytes.len() % 8 != 0 { fields_bytes.push(0); }
    // member field code=3 (last field, no trailing padding in array)
    fields_bytes.push(3u8);
    write_signature(&mut fields_bytes, "s");
    write_variant_val(&mut fields_bytes, &Variant::String(member.to_string()));

    // header fields array length (4 bytes)
    let fl = fields_bytes.len() as u32;
    header.extend_from_slice(&fl.to_le_bytes());
    // header fields array
    header.extend_from_slice(&fields_bytes);

    let header_len = header.len();
    let body_start = if header_len % 8 != 0 {
        header_len + (8 - (header_len % 8))
    } else {
        header_len
    };
    let mut msg = header;
    msg.resize(body_start, 0);
    msg.extend_from_slice(&body);
    msg
}

impl BusConnection {
    pub fn send_raw(&mut self, msg: &[u8]) -> Result<(), Error> {
        unsafe {
            let n = send(self.fd, msg.as_ptr() as *const core::ffi::c_void, msg.len(), 0);
            if n < 0 || n as usize != msg.len() {
                return Err(Error::Io("send".into()));
            }
        }
        Ok(())
    }

    pub fn read_message(&mut self) -> Result<Message, Error> {
        // Read header (16 bytes fixed part)
        let mut header = [0u8; 16];
        read_exact(self.fd, &mut header)?;

        if header[0] != b'l' {
            return Err(Error::Protocol("not LE".into()));
        }
        let msg_type = match header[1] {
            1 => MessageType::MethodCall,
            2 => MessageType::MethodReturn,
            3 => MessageType::Error,
            4 => MessageType::Signal,
            _ => return Err(Error::Protocol("unknown type".into())),
        };
        let body_length = read_u32(&header, &mut 4)?;
        let serial = Serial(read_u32(&header, &mut 8)?);

        // Header fields array length is at offset 12
        let fields_len = read_u32(&header, &mut 12)? as usize;

        let mut fields_data = vec![0u8; fields_len];
        read_exact(self.fd, &mut fields_data)?;

        // Skip padding to 8-byte boundary before body
        let total_so_far = 16 + fields_len;
        let padding = (8 - (total_so_far % 8)) % 8;
        if padding > 0 {
            let mut pad_buf = vec![0u8; padding];
            read_exact(self.fd, &mut pad_buf)?;
        }

        let mut header_fields = Vec::new();
        let mut fp = 0usize;
        while fp < fields_data.len() {
            let code = read_byte(&fields_data, &mut fp)?;
            let vsig = read_signature(&fields_data, &mut fp)?;
            let variant = read_variant_sig(&fields_data, &mut fp, &vsig)?;
            header_fields.push((code, variant));
            // Align to 4 bytes (max variant alignment in header fields)
            while fp % 4 != 0 && fp < fields_data.len() {
                fp += 1;
            }
        }

        let mut body = vec![0u8; body_length as usize];
        read_exact(self.fd, &mut body)?;

        Ok(Message {
            msg_type,
            flags: header[2],
            serial,
            header_fields,
            body,
        })
    }

    pub fn header_field(msg: &Message, code: u8) -> Option<&Variant> {
        for (c, v) in &msg.header_fields {
            if *c == code {
                return Some(v);
            }
        }
        None
    }

    pub fn variant_as_string(v: &Variant) -> Option<&str> {
        match v {
            Variant::String(s) => Some(s),
            Variant::ObjectPath(s) => Some(s),
            _ => None,
        }
    }

    pub fn variant_as_u32(v: &Variant) -> Option<u32> {
        match v {
            Variant::Uint32(v) => Some(*v),
            _ => None,
        }
    }

    pub fn variant_as_dict_sv(v: &Variant) -> Option<Vec<(String, Variant)>> {
        match v {
            Variant::Array(items) => {
                let mut result = Vec::new();
                for item in items {
                    if let Variant::DictEntry(b) = item {
                        let (k, v) = b.as_ref();
                        if let Variant::String(key) = k {
                            result.push((key.clone(), v.clone()));
                        }
                    }
                }
                Some(result)
            }
            _ => None,
        }
    }

    pub fn hello(&mut self) -> Result<String, Error> {
        let serial = self.next_serial();
        let msg = build_method_call(serial, "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", Vec::new());
        // Send BEGIN + message in one write (like dbus-send)
        let mut combined = Vec::new();
        combined.extend_from_slice(b"BEGIN\r\n");
        combined.extend_from_slice(&msg);
        unsafe {
            send(self.fd, combined.as_ptr() as *const _, combined.len(), 0);
        }
        loop {
            let reply = self.read_message()?;
            if reply.serial == serial {
                let mut pos = 0usize;
                align_to_pos(&reply.body, &mut pos, 4);
                let name = read_string(&reply.body, &mut pos)?;
                return Ok(name);
            }
        }
    }

    pub fn request_name(&mut self, name: &str, flags: u32) -> Result<u32, Error> {
        let serial = self.next_serial();
        let mut body = Vec::new();
        align_to_vec(&mut body, 8);
        write_string(&mut body, name);
        write_u32(&mut body, flags);
        let msg = build_method_call(serial, "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName", body);
        self.send_raw(&msg)?;
        loop {
            let reply = self.read_message()?;
            if reply.serial == serial {
                let mut pos = 0usize;
                align_to_pos(&reply.body, &mut pos, 4);
                let result = read_u32(&reply.body, &mut pos)?;
                return Ok(result);
            }
        }
    }

    pub fn emit_signal(&mut self, path: &str, interface: &str, member: &str, body: Vec<u8>) -> Result<Serial, Error> {
        let serial = self.next_serial();
        let msg = build_method_call(serial, path, interface, member, body);
        self.send_raw(&msg)?;
        Ok(serial)
    }
}
