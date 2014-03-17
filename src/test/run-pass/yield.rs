// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::task;

pub fn main() {
    let mut builder = task::task();
    let mut result = builder.future_result();
    builder.spawn(child);
    println!("1");
    task::deschedule();
    println!("2");
    task::deschedule();
    println!("3");
    result.recv();
}

fn child() {
    println!("4"); task::deschedule(); println!("5"); task::deschedule(); println!("6");
}
