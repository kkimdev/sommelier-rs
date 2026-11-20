/*
Copyright 2026 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::os::fd::AsFd;

use super::*;

#[test]
fn keymap_mmap_is_private_not_shared() {
    use nix::sys::memfd::{memfd_create, MFdFlags};
    use std::ffi::CString;

    let fd = memfd_create(
        CString::new("test-keymap-private").unwrap().as_c_str(),
        MFdFlags::MFD_CLOEXEC,
    )
    .expect("memfd_create failed");
    nix::unistd::ftruncate(&fd, 4096).expect("ftruncate failed");
    let view = MmapView::from_fd(fd.as_fd(), 4096).expect("mmap failed");
    let address = view.address();
    let smaps = std::fs::read_to_string("/proc/self/smaps").expect("read smaps");
    let mapping_header = smaps
        .lines()
        .position(|line| {
            let mut fields = line.split_whitespace();
            let Some(range) = fields.next() else {
                return false;
            };
            let Some((start, end)) = range.split_once('-') else {
                return false;
            };
            usize::from_str_radix(start, 16).ok().is_some_and(|start| {
                usize::from_str_radix(end, 16)
                    .ok()
                    .is_some_and(|end| address >= start && address < end)
            })
        })
        .expect("mapping must appear in smaps");
    let vm_flags = smaps
        .lines()
        .skip(mapping_header + 1)
        .take_while(|line| {
            let Some(range) = line.split_whitespace().next() else {
                return true;
            };
            let Some((start, end)) = range.split_once('-') else {
                return true;
            };
            !(usize::from_str_radix(start, 16).is_ok() && usize::from_str_radix(end, 16).is_ok())
        })
        .find_map(|line| line.strip_prefix("VmFlags:"))
        .expect("mapping must have VmFlags");
    assert!(
        !vm_flags.split_whitespace().any(|flag| flag == "sh"),
        "keymap must use MAP_PRIVATE, got shared mapping flags: {vm_flags}"
    );
}
