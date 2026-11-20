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

use std::env;
use std::fs;
use std::path::Path;
use wayland_codegen::generator::{check_byte_indexed_strings, generate, generate_routing};
use wayland_codegen::parse;

fn main() {
    let out_dir = env::var_os("OUT_DIR").unwrap();

    // Every vendored protocol the proxy speaks.
    let protocols = [
        "../third_party/protocols/wayland.xml",
        "../third_party/protocols/xdg-shell.xml",
        "../third_party/protocols/linux-dmabuf-v1.xml",
        "../third_party/protocols/viewporter.xml",
        "../third_party/protocols/text-input-unstable-v3.xml",
        "../third_party/protocols/text-input-unstable-v1.xml",
        "../third_party/protocols/text-input-extension-unstable-v1.xml",
        "../third_party/protocols/xdg-decoration-unstable-v1.xml",
        "../third_party/protocols/fractional-scale-v1.xml",
        "../third_party/protocols/xdg-output-unstable-v1.xml",
        "../third_party/protocols/keyboard-extension-unstable-v1.xml",
        "../third_party/protocols/gtk-shell.xml",
        "../third_party/protocols/aura-shell.xml",
    ];

    let mut parsed_protocols = Vec::with_capacity(protocols.len());
    let mut code = String::new();

    for path_str in protocols {
        let protocol_path = Path::new(path_str);

        println!("cargo:rerun-if-changed={}", protocol_path.display());

        let protocol =
            parse(protocol_path).unwrap_or_else(|e| panic!("Failed to parse {path_str}: {e}"));
        code += &generate(&protocol);
        parsed_protocols.push(protocol);
    }

    check_byte_indexed_strings(&parsed_protocols);
    code += &generate_routing(&parsed_protocols);

    // One file, which `main.rs` includes as `protocols`: adding a protocol
    // is one line above.
    let dest_path = Path::new(&out_dir).join("protocols.rs");
    fs::write(&dest_path, code)
        .unwrap_or_else(|_| panic!("Failed to write {}", dest_path.display()));
}
