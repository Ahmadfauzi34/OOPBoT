//! Fondasi lintas domain (`Layer 0`) tanpa ketergantungan ke modul domain lain.
//!
//! - [`error`]: tipe error bersama (`BoxError`) & ekstraksi pesan error HTTP/HTML.
//! - [`fs`]: pembacaan JSON aman & penulisan file atomik (`0o600` untuk kredensial).
//! - [`time`]: timestamp Unix epoch & konversi/parsing ISO-8601 UTC.

pub mod error;
pub(crate) mod fs;
pub(crate) mod time;

pub use error::BoxError;
