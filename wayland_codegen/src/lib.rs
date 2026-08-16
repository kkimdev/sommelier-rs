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

pub mod generator;
pub mod protocol;

use crate::protocol::Protocol;
use quick_xml::de::from_reader;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub fn parse<P: AsRef<Path>>(path: P) -> Result<Protocol, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let protocol: Protocol = from_reader(reader)?;
    Ok(protocol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn without_whitespace(code: &str) -> String {
        code.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    #[test]
    fn test_parse_wayland() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/wayland.xml");
        let protocol = parse(protocol_path).expect("Failed to parse wayland.xml");
        assert_eq!(protocol.name, "wayland");

        let interfaces: Vec<&crate::protocol::Interface> = protocol
            .items
            .iter()
            .filter_map(|item| {
                if let crate::protocol::ProtocolItem::Interface(i) = item {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        assert!(!interfaces.is_empty());
    }

    #[test]
    fn test_generate_wayland() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/wayland.xml");
        let protocol = parse(protocol_path).expect("Failed to parse wayland.xml");
        let code = generator::generate(&protocol);
        let compact = without_whitespace(&code);
        assert!(code.contains("pub mod wl_display"));
        assert!(code.contains("const REQ_SYNC"));
        assert!(code.contains("pub enum Request"));
        assert!(code.contains("fn from_wire"));
        assert!(
            code.contains("allocate_guest_server_id"),
            "host-generated new_id events must allocate a guest server ID"
        );
        assert!(
            compact.contains("map_id(guest_id,id)"),
            "generated host-generated new_id events must map guest IDs to host IDs"
        );
        assert!(
            compact.contains("mark_pending_destroy(msg.sender_id)"),
            "generated destructor requests must retain mappings until host delete_id"
        );
        assert!(
            compact.contains("is_guest_server_id(msg.sender_id)")
                && compact.contains("remove_id(msg.sender_id)"),
            "generated server-object destructors must release mappings without delete_id"
        );
        assert!(
            compact.contains("queue_local_delete_id"),
            "generated local-only destructors must synthesize a guest delete_id"
        );
        assert!(
            compact.contains(
                "pubfnconsume_event(interface:&str,msg:&mutWireMessage,)->Result<(),ProtocolError>"
            ),
            "generated protocols must expose schema-aware event consumption"
        );
        assert!(
            compact.contains(
                "pubfnconsume_event(msg:&mutWireMessage)->Result<(),ProtocolError>{let_=Event::from_wire(msg)?;Ok(())}"
            ),
            "event consumption must decode the complete schema without dispatching a handler"
        );
        assert!(
            compact.contains("_=>Err(ProtocolError::InvalidObjectId(msg.sender_id))"),
            "event consumption must reject interfaces outside the generated protocol"
        );
    }

    #[test]
    fn test_generate_nullable_strings_without_collapsing_null_to_empty() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path =
            manifest_dir.join("../third_party/protocols/text-input-unstable-v3.xml");
        let protocol = parse(protocol_path).expect("Failed to parse text-input-unstable-v3.xml");
        let code = generator::generate(&protocol);
        let compact = without_whitespace(&code);

        assert!(
            compact.contains("text:Option<String>"),
            "nullable protocol strings must use Option<String> in generated messages"
        );
        assert!(
            compact.contains("msg.read_nullable_string()?"),
            "nullable protocol strings must use the nullable wire decoder"
        );
        assert!(
            compact.contains("builder.write_nullable_string(text.as_deref())"),
            "nullable protocol strings must preserve a null wire value when forwarded"
        );
    }

    #[test]
    fn test_generate_since_version_guards() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/xdg-shell.xml");
        let protocol = parse(protocol_path).expect("Failed to parse xdg-shell.xml");
        let code = generator::generate(&protocol);
        let compact = without_whitespace(&code);

        assert!(
            compact.contains("guest_object_version(msg.sender_id)"),
            "generated requests must check the negotiated guest object version"
        );
        assert!(
            compact.contains("host_object_version(msg.sender_id)"),
            "generated events must check the negotiated host object version"
        );
        assert!(
            compact.contains("ProtocolError::UnsupportedVersion"),
            "version failures must be reported as protocol errors"
        );
    }

    #[test]
    fn test_request_object_validation_accepts_local_only_objects() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/gtk-shell.xml");
        let protocol = parse(protocol_path).expect("Failed to parse gtk-shell.xml");
        let code = generator::generate(&protocol);
        let compact = without_whitespace(&code);

        assert!(
            compact.contains(
                "get_host_id(surface).is_none()&&!ctx.shadow_table.is_local_only_guest_object(surface)"
            ),
            "typed object arguments must accept synthetic local-only objects for local handlers"
        );
    }
}
