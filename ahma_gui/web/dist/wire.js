// What crosses between the server and this page. Hand-written mirror of
// ahma_gui/src/dto.rs (protocol 1); that file is the source of truth.
//
// Rust `Option<T>` is `T | null` here (always present on the wire). A View's
// regions are *absent* when unchanged (`skip_serializing_if`), hence optional.
export const PROTOCOL = 1;
