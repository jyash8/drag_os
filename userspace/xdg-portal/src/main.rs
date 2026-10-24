//! xdg-desktop-portal — mediated interface between unprivileged apps and the desktop.

use crate::dbus::BusConnection;

mod compositor;
mod dbus;
mod portal;

fn eprintln(s: &str) {
    unsafe {
        let bytes = s.as_bytes();
        libc::write(2, bytes.as_ptr() as *const _, bytes.len());
        libc::write(2, b"\n".as_ptr() as *const _, 1);
    }
}

pub fn main() {
    let addr = match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        Ok(a) => a,
        Err(_) => {
            eprintln("[xdg-portal] DBUS_SESSION_BUS_ADDRESS not set");
            std::process::exit(1);
        }
    };

    let path = addr.strip_prefix("unix:path=").unwrap_or(&addr);

    let mut bus = match BusConnection::connect(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln(&format!("[xdg-portal] connect error: {:?}", e));
            std::process::exit(1);
        }
    };

    let unique_name = match bus.hello() {
        Ok(name) => name,
        Err(e) => {
            eprintln(&format!("[xdg-portal] Hello failed: {:?}", e));
            std::process::exit(1);
        }
    };
    eprintln(&format!("[xdg-portal] connected as {}", unique_name));

    if let Err(e) = bus.request_name("org.freedesktop.portal.Desktop", 0) {
        eprintln(&format!("[xdg-portal] RequestName failed: {:?}", e));
        std::process::exit(1);
    }
    eprintln("[xdg-portal] owns org.freedesktop.portal.Desktop");

    loop {
        match bus.read_message() {
            Ok(msg) => {
                if let Err(e) = crate::portal::dispatch(&mut bus, &msg) {
                    eprintln(&format!("[xdg-portal] dispatch error: {:?}", e));
                }
            }
            Err(crate::dbus::Error::ConnectionClosed) => break,
            Err(e) => {
                eprintln(&format!("[xdg-portal] read error: {:?}", e));
                break;
            }
        }
    }
}
