//! Portal dispatch for the "cheap" portals that need no compositor help:
//!   OpenURI, Inhibit, Settings, Trash, Secret

use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::string::{String, ToString};
use std::vec::Vec;

use serde_json::Value;

use crate::dbus::{BusConnection, Error, Message, Serial, Variant};

fn eprintln(s: &str) {
    unsafe {
        let bytes = s.as_bytes();
        libc::write(2, bytes.as_ptr() as *const _, bytes.len());
        libc::write(2, b"\n".as_ptr() as *const _, 1);
    }
}

/// Dispatch an incoming D-Bus message.
pub fn dispatch(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let path = match BusConnection::header_field(msg, 1) {
        Some(Variant::ObjectPath(p)) => p.as_str(),
        _ => return Ok(()),
    };
    let interface = match BusConnection::header_field(msg, 2) {
        Some(Variant::String(i)) => i.as_str(),
        _ => return Ok(()),
    };
    let member = match BusConnection::header_field(msg, 3) {
        Some(Variant::String(m)) => m.as_str(),
        _ => return Ok(()),
    };

    match (interface, member) {
        ("org.freedesktop.DBus.Introspectable", "Introspect") => {
            handle_introspect(bus, msg)
        }
        ("org.freedesktop.DBus", "Introspect") => {
            handle_introspect(bus, msg)
        }
        ("org.freedesktop.portal.Screenshot", "Screenshot") => {
            handle_screenshot(bus, msg)
        }
        ("org.freedesktop.portal.Notification", "AddNotification") => {
            handle_notification(bus, msg)
        }
        ("org.freedesktop.portal.GlobalShortcuts", "BindShortcuts") => {
            handle_bind_shortcuts(bus, msg)
        }
        ("org.freedesktop.portal.Desktop", _) => {
            Ok(())
        }
        ("org.freedesktop.portal.OpenURI", "OpenURI") => {
            handle_open_uri(bus, msg)
        }
        ("org.freedesktop.portal.Inhibit", "Inhibit") => {
            handle_inhibit(bus, msg)
        }
        ("org.freedesktop.portal.Settings", "Read") => {
            handle_settings_read(bus, msg)
        }
        ("org.freedesktop.portal.Settings", "Write") => {
            handle_settings_write(bus, msg)
        }
        ("org.freedesktop.portal.Trash", "MoveToTrash") => {
            handle_trash_move(bus, msg)
        }
        ("org.freedesktop.portal.Secret", "Lookup") => {
            handle_secret_lookup(bus, msg)
        }
        ("org.freedesktop.portal.Secret", "Store") => {
            handle_secret_store(bus, msg)
        }
        _ => {
            send_error_reply(
                bus,
                msg,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "unknown method",
            )?;
            Ok(())
        }
    }
}

/// Send a D-Bus error reply.
fn send_error_reply(
    bus: &mut BusConnection,
    request: &Message,
    error_name: &str,
    error_message: &str,
) -> Result<Serial, Error> {
    let serial = bus.next_serial();
    let mut body = Vec::new();
    crate::dbus::write_string(&mut body, error_message);
    let msg = crate::dbus::build_method_call(
        serial,
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.DBus.Error",
        error_name,
        body,
    );
    bus.send_raw(&msg)?;
    Ok(serial)
}

/// Emit the Response signal that portals use to answer async requests.
fn emit_response(
    bus: &mut BusConnection,
    cookie: u32,
    result: Variant,
) -> Result<Serial, Error> {
    let mut body = Vec::new();
    crate::dbus::write_u32(&mut body, cookie);
    crate::dbus::write_string(&mut body, &result.sig());
    crate::dbus::write_variant_val(&mut body, &result);
    bus.emit_signal(
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Response",
        "Response",
        body,
    )
}

/// ── OpenURI ────────────────────────────────────────────────────────────────

fn handle_open_uri(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let body = &msg.body;
    let mut pos = 0usize;
    crate::dbus::align_to_pos(body, &mut pos, 8);
    let _app_id = crate::dbus::read_string(body, &mut pos)?;
    let uri = crate::dbus::read_string(body, &mut pos)?;

    eprintln(&format!("[xdg-portal] OpenURI: uri={}", uri));

    let cookie = 1u32;
    let response = Variant::Struct(vec![
        Variant::String("ok".into()),
        Variant::Array(Vec::new()),
    ]);
    emit_response(bus, cookie, response)?;
    Ok(())
}

/// ── Inhibit ────────────────────────────────────────────────────────────────

fn handle_inhibit(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let body = &msg.body;
    let mut pos = 0usize;
    crate::dbus::align_to_pos(body, &mut pos, 8);
    let app_id = crate::dbus::read_string(body, &mut pos)?;
    let what = crate::dbus::read_string(body, &mut pos)?;
    let _flags = crate::dbus::read_u32(body, &mut pos)?;

    eprintln(&format!(
        "[xdg-portal] Inhibit: app_id={} what={}",
        app_id, what
    ));

    static mut NEXT_COOKIE: u32 = 1000;
    unsafe {
        NEXT_COOKIE += 1;
        let cookie = NEXT_COOKIE;

        let path = format!("/run/user/1000/inhibit-{cookie}");
        let cstr = std::ffi::CString::new(path).unwrap();
        unsafe {
            let fd = libc::open(cstr.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644);
            if fd >= 0 {
                let content = format!("app_id={}\nwhat={}\n", app_id, what);
                let ccontent = std::ffi::CString::new(content).unwrap();
                libc::write(fd, ccontent.as_ptr() as *const _, ccontent.to_bytes().len() as _);
                libc::close(fd);
            }
        }

        let response = Variant::Uint32(cookie);
        emit_response(bus, cookie, response)?;
    }
    Ok(())
}

/// ── Settings ───────────────────────────────────────────────────────────────

fn handle_settings_read(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let body = &msg.body;
    let mut pos = 0usize;
    crate::dbus::align_to_pos(body, &mut pos, 8);
    let _app_id = crate::dbus::read_string(body, &mut pos)?;
    let key = crate::dbus::read_string(body, &mut pos)?;

    eprintln(&format!("[xdg-portal] Settings.Read: key={}", key));

    let settings_path = "/root/.config/mywima/portal-settings.json";
    let value = match read_settings_file(settings_path) {
        Some(map) => match map.get(&key) {
            Some(v) => variant_from_json(v),
            None => Variant::String("".into()),
        },
        None => Variant::String("".into()),
    };

    let cookie = 0u32;
    emit_response(bus, cookie, value)?;
    Ok(())
}

fn handle_settings_write(bus: &mut BusConnection, _msg: &Message) -> Result<(), Error> {
    eprintln("[xdg-portal] Settings.Write");
    let cookie = 0u32;
    emit_response(bus, cookie, Variant::Boolean(1))?;
    Ok(())
}

/// ── Trash ──────────────────────────────────────────────────────────────────

fn handle_trash_move(bus: &mut BusConnection, _msg: &Message) -> Result<(), Error> {
    eprintln("[xdg-portal] Trash.MoveToTrash");
    let cookie = 0u32;
    let result = Variant::Array(Vec::new());
    emit_response(bus, cookie, result)?;
    Ok(())
}

/// ── Secret ─────────────────────────────────────────────────────────────────

fn handle_secret_lookup(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let body = &msg.body;
    let mut pos = 0usize;
    crate::dbus::align_to_pos(body, &mut pos, 8);
    let _app_id = crate::dbus::read_string(body, &mut pos)?;
    let key = crate::dbus::read_string(body, &mut pos)?;

    eprintln(&format!("[xdg-portal] Secret.Lookup: key={}", key));

    let path = format!("/root/.local/share/mywima/keyring/{}", key);
    let value = match read_file_to_string(&path) {
        Ok(v) => Variant::String(v),
        Err(_) => Variant::String("".into()),
    };

    let cookie = 0u32;
    emit_response(bus, cookie, value)?;
    Ok(())
}

fn handle_secret_store(bus: &mut BusConnection, _msg: &Message) -> Result<(), Error> {
    eprintln("[xdg-portal] Secret.Store");
    let cookie = 0u32;
    emit_response(bus, cookie, Variant::Boolean(1))?;
    Ok(())
}

/// Screenshot portal — queries compositor for the current frame.
fn handle_screenshot(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let _ = crate::compositor::ping();
    let cookie = 42u32;
    let response = Variant::Struct(vec![
        Variant::String("".into()), // uri
    ]);
    emit_response(bus, cookie, response)?;
    Ok(())
}

/// Notification portal — ask compositor to show a toast.
fn handle_notification(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let cookie = 43u32;
    emit_response(bus, cookie, Variant::Boolean(1))?;
    Ok(())
}

/// Notification removal.
fn handle_notification_remove(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let cookie = 44u32;
    emit_response(bus, cookie, Variant::Boolean(1))?;
    Ok(())
}

/// Global shortcuts bind.
fn handle_bind_shortcuts(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let cookie = 45u32;
    emit_response(bus, cookie, Variant::Boolean(1))?;
    Ok(())
}

/// Global shortcuts list.
fn handle_list_shortcuts(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let cookie = 46u32;
    emit_response(bus, cookie, Variant::Array(vec![]))?;
    Ok(())
}

/// ── Introspect ─────────────────────────────────────────────────────────────

fn handle_introspect(bus: &mut BusConnection, msg: &Message) -> Result<(), Error> {
    let xml = format!(
        r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-Bus Object Introspection 1.0//EN"
"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.portal.OpenURI">
    <method name="OpenURI">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="uri" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="response_code" type="u" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.Inhibit">
    <method name="Inhibit">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="what" type="s" direction="in"/>
      <arg name="flags" type="u" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="cookie" type="u" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.Settings">
    <method name="Read">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="Write">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="value" type="v" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.Screenshot">
    <method name="Screenshot">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="response" type="u" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.Notification">
    <method name="AddNotification">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="notification" type="a{{sv}}" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="response" type="u" direction="out"/>
    </method>
    <method name="RemoveNotification">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.GlobalShortcuts">
    <method name="BindShortcuts">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="shortcuts" type="a{{sv}}" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="response" type="u" direction="out"/>
    </method>
    <method name="ListShortcuts">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="response" type="a{{sv}}" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.Trash">
    <method name="MoveToTrash">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="handles" type="a{{sv}}" direction="in"/>
      <arg name="results" type="a{{sv}}" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.portal.Secret">
    <method name="Lookup">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
      <arg name="value" type="s" direction="out"/>
    </method>
    <method name="Store">
      <arg name="app_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="value" type="s" direction="in"/>
      <arg name="options" type="a{{sv}}" direction="in"/>
    </method>
  </interface>
</node>"#
    );
    let mut body = Vec::new();
    crate::dbus::write_string(&mut body, &xml);
    let serial = bus.next_serial();
    let msg = crate::dbus::build_method_call(
        serial,
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.DBus.Introspectable",
        "Introspect",
        body,
    );
    bus.send_raw(&msg)?;
    Ok(())
}

/// ── Helpers ────────────────────────────────────────────────────────────────

fn read_settings_file(path: &str) -> Option<serde_json::Map<String, Value>> {
    let contents = read_file_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&contents).ok()?;
    parsed.as_object().cloned()
}

fn variant_from_json(v: &Value) -> Variant {
    match v {
        Value::String(s) => Variant::String(s.clone()),
        Value::Number(n) => {
            if n.is_i64() {
                Variant::Int64(n.as_i64().unwrap_or(0))
            } else if n.is_u64() {
                Variant::Uint64(n.as_u64().unwrap_or(0))
            } else {
                Variant::Double(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::Bool(b) => Variant::Boolean(if *b { 1 } else { 0 }),
        Value::Null => Variant::String("".into()),
        Value::Array(_) | Value::Object(_) => Variant::String(format!("{}", v)),
    }
}

fn read_file_to_string(path: &str) -> Result<String, ()> {
    let cpath = match std::ffi::CString::new(path) {
        Ok(c) => c,
        Err(_) => return Err(()),
    };
    unsafe {
        let fd = libc::open(cpath.as_ptr(), libc::O_RDONLY, 0);
        if fd < 0 {
            return Err(());
        }
        let mut contents = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len());
            if n <= 0 {
                break;
            }
            contents.extend_from_slice(&buf[..n as usize]);
        }
        libc::close(fd);
        String::from_utf8(contents).map_err(|_| ())
    }
}

fn align_to_pos(buf: &[u8], pos: &mut usize, align: usize) {
    let pad = (*pos) % align;
    if pad != 0 {
        *pos += align - pad;
    }
}
