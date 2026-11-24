//! Bindings for the phone app and its file provider extensions.
//!
//! Owns: the interface exposed through UniFFI to Swift and Kotlin: the
//! core's work in steps a host can run inside any moment the operating
//! system grants and stop between (pull changes, plan, next transfers,
//! transfer done), each with a budget of count and deadline; the read
//! functions an extension calls (children, item, changes since); and the
//! descriptions of transfers the operating system performs itself.
//!
//! Shaped for the harder host: an extension with little memory, started
//! for seconds. Nothing here loads a tree or starts a runtime to answer a
//! listing.
