//! Hearth's sync API over HTTP, behind the core's `Remote` interface.
//!
//! Owns: every request of the contract (see the `CONTRACT` file at the
//! repository's root for the document and the commit of it this code
//! implements), their encoding and decoding, the one place where an error
//! answer is turned into the engine's error classes, sign-in of a device
//! and the keeping of its token in the system's secret store.
//!
//! It implements the contract and nothing else: no WebDAV, no other
//! server. It decides nothing about sync; it tells the core exactly what
//! the server said.
#![forbid(unsafe_code)]
