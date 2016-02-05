/*
 * The Rust SGXS library.
 *
 * (C) Copyright 2016 Jethro G. Beekman
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; version 2
 * of the License.
 */

#![feature(asm)]
#![feature(unsafe_no_drop_flag)]
#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate ioctl as ioctl_crate;
extern crate libc;
extern crate byteorder;
extern crate crypto;

pub mod abi;
pub mod sgxdev;
pub mod sgxs;
mod intelcall;
pub mod loader;

#[cfg(target_endian="big")] const E:ENDIANNESS_NOT_SUPPORTED=();
