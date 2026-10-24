#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// Not enough physical memory is available.
    OutOfMemory,

    /// The requested alignment is invalid.
    InvalidAlignment,

    /// The allocator detected internal corruption or an
    /// impossible state.
    Corrupted,

    /// The allocator backend failed unexpectedly.
    Backend,

    AlreadyMapped,
}

use core::fmt;

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocError::OutOfMemory => write!(f, "out of memory"),
            AllocError::InvalidAlignment => write!(f, "invalid alignment"),
            AllocError::Corrupted => write!(f, "allocator corrupted"),
            AllocError::Backend => write!(f, "allocator backend failure"),
            AllocError::AlreadyMapped => write!(f, "entry already exists"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(AllocError::OutOfMemory.to_string(), "out of memory");
        assert_eq!(AllocError::InvalidAlignment.to_string(), "invalid alignment");
        assert_eq!(AllocError::Corrupted.to_string(), "allocator corrupted");
        assert_eq!(AllocError::Backend.to_string(), "allocator backend failure");
        assert_eq!(AllocError::AlreadyMapped.to_string(), "entry already exists");
    }

    #[test]
    fn errors_are_eq_clone_copy() {
        let a = AllocError::OutOfMemory;
        let b = a; // Copy
        assert_eq!(a, b); // Eq
    }
}