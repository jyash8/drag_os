use alloc::{string::String, vec::Vec};

use crate::arch::sync::spinlock::SpinLock;

pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
}

/// Very basic user registry. ponytail: a flat Vec is fine until the OS needs
/// anything beyond uid->(gid,name); swap for BTreeMap<u32, User> when lookups
/// by uid outnumber registrations.
static USERS: SpinLock<Vec<User>> = SpinLock::new(Vec::new());

pub fn init() {
    let _ = add_user(0, 0, "root");
}

pub fn add_user(uid: u32, gid: u32, name: &str) -> Result<(), ()> {
    let mut users = USERS.lock();
    if users.iter().any(|u| u.uid == uid) {
        return Err(());
    }
    users.push(User {
        uid,
        gid,
        name: String::from(name),
    });
    Ok(())
}

pub fn user_by_uid(uid: u32) -> Option<User> {
    USERS.lock().iter().find(|u| u.uid == uid).map(|u| User {
        uid: u.uid,
        gid: u.gid,
        name: u.name.clone(),
    })
}
