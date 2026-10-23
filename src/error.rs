// SPDX-License-Identifier: GPL-3.0-or-later
//! The one error type: a message meant for the person running the command.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `err!("snapshot {id} ...")` - an [`Error`] from a format string.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::error::Error(format!($($arg)*)) };
}

/// Return early with an [`Error`].
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::err!($($arg)*)) };
}
