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
            compact.contains("adopt_host_created(id,\"wl_data_offer\","),
            "host-generated new_id events must get a guest object"
        );
        assert!(
            compact.contains("guest_destroyed(msg.sender_id)"),
            "generated destructor requests must end the guest object"
        );
        // wl_display.sync is the first request arm and has a new_id.
        let sender_check = compact
            .find("Droppingrequestfromlocalobject")
            .expect("request sender check");
        let allocation = compact.find("create_paired(").expect("new_id allocation");
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

    /// The strings other arguments index by byte offset decode to
    /// `IndexedString`, and every other string to a lossy `String`.
    #[test]
    fn test_generate_byte_indexed_strings() {
        let v1 = generate_compact("text-input-unstable-v1.xml");
        assert!(
            v1.contains(
                "SetSurroundingText{text:crate::wire::IndexedString,cursor:u32,anchor:u32}"
            )
        );
        assert!(
            v1.contains("PreeditString{serial:u32,text:crate::wire::IndexedString,commit:String}"),
            "only the preedit is indexed"
        );
        assert!(v1.contains("CommitString{serial:u32,text:String}"));
        assert!(v1.contains("msg.read_indexed_string()?"));
        assert!(
            v1.contains("builder.write_indexed_string(&text)"),
            "a forward sends the string as it came"
        );
        assert!(
            v1.contains("_text:&crate::wire::IndexedString"),
            "a handler gets the IndexedString"
        );
        assert!(
            v1.contains("pubfnpreedit_string(sender_id:u32,serial:u32,text:&str,commit:&str)"),
            "an encoder takes a str, as the proxy builds only UTF-8"
        );

        let v3 = generate_compact("text-input-unstable-v3.xml");
        assert!(
            v3.contains(
                "SetSurroundingText{text:crate::wire::IndexedString,cursor:i32,anchor:i32}"
            )
        );
        assert!(v3.contains(
            "PreeditString{text:Option<crate::wire::IndexedString>,cursor_begin:i32,cursor_end:i32}"
        ));
        assert!(v3.contains("msg.read_nullable_indexed_string()?"));
        assert!(v3.contains("builder.write_nullable_indexed_string(text.as_ref())"));
        assert!(v3.contains("CommitString{text:Option<String>}"));

        let xdg_shell = generate_compact("xdg-shell.xml");
        assert!(xdg_shell.contains("SetTitle{title:String}"));
        assert!(!xdg_shell.contains("IndexedString"));
    }

    /// Every BYTE_INDEXED_STRINGS entry names one string argument of the
    /// vendored protocols, and one that names none, or no string, is
    /// reported, so a stale entry fails the build.
    #[test]
    fn test_byte_indexed_strings_each_name_one_argument() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocols: Vec<_> = std::fs::read_dir(manifest_dir.join("../third_party/protocols"))
            .expect("the vendored protocols")
            .map(|entry| entry.expect("a protocol file").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "xml"))
            .map(|path| parse(&path).expect("a vendored protocol"))
            .collect();
        generator::check_byte_indexed_strings(&protocols);
        let table = [
            ("zwp_text_input_v3", "set_surrounding_text", "text"),
            ("zwp_text_input_v3", "set_surrounding_text", "txt"),
            ("zwp_text_input_v3", "set_surrounding_text", "cursor"),
            ("zwp_text_input_v4", "set_surrounding_text", "text"),
        ];
        assert_eq!(generator::unmatched_entries(&protocols, &table), table[1..]);
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
            wayland.contains("#[must_use=\"amessagethatisnotqueuedisneversent\"]pubfncreate_pool("),
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
                "Ok((builder.try_build_message(sender_id,super::REQ_RECEIVE)?,vec![fd]))"
            ),
            "the message owns its descriptors"
        );
        let request_encoders = wayland.find("pubmodreq{").expect("request encoders");
        let event_encoders = wayland.find("pubmodevt{").expect("event encoders");
        assert!(request_encoders < event_encoders);
    }

    #[test]
    fn test_generate_descriptor_ownership() {
        let wayland = generate_compact("wayland.xml");
        assert!(
            wayland.contains("CreatePool{id:u32,fd:std::os::fd::OwnedFd,size:i32}"),
            "a decoded message owns its descriptors"
        );
        assert!(
            wayland.contains(
                "fnon_create_pool(&mutself,_ctx:&mutContext,_id:u32,_fd:std::os::fd::BorrowedFd<'_>,_size:i32)"
            ),
            "a handler borrows the message's descriptors"
        );
        assert!(
            wayland.contains("builder.write_fd(fd);"),
            "forwarding moves the descriptor into the outgoing message"
        );
        // The generated module allows clippy::disallowed_methods for
        // MessageBuilder::new, which also silences the descriptor guard.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocols = manifest_dir.join("../third_party/protocols");
        for entry in std::fs::read_dir(protocols).expect("read the vendored protocols") {
            let path = entry.expect("a protocol file").path();
            if path.extension().is_none_or(|extension| extension != "xml") {
                continue;
            }
            let file = path.file_name().unwrap().to_str().unwrap();
            let code = generate_compact(file);
            for raw in ["into_raw_fd", "from_raw_fd", "borrow_raw", "::close("] {
                assert!(!code.contains(raw), "{file} generates {raw}");
            }
        }
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
    fn test_request_object_arguments_are_validated_and_translated_at_once() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocol_path = manifest_dir.join("../third_party/protocols/gtk-shell.xml");
        let protocol = parse(protocol_path).expect("Failed to parse gtk-shell.xml");
        let code = generator::generate(&protocol);
        let compact = without_whitespace(&code);

        assert!(
            compact.contains(
                "lettranslated_surface=ctx.shadow_table.request_object(surface,\"wl_surface\").ok_or(ProtocolError::InvalidObjectId(surface))?;"
            ),
            "an object argument must be resolved once, before the handler runs"
        );
    }

    /// A request that names a local object where the host needs one of its
    /// own is dropped before its `new_id` is mapped, so it uses up no host
    /// ID: libwayland-server refuses the next new ID after a gap.
    #[test]
    fn test_request_object_arguments_are_checked_before_new_id_mapping() {
        let compact = generate_compact("xdg-output-unstable-v1.xml");
        let arm = compact
            .find("Request::GetXdgOutput{id,output}=>{")
            .map(|start| &compact[start..])
            .expect("get_xdg_output dispatch arm");
        let local_check = arm
            .find("namingalocalobjectasnon-nullableargument")
            .expect("local object check");
        let mapping = arm.find("create_paired(").expect("new_id mapping");
        assert!(
            local_check < mapping,
            "every object argument must be checked before the new_id is mapped"
        );
    }

    /// No forwarded message of any vendored protocol can drop once it has
    /// mapped its `new_id`: every check that drops it comes first, whatever
    /// the shape of its arguments.
    #[test]
    fn test_no_forward_drops_after_mapping_a_new_id() {
        use crate::protocol::{InterfaceItem, MessageItem, ProtocolItem};
        let protocols = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../third_party/protocols");
        for entry in std::fs::read_dir(protocols).expect("read the vendored protocols") {
            let path = entry.expect("a protocol file").path();
            if path.extension().is_none_or(|extension| extension != "xml") {
                continue;
            }
            let protocol = parse(&path).expect("parse a vendored protocol");
            // Every request and event with a `new_id` argument has one
            // dispatch arm, whose forward block maps it.
            let creating_messages = protocol
                .items
                .iter()
                .filter_map(|item| match item {
                    ProtocolItem::Interface(interface) => Some(&interface.items),
                    _ => None,
                })
                .flatten()
                .filter(|item| {
                    let (InterfaceItem::Request(message) | InterfaceItem::Event(message)) = item
                    else {
                        return false;
                    };
                    message
                        .items
                        .iter()
                        .any(|item| matches!(item, MessageItem::Arg(arg) if arg.typ == "new_id"))
                })
                .count();
            let compact = without_whitespace(&generator::generate(&protocol));
            // Each forward block runs from its handler's verdict to the
            // message it returns.
            let ends: Vec<_> = compact.split("returnOk(Some(outgoing));").collect();
            let mut mapping_forwards = 0;
            for end in &ends[..ends.len() - 1] {
                let start = end.rfind("==Action::Forward{").expect("a forward block");
                let block = &end[start..];
                let Some(mapping) = ["create_paired(", "adopt_host_created("]
                    .iter()
                    .filter_map(|mapping| block.find(mapping))
                    .min()
                else {
                    continue;
                };
                mapping_forwards += 1;
                assert!(
                    !block[mapping..].contains("returnOk(None)"),
                    "{}: a forward drops after mapping a new_id: {block}",
                    path.display()
                );
            }
            assert_eq!(
                mapping_forwards,
                creating_messages,
                "{}: every message that creates an object has a forward block that maps it",
                path.display()
            );
        }
    }

    /// An event drops for an object argument of no interface that has no
    /// guest counterpart before its `new_id` is mapped, so it uses up no
    /// guest server ID. No vendored event has both; this one does.
    #[test]
    fn test_event_object_arguments_are_checked_before_new_id_mapping() {
        let xml = r#"
        <protocol name="order">
          <interface name="thing" version="1">
            <event name="made">
              <arg name="id" type="new_id" interface="thing"/>
              <arg name="cause" type="object"/>
            </event>
          </interface>
        </protocol>
        "#;
        let protocol: crate::protocol::Protocol =
            quick_xml::de::from_str(xml).expect("Failed to parse XML");
        let compact = without_whitespace(&generator::generate(&protocol));
        let arm = compact
            .find("Event::Made{id,cause}=>{")
            .map(|start| &compact[start..])
            .expect("made dispatch arm");
        let unmapped_check = arm
            .find("naminganobjectwithnoguestcounterpart")
            .expect("unmapped object check");
        let mapping = arm.find("adopt_host_created(").expect("new_id mapping");
        assert!(
            unmapped_check < mapping,
            "every object argument must be checked before the new_id is mapped"
        );
    }

    #[test]
    #[should_panic(expected = "interface `wl_display` is defined by more than one protocol")]
    fn routing_rejects_an_interface_defined_twice() {
        let wayland =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../third_party/protocols/wayland.xml");
        let protocol = parse(&wayland).expect("Failed to parse wayland.xml");
        generator::generate_routing(&[protocol.clone(), protocol]);
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
        assert!(code.contains("fn interface_version"));
        assert!(code.contains("fn consume_event"));
        let code = without_whitespace(&code);
        assert!(
            code.contains(
                "\"wl_surface\"=>wayland::wl_surface::Event::from_wire(msg).map(Self::WlSurface),"
            ),
            "a test decodes an event of any interface by its sender's interface"
        );
        assert!(
            code.contains("implFrom<xdg_shell::xdg_toplevel::Request>forAnyRequest"),
            "a test names a message of any interface by its own type"
        );
    }

    /// `new_object` reports the one object a message creates.
    #[test]
    fn test_no_message_creates_two_objects() {
        use crate::protocol::{InterfaceItem, MessageItem, ProtocolItem};
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let protocols = manifest_dir.join("../third_party/protocols");
        for entry in std::fs::read_dir(protocols).expect("read the vendored protocols") {
            let path = entry.expect("a protocol file").path();
            if path.extension().is_none_or(|extension| extension != "xml") {
                continue;
            }
            let protocol = parse(&path).expect("parse a vendored protocol");
            for item in &protocol.items {
                let ProtocolItem::Interface(interface) = item else {
                    continue;
                };
                for item in &interface.items {
                    let (InterfaceItem::Request(message) | InterfaceItem::Event(message)) = item
                    else {
                        continue;
                    };
                    let new_ids = message
                        .items
                        .iter()
                        .filter(|item| matches!(item, MessageItem::Arg(arg) if arg.typ == "new_id"))
                        .count();
                    assert!(
                        new_ids <= 1,
                        "{}.{} creates {new_ids} objects",
                        interface.name,
                        message.name
                    );
                }
            }
        }
    }

    #[test]
    fn test_generate_test_equality_and_created_objects() {
        let wayland = generate_compact("wayland.xml");
        assert!(
            wayland.contains(
                "(Self::CreatePool{id:l_id,fd:l_fd,size:l_size},Self::CreatePool{id:r_id,fd:r_fd,size:r_size})=>{true&&l_id==r_id&&crate::test_support::same_file(l_fd,r_fd)&&l_size==r_size}"
            ),
            "a descriptor compares by the file it refers to"
        );
        assert!(
            wayland.contains("Self::CreatePool{id,..}=>Some((*id,\"wl_shm_pool\")),"),
            "a typed new_id creates an object of its interface"
        );
        assert!(
            wayland.contains("Self::Bind{id,..}=>Some((id.2,id.0.as_str())),"),
            "a bind creates an object of the interface it names"
        );
        assert!(
            wayland.contains("Self::DataOffer{id,..}=>Some((*id,\"wl_data_offer\")),"),
            "an event creates objects too"
        );
    }
}
