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
        // wl_display.sync is the first request arm and has a new_id.
        let sender_check = compact
            .find("Droppingrequestfromunmappedsender")
            .expect("request sender check");
        let allocation = compact
            .find("allocate_host_id()")
            .expect("new_id allocation");
        assert!(
            sender_check < allocation,
            "a request's sender must be checked before its new_id is mapped"
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
    fn test_generate_interface_version_from_protocol_xml() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/aura-shell.xml");
        let protocol = parse(protocol_path).expect("Failed to parse aura-shell.xml");
        let compact = without_whitespace(&generator::generate(&protocol));

        let zaura_shell_start = compact
            .find("pubmodzaura_shell{")
            .expect("zaura_shell module");
        assert!(
            compact[zaura_shell_start..]
                .starts_with("pubmodzaura_shell{usesuper::*;pubconstVERSION:u32=38u32;"),
            "an interface's XML version must become its VERSION constant"
        );
    }

    #[test]
    fn test_generate_since_constants_from_protocol_xml() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/aura-shell.xml");
        let protocol = parse(protocol_path).expect("Failed to parse aura-shell.xml");
        let code = generator::generate(&protocol);
        let compact = without_whitespace(&code);

        assert!(
            compact.contains("pubconstREQ_SET_WINDOW_BOUNDS_SINCE:u32=29u32;"),
            "a request's since attribute must become a named constant"
        );
        assert!(
            compact.contains("pubconstREQ_GET_AURA_SURFACE_SINCE:u32=1u32;"),
            "a request without since exists from version 1"
        );
        let zaura_toplevel_start = compact
            .find("pubmodzaura_toplevel{")
            .expect("zaura_toplevel module");
        let zaura_toplevel = &compact[zaura_toplevel_start..];
        let zaura_toplevel = &zaura_toplevel[..zaura_toplevel
            .find("pubmodzaura_popup{")
            .unwrap_or(zaura_toplevel.len())];
        assert!(
            zaura_toplevel.contains("pubconstEVT_CONFIGURE_SINCE:u32=29u32;"),
            "events get since constants too, scoped to their interface"
        );
    }

    fn generate_compact(file: &str) -> String {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols").join(file);
        let protocol = parse(protocol_path).expect("Failed to parse protocol fixture");
        without_whitespace(&generator::generate(&protocol))
    }

    #[test]
    fn test_generate_enum_constants_from_protocol_xml() {
        let wayland = generate_compact("wayland.xml");
        assert!(
            wayland.contains("pubconstERROR_INVALID_FD:u32=2u32;"),
            "an entry becomes <ENUM>_<ENTRY> in its interface module"
        );
        assert!(
            wayland.contains("pubconstFORMAT_NV12:u32=842094158u32;"),
            "hexadecimal values are decoded"
        );
        assert!(
            wayland.contains("pubconstTRANSFORM_FLIPPED_270:i32=7i32;"),
            "an enum that an int argument carries is i32"
        );

        let extension = generate_compact("text-input-extension-unstable-v1.xml");
        assert!(
            extension.contains("pubconstINPUT_FLAGS_SPELLCHECK_ON:u32=16u32;"),
            "shifted values are decoded"
        );
    }

    #[test]
    fn test_generate_enum_since_from_protocol_xml() {
        let xdg_shell = generate_compact("xdg-shell.xml");
        let toplevel = &xdg_shell[xdg_shell
            .find("pubmodxdg_toplevel{")
            .expect("xdg_toplevel module")..];
        assert!(
            toplevel
                .contains("pubfnstate_since(value:u32)->Option<u32>{matchvalue{1u32=>Some(1u32),"),
            "an entry without since exists from version 1"
        );
        assert!(
            toplevel.contains("9u32=>Some(6u32),"),
            "an entry's since attribute is its version"
        );
        assert!(
            toplevel.contains("_=>None,"),
            "a value the XML does not define has no version"
        );

        let extension = generate_compact("text-input-extension-unstable-v1.xml");
        assert!(
            extension.contains(
                "pubfninput_type_since(value:u32)->Option<u32>{matchvalue{0u32=>Some(2u32),"
            ),
            "an entry inherits its enum's since attribute"
        );
    }

    #[test]
    fn test_generate_typed_encoders() {
        let wayland = generate_compact("wayland.xml");
        assert!(
            wayland
                .contains("pubfncreate_pool(sender_id:u32,id:u32,fd:OwnedFd,size:i32)->Outgoing{"),
            "an encoder takes the XML arguments in order, and owns its descriptors"
        );
        assert!(
            wayland.contains(
                "#[must_use=\"amessagethatisnotqueuedleaksitsdescriptors\"]pubfncreate_pool("
            ),
            "an unused encoder result is a warning"
        );
        assert!(
            wayland
                .contains("pubfnattach(sender_id:u32,buffer:Option<u32>,x:i32,y:i32)->Outgoing{"),
            "a nullable object is optional"
        );
        assert!(
            wayland.contains(
                "pubfnbind(sender_id:u32,name:u32,interface:&str,version:u32,id:u32)->Result<Outgoing,ProtocolError>{"
            ),
            "an untyped new_id names its interface and version, and a string makes the encoder fallible"
        );
        assert!(
            wayland.contains(
                "Ok((builder.try_build_message(sender_id,super::REQ_RECEIVE)?,vec![fd.into_raw_fd()]))"
            ),
            "a descriptor becomes raw only after its message is built"
        );
        let request_encoders = wayland.find("pubmodreq{").expect("request encoders");
        let event_encoders = wayland.find("pubmodevt{").expect("event encoders");
        assert!(request_encoders < event_encoders);
    }

    #[test]
    #[should_panic(expected = "thing generates the constant REQ_PING twice")]
    fn test_generate_rejects_colliding_constant_names() {
        let xml = r#"
        <protocol name="collision">
          <interface name="thing" version="1">
            <request name="ping"/>
            <enum name="req">
              <entry name="ping" value="0"/>
            </enum>
          </interface>
        </protocol>
        "#;
        let protocol: crate::protocol::Protocol =
            quick_xml::de::from_str(xml).expect("Failed to parse XML");
        generator::generate(&protocol);
    }

    #[test]
    fn test_generate_one_since_arm_per_enum_value() {
        let xml = r#"
        <protocol name="aliases">
          <interface name="thing" version="3">
            <enum name="mode">
              <entry name="old" value="1"/>
              <entry name="new_name" value="1" since="3"/>
              <entry name="other" value="2" since="2"/>
            </enum>
          </interface>
        </protocol>
        "#;
        let protocol: crate::protocol::Protocol =
            quick_xml::de::from_str(xml).expect("Failed to parse XML");
        let compact = without_whitespace(&generator::generate(&protocol));
        assert!(
            compact.contains("pubconstMODE_OLD:u32=1u32;pubconstMODE_NEW_NAME:u32=1u32;"),
            "every entry keeps its constant"
        );
        assert!(
            compact.contains("matchvalue{1u32=>Some(1u32),2u32=>Some(2u32),_=>None,}"),
            "a shared value has one arm, with the older version"
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

    #[test]
    fn test_generate_routing() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocols = ["wayland.xml", "xdg-shell.xml"]
            .iter()
            .map(|file| parse(manifest_dir.join("../third_party/protocols").join(file)))
            .collect::<Result<Vec<_>, _>>()
            .expect("Failed to parse routing protocol fixtures");

        let code = generator::generate_routing(&protocols);
        assert!(code.contains("enum ProtocolFamily"));
        assert!(code.contains("Wayland"));
        assert!(code.contains("XdgShell"));
        assert!(code.contains("wl_surface"));
        assert!(code.contains("fn classify_interface"));
        assert!(code.contains("fn consume_event"));
    }
}
