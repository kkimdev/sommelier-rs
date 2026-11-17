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

use crate::protocol::*;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use std::collections::{BTreeMap, HashSet};

pub fn generate(protocol: &Protocol) -> String {
    let mut parts = Vec::new();
    let mut interface_names = Vec::new();
    let mut handler_trait_names = Vec::new();
    let mut dispatch_request_cases = Vec::new();
    let mut dispatch_event_cases = Vec::new();
    let mut consume_event_cases = Vec::new();
    let int_enums = int_enums(protocol);

    for item in &protocol.items {
        if let ProtocolItem::Interface(interface) = item {
            let name = &interface.name;
            let mod_name = format_ident!("{}", name);
            let handler_trait_name = format_ident!("{}Handler", snake_to_camel(name));

            interface_names.push(name.clone());
            handler_trait_names.push(quote! { #mod_name::#handler_trait_name });

            dispatch_request_cases.push(quote! {
                #name => #mod_name::dispatch_request(msg, handler, ctx),
            });
            dispatch_event_cases.push(quote! {
                #name => #mod_name::dispatch_event(msg, handler, ctx),
            });
            consume_event_cases.push(quote! {
                #name => #mod_name::consume_event(msg),
            });

            parts.push(generate_interface(interface, &int_enums));
        }
    }

    let protocol_name = format_ident!("{}", protocol.name);
    let delegation_macro = generate_delegation_macro(protocol);

    let expanded = quote! {
        pub mod #protocol_name {
            #![allow(non_upper_case_globals)]
            #![allow(unused_imports)]
            #![allow(non_camel_case_types)]
            #![allow(dead_code)]
            #![allow(unused_variables)]
            #![allow(clippy::match_single_binding)]
            #![allow(clippy::too_many_arguments)]
            #![allow(clippy::type_complexity)]
            #![allow(clippy::single_match)]
            #![allow(clippy::collapsible_match)]

            use std::os::fd::OwnedFd;
            use crate::wire::{WireMessage, MessageBuilder, Action, Outgoing, ProtocolError};
            use crate::state::{Context, Link};

            pub const ALLOWED_INTERFACES: &[&str] = &[
                #(#interface_names),*
            ];

            #(#parts)*

            pub trait ProtocolHandler:
                #(#handler_trait_names +)*
            {}

            pub fn dispatch_request<H: ProtocolHandler + ?Sized>(
                interface: &str,
                msg: &mut WireMessage,
                handler: &mut H,
                ctx: &mut Context,
            ) -> Result<Option<Outgoing>, ProtocolError> {
                ctx.last_sender_id = msg.sender_id;
                match interface {
                    #(#dispatch_request_cases)*
                    _ => Ok(None),
                }
            }

            pub fn dispatch_event<H: ProtocolHandler + ?Sized>(
                interface: &str,
                msg: &mut WireMessage,
                handler: &mut H,
                ctx: &mut Context,
            ) -> Result<Option<Outgoing>, ProtocolError> {
                ctx.last_sender_id = msg.sender_id;
                match interface {
                    #(#dispatch_event_cases)*
                    _ => Ok(None),
                }
            }

            pub fn consume_event(
                interface: &str,
                msg: &mut WireMessage,
            ) -> Result<(), ProtocolError> {
                match interface {
                    #(#consume_event_cases)*
                    _ => Err(ProtocolError::InvalidObjectId(msg.sender_id)),
                }
            }

            #delegation_macro
        }
    };

    // `quote` already emits valid Rust. Running rustfmt here is unnecessary
    // for generated build output, and large delegation macros can trigger
    // rustfmt source-map ICEs instead of producing a diagnostic. Keep code
    // generation deterministic and leave source formatting to handwritten
    // files.
    expanded.to_string()
}

/// Generate the cross-protocol routing table used by the proxy.
///
/// Individual protocol modules own message decoding and handler dispatch, but
/// the proxy should not need to repeat the interface-to-protocol mapping for
/// every message direction. The build script passes the complete protocol
/// list here so that the classifier and both dispatch entry points are derived
/// from the same source as the protocol modules themselves.
pub fn generate_routing(protocols: &[Protocol]) -> String {
    let mut family_variants = Vec::new();
    let mut handler_traits = Vec::new();
    let mut classify_arms = Vec::new();
    let mut dispatch_request_arms = Vec::new();
    let mut dispatch_event_arms = Vec::new();
    let mut special_request_arms = Vec::new();
    let mut special_event_arms = Vec::new();
    let mut consume_event_arms = Vec::new();
    let mut version_arms = Vec::new();
    // The proxy routes and versions an object by its interface name alone.
    let mut routed_interfaces = HashSet::new();

    for protocol in protocols {
        let protocol_name = format_ident!("{}", protocol.name);
        let family_name = format_ident!("{}", snake_to_camel(&protocol.name));

        family_variants.push(quote! { #family_name });
        // Aura objects are internal to Sommelier. Their event payload is
        // consumed by the proxy's explicit special case, and no aggregate
        // handler implementation exists for the generated Aura interfaces.
        // Keep the family in the classifier while leaving it out of the
        // generic handler bounds and dispatch arms.
        if protocol.name != "aura_shell" {
            handler_traits.push(quote! { #protocol_name::ProtocolHandler });
            dispatch_request_arms.push(quote! {
                ProtocolFamily::#family_name => #protocol_name::dispatch_request(interface, msg, handler, ctx),
            });
            dispatch_event_arms.push(quote! {
                ProtocolFamily::#family_name => #protocol_name::dispatch_event(interface, msg, handler, ctx),
            });
        } else {
            special_request_arms.push(quote! {
                ProtocolFamily::#family_name => Ok(None),
            });
            special_event_arms.push(quote! {
                ProtocolFamily::#family_name => Ok(None),
            });
        }
        consume_event_arms.push(quote! {
            ProtocolFamily::#family_name => #protocol_name::consume_event(interface, msg),
        });

        let interface_names: Vec<String> = protocol
            .items
            .iter()
            .filter_map(|item| {
                if let ProtocolItem::Interface(interface) = item {
                    Some(interface.name.clone())
                } else {
                    None
                }
            })
            .collect();

        if !interface_names.is_empty() {
            classify_arms.push(quote! {
                #(#interface_names)|* => Some(ProtocolFamily::#family_name),
            });
        }
        for name in &interface_names {
            assert!(
                routed_interfaces.insert(name.clone()),
                "interface `{name}` is defined by more than one protocol"
            );
            let interface = format_ident!("{}", name);
            version_arms.push(quote! {
                #name => Some(#protocol_name::#interface::VERSION),
            });
        }
    }

    let expanded = quote! {
        use crate::state::Context;
        use crate::wire::{Outgoing, ProtocolError, WireMessage};

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ProtocolFamily {
            #(#family_variants),*
        }

        pub trait AllProtocolHandlers:
            #(#handler_traits +)*
        {}

        impl<T: ?Sized> AllProtocolHandlers for T
        where
            T: #(#handler_traits +)*
        {}

        pub fn classify_interface(interface: &str) -> Option<ProtocolFamily> {
            match interface {
                #(#classify_arms)*
                _ => None,
            }
        }

        /// The newest version of `interface` in the vendored XML, the
        /// newest the generated code can decode, or `None` for an interface
        /// no vendored protocol defines.
        pub fn interface_version(interface: &str) -> Option<u32> {
            match interface {
                #(#version_arms)*
                _ => None,
            }
        }

        pub fn dispatch_request<H: AllProtocolHandlers + ?Sized>(
            family: ProtocolFamily,
            interface: &str,
            msg: &mut WireMessage,
            handler: &mut H,
            ctx: &mut Context,
        ) -> Result<Option<Outgoing>, ProtocolError> {
            match family {
                #(#dispatch_request_arms)*
                #(#special_request_arms)*
            }
        }

        pub fn dispatch_event<H: AllProtocolHandlers + ?Sized>(
            family: ProtocolFamily,
            interface: &str,
            msg: &mut WireMessage,
            handler: &mut H,
            ctx: &mut Context,
        ) -> Result<Option<Outgoing>, ProtocolError> {
            match family {
                #(#dispatch_event_arms)*
                #(#special_event_arms)*
            }
        }

        pub fn consume_event(
            family: ProtocolFamily,
            interface: &str,
            msg: &mut WireMessage,
        ) -> Result<(), ProtocolError> {
            match family {
                #(#consume_event_arms)*
            }
        }
    };

    expanded.to_string()
}

fn map_type(arg: &Arg) -> TokenStream {
    match arg.typ.as_str() {
        "int" => quote! { i32 },
        "uint" => quote! { u32 },
        // Keep wl_fixed_t in its raw signed 24.8 representation. A float
        // round-trip can lose low bits while proxying an otherwise opaque
        // protocol value.
        "fixed" => quote! { i32 },
        "string" if arg.allow_null.unwrap_or(false) => quote! { Option<String> },
        "string" => quote! { String },
        "object" => quote! { u32 },
        "new_id" => {
            if arg.interface.is_none() {
                quote! { (String, u32, u32) }
            } else {
                quote! { u32 }
            }
        }
        "array" => quote! { Vec<u8> },
        // A decoded message owns its descriptors; dropping it closes them.
        "fd" => quote! { std::os::fd::OwnedFd },
        _ => quote! { u32 },
    }
}

fn map_read_fn(arg: &Arg) -> TokenStream {
    match arg.typ.as_str() {
        "int" => quote! { msg.read_i32()? },
        "uint" => quote! { msg.read_u32()? },
        "fixed" => quote! { msg.read_fixed()? },
        "string" if arg.allow_null.unwrap_or(false) => quote! { msg.read_nullable_string()? },
        "string" => quote! { msg.read_string()? },
        "object" => quote! { msg.read_u32()? },
        "new_id" => {
            if arg.interface.is_none() {
                quote! { (msg.read_string()?, msg.read_u32()?, msg.read_u32()?) }
            } else {
                quote! { msg.read_u32()? }
            }
        }
        "array" => quote! { msg.read_array()? },
        "fd" => quote! { msg.read_fd()? },
        _ => quote! { msg.read_u32()? },
    }
}

fn map_write_fn(arg: &Arg, name: &Ident) -> TokenStream {
    match arg.typ.as_str() {
        "int" => quote! { builder.write_i32(#name) },
        "uint" | "object" => quote! { builder.write_u32(#name) },
        "new_id" => {
            if arg.interface.is_none() {
                quote! {
                    builder.write_string(&#name.0);
                    builder.write_u32(#name.1);
                    builder.write_u32(#name.2);
                }
            } else {
                quote! { builder.write_u32(#name) }
            }
        }
        "fixed" => quote! { builder.write_fixed(#name) },
        "string" if arg.allow_null.unwrap_or(false) => {
            quote! { builder.write_nullable_string(#name.as_deref()) }
        }
        "string" => quote! { builder.write_string(&#name) },
        "array" => quote! { builder.write_array(&#name) },
        "fd" => quote! { builder.write_fd(#name) },
        _ => quote! { builder.write_u32(#name) },
    }
}

fn generate_interface(interface: &Interface, int_enums: &HashSet<(String, String)>) -> TokenStream {
    let mod_name = format_ident!("{}", interface.name);
    let mut constant_names = ConstantNames::new(&interface.name);
    constant_names.claim("VERSION");
    let handler_trait_name = format_ident!("{}Handler", snake_to_camel(&interface.name));

    let mut request_opcodes = Vec::new();
    let mut event_opcodes = Vec::new();

    let mut request_variants = Vec::new();
    let mut event_variants = Vec::new();

    let mut request_decoders = Vec::new();
    let mut event_decoders = Vec::new();

    let mut request_opcode_match = Vec::new();
    let mut event_opcode_match = Vec::new();

    let mut handler_methods = Vec::new();
    let mut dispatch_request_arms = Vec::new();
    let mut dispatch_event_arms = Vec::new();

    let mut enums = Vec::new();
    let mut request_encoders = Vec::new();
    let mut event_encoders = Vec::new();

    let mut req_idx = 0u16;
    let mut evt_idx = 0u16;

    let mut requests = Vec::new();
    let mut events = Vec::new();

    for item in &interface.items {
        match item {
            InterfaceItem::Request(req) => {
                requests.push(req);
                let opcode_name = format_ident!("REQ_{}", req.name.to_uppercase());
                constant_names.claim_opcode(&opcode_name);
                request_opcodes.push(quote! { pub const #opcode_name: u16 = #req_idx; });
                request_opcodes.push(generate_since_constant(&opcode_name, req.since));
                request_encoders.push(generate_encoder(&interface.name, req, &opcode_name));

                let var_name = format_ident!("{}", snake_to_camel(&req.name));
                let fields = generate_fields(&req.items);
                request_variants.push(quote! { #var_name { #fields } });

                let decodes = generate_reads(&req.items);
                let field_names = generate_field_names(&req.items);
                let request_validations = generate_request_validations(&req.items);
                let request_version_validation = generate_version_validation(req.since, false);
                request_decoders.push(quote! {
                    #req_idx => {
                        #decodes
                        Ok(Request::#var_name { #field_names })
                    }
                });

                request_opcode_match.push(quote! {
                    Request::#var_name { .. } => #req_idx,
                });

                // Handler method
                let method_name = format_ident!("on_{}", req.name);
                let method_args = generate_handler_args(&req.items);
                handler_methods.push(quote! {
                    fn #method_name(&mut self, _ctx: &mut Context, #method_args) -> Action {
                        Action::Forward
                    }
                });

                // Dispatch arm
                let mut mapping_and_writing = Vec::new();
                let mut handler_call_args = Vec::new();
                let mut field_names_list = Vec::new();

                for arg_item in &req.items {
                    if let MessageItem::Arg(arg) = arg_item {
                        let name = sanitize_ident(&arg.name);
                        let arg_name_str = &arg.name;
                        field_names_list.push(name.clone());

                        if arg.typ == "string"
                            || arg.typ == "array"
                            || (arg.typ == "new_id" && arg.interface.is_none())
                        {
                            handler_call_args.push(quote! { &#name });
                        } else if arg.typ == "fd" {
                            handler_call_args.push(quote! { std::os::fd::AsFd::as_fd(&#name) });
                        } else {
                            handler_call_args.push(quote! { #name });
                        }

                        match arg.typ.as_str() {
                            // The pre-handler validation translated the
                            // object (see `generate_request_validations`).
                            "object" => {
                                let host = translated_object_ident(arg);
                                if arg.allow_null.unwrap_or(false) {
                                    mapping_and_writing.push(quote! {
                                        builder.write_u32(#host);
                                    });
                                } else {
                                    let req_name_str = &req.name;
                                    mapping_and_writing.push(quote! {
                                        let Link::Host(host_id) = #host else {
                                            log::debug!(
                                                "Dropping request {} naming a local object as non-nullable argument {}",
                                                #req_name_str,
                                                #arg_name_str
                                            );
                                            return Ok(None);
                                        };
                                        builder.write_u32(host_id);
                                    });
                                }
                            }
                            "new_id" => {
                                if let Some(ref interface_name) = arg.interface {
                                    // A child object takes its parent's
                                    // version on each side. No local is
                                    // bound, so none can shadow a later
                                    // argument.
                                    mapping_and_writing.push(quote! {
                                        builder.write_u32(ctx.shadow_table.create_paired(
                                            #name,
                                            #interface_name,
                                            ctx.shadow_table.guest_object_version_or_unknown(msg.sender_id),
                                            ctx.shadow_table.host_object_version_or_unknown(host_sender_id),
                                        ));
                                    });
                                } else {
                                    mapping_and_writing.push(quote! {
                                        builder.write_string(&#name.0);
                                        builder.write_u32(#name.1);
                                        builder.write_u32(
                                            ctx.shadow_table.create_paired(#name.2, &#name.0, #name.1, #name.1),
                                        );
                                    });
                                }
                            }
                            _ => {
                                let write_call = map_write_fn(arg, &name);
                                mapping_and_writing.push(quote! { #write_call; });
                            }
                        }
                    }
                }

                // A local object's destructor completes in the proxy, whether
                // its handler forwards or drops the request.
                let (destructor_cleanup, local_destructor_cleanup, dropped_destructor_cleanup) =
                    if req.msg_type.as_deref() == Some("destructor") {
                        (
                            quote! { ctx.guest_destroyed(msg.sender_id); },
                            quote! { crate::handler::display::queue_local_delete_id(ctx, msg.sender_id); },
                            quote! {
                                if sender == Link::Local {
                                    crate::handler::display::queue_local_delete_id(ctx, msg.sender_id);
                                }
                            },
                        )
                    } else {
                        (quote! {}, quote! {}, quote! {})
                    };

                dispatch_request_arms.push(quote! {
                    Request::#var_name { #(#field_names_list),* } => {
                        #request_version_validation
                        #(#request_validations)*
                        let handler_action = handler.#method_name(ctx, #(#handler_call_args),*);
                        if handler_action == Action::Forward {
                            // Check the sender before mapping any new_id: a
                            // dropped request must not consume or map an ID.
                            let Link::Host(host_sender_id) = sender else {
                                log::debug!(
                                    "Dropping request from local object {}",
                                    msg.sender_id
                                );
                                #local_destructor_cleanup
                                return Ok(None);
                            };
                            #[allow(unused_mut)]
                            let mut builder = MessageBuilder::new();
                            #(#mapping_and_writing)*
                            let outgoing = builder.try_build(host_sender_id, #req_idx)?;
                            #destructor_cleanup
                            return Ok(Some(outgoing));
                        }
                        #dropped_destructor_cleanup
                    }
                });

                req_idx += 1;
            }
            InterfaceItem::Event(evt) => {
                events.push(evt);
                let opcode_name = format_ident!("EVT_{}", evt.name.to_uppercase());
                constant_names.claim_opcode(&opcode_name);
                event_opcodes.push(quote! { pub const #opcode_name: u16 = #evt_idx; });
                event_opcodes.push(generate_since_constant(&opcode_name, evt.since));
                event_encoders.push(generate_encoder(&interface.name, evt, &opcode_name));

                let var_name = format_ident!("{}", snake_to_camel(&evt.name));
                let fields = generate_fields(&evt.items);
                event_variants.push(quote! { #var_name { #fields } });

                let decodes = generate_reads(&evt.items);
                let field_names = generate_field_names(&evt.items);
                let event_validations = generate_event_validations(&evt.items);
                let event_version_validation = generate_version_validation(evt.since, true);
                event_decoders.push(quote! {
                    #evt_idx => {
                        #decodes
                        Ok(Event::#var_name { #field_names })
                    },
                });

                event_opcode_match.push(quote! {
                    Event::#var_name { .. } => #evt_idx,
                });

                // Handler method for event
                let method_name = format_ident!("on_{}", evt.name);
                let method_args = generate_handler_args(&evt.items);
                handler_methods.push(quote! {
                    fn #method_name(&mut self, _ctx: &mut Context, #method_args) -> Action {
                        Action::Forward
                    }
                });

                // Dispatch arm for event (Host -> Guest)
                let mut mapping_and_writing = Vec::new();
                let mut handler_call_args = Vec::new();
                let mut field_names_list = Vec::new();

                for arg_item in &evt.items {
                    if let MessageItem::Arg(arg) = arg_item {
                        let name = sanitize_ident(&arg.name);
                        field_names_list.push(name.clone());

                        if arg.typ == "string"
                            || arg.typ == "array"
                            || (arg.typ == "new_id" && arg.interface.is_none())
                        {
                            handler_call_args.push(quote! { &#name });
                        } else if arg.typ == "fd" {
                            handler_call_args.push(quote! { std::os::fd::AsFd::as_fd(&#name) });
                        } else {
                            handler_call_args.push(quote! { #name });
                        }

                        match arg.typ.as_str() {
                            // The pre-handler validation translated the
                            // object (see `generate_event_validations`).
                            "object" if arg.interface.is_some() => {
                                let guest = translated_object_ident(arg);
                                mapping_and_writing.push(quote! {
                                    builder.write_u32(#guest);
                                });
                            }
                            "object" => {
                                assert!(
                                    !arg.allow_null.unwrap_or(false),
                                    "event argument {} names a nullable object of no interface, which no vendored protocol has",
                                    arg.name
                                );
                                let evt_name_str = &evt.name;
                                let arg_name_str = &arg.name;
                                mapping_and_writing.push(quote! {
                                    let Some(guest_id) = ctx.shadow_table.get_guest_id(#name) else {
                                        log::debug!(
                                            "Dropping event {} naming an object with no guest counterpart as argument {}",
                                            #evt_name_str,
                                            #arg_name_str
                                        );
                                        return Ok(None);
                                    };
                                    builder.write_u32(guest_id);
                                });
                            }
                            "new_id" => {
                                if let Some(ref interface_name) = arg.interface {
                                    // A child object takes its parent's
                                    // version, bound to no local that could
                                    // shadow a later argument.
                                    mapping_and_writing.push(quote! {
                                        builder.write_u32(ctx.shadow_table.adopt_host_created(
                                            #name,
                                            #interface_name,
                                            ctx.shadow_table.host_object_version_or_unknown(msg.sender_id),
                                        ));
                                    });
                                } else {
                                    mapping_and_writing.push(quote! {
                                        builder.write_u32(#name);
                                    });
                                }
                            }
                            _ => {
                                let write_call = map_write_fn(arg, &name);
                                mapping_and_writing.push(quote! { #write_call; });
                            }
                        }
                    }
                }

                let is_destructor = evt.msg_type.as_deref() == Some("destructor");
                let destructor_cleanup = if is_destructor {
                    quote! {
                        ctx.guest_destroyed(guest_sender_id);
                    }
                } else {
                    quote! {}
                };

                dispatch_event_arms.push(quote! {
                    Event::#var_name { #(#field_names_list),* } => {
                        #event_version_validation
                        #(#event_validations)*
                        // Capture the paired guest sender before invoking the
                        // handler. A custom handler may retire the mapping
                        // while consuming or translating an event; forwarding
                        // must still use the ID that addressed the event on
                        // the wire, and destructor cleanup must remove that
                        // same object rather than looking it up again.
                        let mapped_guest_sender_id = ctx.shadow_table.get_guest_id(msg.sender_id);
                        if handler.#method_name(ctx, #(#handler_call_args),*) == Action::Forward {
                            // Check the sender before mapping any new_id: a
                            // dropped event must not consume a guest server
                            // ID, since libwayland-client refuses gaps.
                            let Some(guest_sender_id) = mapped_guest_sender_id else {
                                log::debug!(
                                    "Dropping event from unmapped sender {}",
                                    msg.sender_id
                                );
                                return Ok(None);
                            };
                            #[allow(unused_mut)]
                            let mut builder = MessageBuilder::new();
                            #(#mapping_and_writing)*
                            let outgoing = builder.try_build(guest_sender_id, #evt_idx)?;
                            #destructor_cleanup
                            return Ok(Some(outgoing));
                        }
                    }
                });

                evt_idx += 1;
            }
            InterfaceItem::Enum(enumeration) => {
                let is_int =
                    int_enums.contains(&(interface.name.clone(), enumeration.name.clone()));
                enums.push(generate_enum(enumeration, is_int, &mut constant_names));
            }
            InterfaceItem::Description(_) => {}
        }
    }

    // The XML owns the newest version of each interface, as it owns each
    // message's since-version; nothing newer can be decoded.
    let interface_version = interface.version;
    let request_test_impls = generate_test_impls(&format_ident!("Request"), &requests);
    let event_test_impls = generate_test_impls(&format_ident!("Event"), &events);
    quote! {
        pub mod #mod_name {
            use super::*;

            pub const VERSION: u32 = #interface_version;
            #(#request_opcodes)*
            #(#event_opcodes)*
            #(#enums)*

            /// Encoders of the requests a client sends to this interface.
            pub mod req {
                use super::*;
                #(#request_encoders)*
            }

            /// Encoders of the events a compositor sends from this interface.
            pub mod evt {
                use super::*;
                #(#event_encoders)*
            }

            #[derive(Debug)]
            pub enum Request {
                #(#request_variants),*
            }

            #request_test_impls

            impl Request {
                pub fn from_wire(msg: &mut WireMessage) -> Result<Self, ProtocolError> {
                    match msg.opcode {
                        #(#request_decoders)*
                        _ => Err(ProtocolError::UnknownOpcode(msg.opcode))
                    }
                }

                pub fn opcode(&self) -> u16 {
                    match self {
                        #(#request_opcode_match)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!()
                    }
                }
            }

            #[derive(Debug)]
            pub enum Event {
                #(#event_variants),*
            }

            #event_test_impls

            impl Event {
                pub fn from_wire(msg: &mut WireMessage) -> Result<Self, ProtocolError> {
                    match msg.opcode {
                        #(#event_decoders)*
                        _ => Err(ProtocolError::UnknownOpcode(msg.opcode))
                    }
                }

                pub fn opcode(&self) -> u16 {
                    match self {
                        #(#event_opcode_match)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!()
                    }
                }
            }

            pub trait #handler_trait_name {
                #(#handler_methods)*
            }

            pub fn dispatch_request<H: #handler_trait_name + ?Sized>(
                msg: &mut WireMessage,
                handler: &mut H,
                ctx: &mut Context,
            ) -> Result<Option<Outgoing>, ProtocolError> {
                // The sender comes first, as in libwayland-server: a request
                // from an object that is gone is reported as such, whatever
                // its opcode and arguments.
                let Some(sender) = ctx.shadow_table.request_sender(msg.sender_id) else {
                    return Err(ProtocolError::InvalidSender(msg.sender_id));
                };
                let req = Request::from_wire(msg)?;
                match req {
                    #(#dispatch_request_arms)*
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
                Ok(None)
            }

            pub fn dispatch_event<H: #handler_trait_name + ?Sized>(
                msg: &mut WireMessage,
                handler: &mut H,
                ctx: &mut Context,
            ) -> Result<Option<Outgoing>, ProtocolError> {
                // The proxy dispatches an event by its sender's interface
                // (`ShadowTable::get_host_interface`), so the sender is a
                // live host object.
                let evt = Event::from_wire(msg)?;
                match evt {
                    #(#dispatch_event_arms)*
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
                Ok(None)
            }

            pub fn consume_event(msg: &mut WireMessage) -> Result<(), ProtocolError> {
                let _ = Event::from_wire(msg)?;
                Ok(())
            }
        }
    }
}

/// Generate what tests need of a decoded message type, `Request` or
/// `Event`: equality, which compares a descriptor by the file it refers
/// to (`test_support::same_file`), since descriptors have no equality of
/// their own, and `new_object`, the ID and interface of the object a
/// message creates, which lets a test peer follow the objects it is told
/// about as a Wayland client library does. No vendored message creates
/// more than one object (`test_no_message_creates_two_objects`).
fn generate_test_impls(enum_name: &Ident, messages: &[&Message]) -> TokenStream {
    let mut eq_arms = Vec::new();
    let mut new_object_arms = Vec::new();
    for message in messages {
        let variant = format_ident!("{}", snake_to_camel(&message.name));
        let mut fields = Vec::new();
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut comparisons = Vec::new();
        for arg in args(&message.items) {
            let field = sanitize_ident(&arg.name);
            let l = format_ident!("l_{}", arg.name);
            let r = format_ident!("r_{}", arg.name);
            comparisons.push(if arg.typ == "fd" {
                quote! { crate::test_support::same_file(#l, #r) }
            } else {
                quote! { #l == #r }
            });
            fields.push(field);
            left.push(l);
            right.push(r);
        }
        eq_arms.push(quote! {
            (Self::#variant { #(#fields: #left),* }, Self::#variant { #(#fields: #right),* }) => {
                true #(&& #comparisons)*
            }
        });
        if let Some(arg) = args(&message.items).find(|arg| arg.typ == "new_id") {
            let field = sanitize_ident(&arg.name);
            new_object_arms.push(match &arg.interface {
                Some(interface) => quote! {
                    Self::#variant { #field, .. } => Some((*#field, #interface)),
                },
                // `wl_registry.bind` names the interface it creates.
                None => quote! {
                    Self::#variant { #field, .. } => Some((#field.2, #field.0.as_str())),
                },
            });
        }
    }
    quote! {
        #[cfg(test)]
        impl PartialEq for #enum_name {
            // A message type with one argument-less message compares by
            // literals alone.
            #[allow(clippy::match_like_matches_macro)]
            fn eq(&self, other: &Self) -> bool {
                match (self, other) {
                    #(#eq_arms)*
                    #[allow(unreachable_patterns)]
                    _ => false,
                }
            }
        }

        #[cfg(test)]
        impl #enum_name {
            /// The ID and interface of the object this message creates.
            pub fn new_object(&self) -> Option<(u32, &str)> {
                match self {
                    #(#new_object_arms)*
                    #[allow(unreachable_patterns)]
                    _ => None,
                }
            }
        }
    }
}

fn generate_delegation_macro(protocol: &Protocol) -> TokenStream {
    let mut dispatch_arms = Vec::new();
    let mut entry_point_calls = Vec::new();
    let protocol_name = format_ident!("{}", protocol.name);

    for item in &protocol.items {
        if let ProtocolItem::Interface(interface) = item {
            let iface_name = &interface.name;
            let iface_ident = format_ident!("{}", iface_name);
            let mod_name = format_ident!("{}", iface_name);
            let handler_trait_name = format_ident!("{}Handler", snake_to_camel(iface_name));

            // Entry point call
            entry_point_calls.push(quote! {
                $crate::protocols::#protocol_name::impl_sommelier_delegates!(@dispatch $target, #iface_ident, [ $($iface : $field,)* ]);
            });

            let mut method_impls = Vec::new();
            for item in &interface.items {
                let (name, args_items) = match item {
                    InterfaceItem::Request(req) => (&req.name, &req.items),
                    InterfaceItem::Event(evt) => (&evt.name, &evt.items),
                    _ => continue,
                };

                let method_name = format_ident!("on_{}", name);
                let sig_args = generate_handler_args(args_items);
                let call_args = generate_forwarding_call_args(args_items);

                method_impls.push(quote! {
                     fn #method_name(&mut self, ctx: &mut $crate::state::Context, #sig_args) -> $crate::wire::Action {
                         self.$field.#method_name(ctx, #call_args)
                     }
                 });
            }

            dispatch_arms.push(quote! {
                (@dispatch $target:ty, #iface_ident, [ #iface_ident : $field:ident, $($rest:tt)* ]) => {
                    impl $crate::protocols::#protocol_name::#mod_name::#handler_trait_name for $target {
                        #(#method_impls)*
                    }
                };
                (@dispatch $target:ty, #iface_ident, [ $other:ident : $field:ident, $($rest:tt)* ]) => {
                    $crate::protocols::#protocol_name::impl_sommelier_delegates!(@dispatch $target, #iface_ident, [ $($rest)* ]);
                };
                (@dispatch $target:ty, #iface_ident, []) => {
                    impl $crate::protocols::#protocol_name::#mod_name::#handler_trait_name for $target {}
                };
            });
        }
    }

    quote! {
        macro_rules! impl_sommelier_delegates {
            ($target:ty, { $($iface:ident : $field:ident),* }) => {
                #( #entry_point_calls )*
            };
            #(#dispatch_arms)*
        }
        pub(crate) use impl_sommelier_delegates;
    }
}

fn generate_reads(items: &[MessageItem]) -> TokenStream {
    let mut statements = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            let name = sanitize_ident(&arg.name);
            let read_call = map_read_fn(arg);
            statements.push(quote! { let #name = #read_call; });
        }
    }
    quote! { #(#statements)* }
}

/// Generate `<OPCODE>_SINCE`, the object version that introduced a message.
///
/// The protocol XML is the single owner of these thresholds; hand-written
/// handlers compare negotiated versions against the generated constant
/// instead of copying the number. A message without a `since` attribute
/// exists from version 1.
fn generate_since_constant(opcode_name: &Ident, since: Option<u32>) -> TokenStream {
    let since_name = format_ident!("{}_SINCE", opcode_name);
    let since = since.unwrap_or(1);
    quote! { pub const #since_name: u32 = #since; }
}

/// Generate the encoder of one message: `req::<name>` or `evt::<name>`.
///
/// The encoder takes the sender's ID and then the message's arguments in
/// XML order, so the compiler checks the opcode, argument order and
/// argument types of every message the proxy builds itself. A message that
/// carries a string or an array can exceed the wire's 16-bit length and
/// returns a `Result`; any other cannot and returns the message directly.
/// The message owns its descriptors, as `OwnedFd`s, so one that cannot be
/// built or is never sent closes them.
fn generate_encoder(interface: &str, message: &Message, opcode: &Ident) -> TokenStream {
    let name = sanitize_ident(&message.name);
    let mut params = Vec::new();
    let mut writes = Vec::new();
    let mut fds = Vec::new();
    let mut fallible = false;
    for arg in args(&message.items) {
        let arg_name = sanitize_ident(&arg.name);
        let nullable = arg.allow_null.unwrap_or(false);
        // Encoders are for messages the proxy builds itself, whose IDs it
        // chose, so a null required object is a proxy bug. Host data is not
        // checked here: forwarding translates and validates it on its own
        // path, and must not pass it through an encoder unchecked.
        if matches!(arg.typ.as_str(), "object" | "new_id") && !nullable {
            let null = format!("{}.{}: {} is null", interface, message.name, arg.name);
            writes.push(quote! { debug_assert_ne!(#arg_name, 0, #null); });
        }
        match arg.typ.as_str() {
            "int" | "fixed" => {
                params.push(quote! { #arg_name: i32 });
                writes.push(quote! { builder.write_i32(#arg_name); });
            }
            "uint" => {
                params.push(quote! { #arg_name: u32 });
                writes.push(quote! { builder.write_u32(#arg_name); });
            }
            "object" | "new_id" if nullable => {
                params.push(quote! { #arg_name: Option<u32> });
                writes.push(quote! { builder.write_u32(#arg_name.unwrap_or(0)); });
            }
            // `wl_registry.bind` names the interface and version of the
            // object it creates.
            "new_id" if arg.interface.is_none() => {
                fallible = true;
                params.push(quote! { interface: &str, version: u32, #arg_name: u32 });
                writes.push(quote! {
                    builder.write_string(interface);
                    builder.write_u32(version);
                    builder.write_u32(#arg_name);
                });
            }
            "object" | "new_id" => {
                params.push(quote! { #arg_name: u32 });
                writes.push(quote! { builder.write_u32(#arg_name); });
            }
            "string" if nullable => {
                fallible = true;
                params.push(quote! { #arg_name: Option<&str> });
                writes.push(quote! { builder.write_nullable_string(#arg_name); });
            }
            "string" => {
                fallible = true;
                params.push(quote! { #arg_name: &str });
                writes.push(quote! { builder.write_string(#arg_name); });
            }
            "array" => {
                fallible = true;
                params.push(quote! { #arg_name: &[u8] });
                writes.push(quote! { builder.write_array(#arg_name); });
            }
            "fd" => {
                params.push(quote! { #arg_name: OwnedFd });
                fds.push(quote! { #arg_name });
            }
            other => panic!(
                "{}.{}: unknown argument type {}",
                interface, message.name, other
            ),
        }
    }
    let fds = if fds.is_empty() {
        quote! { Vec::new() }
    } else {
        quote! { vec![#(#fds),*] }
    };
    let (output, message) = if fallible {
        (
            quote! { Result<Outgoing, ProtocolError> },
            quote! { Ok((builder.try_build_message(sender_id, super::#opcode)?, #fds)) },
        )
    } else {
        (
            quote! { Outgoing },
            quote! { (builder.build_message(sender_id, super::#opcode), #fds) },
        )
    };
    // A dropped encoder result loses the message without a trace. The
    // reason keeps Clippy from calling the attribute redundant on a
    // `Result`.
    quote! {
        #[must_use = "a message that is not queued is never sent"]
        pub fn #name(sender_id: u32, #(#params),*) -> #output {
            #[allow(unused_mut)]
            let mut builder = MessageBuilder::new();
            #(#writes)*
            #message
        }
    }
}

/// The flat constant names of one interface module.
///
/// Opcodes, their `_SINCE` versions, `VERSION` and enum entries share one
/// namespace, so an enum named after a message could produce a name twice.
/// Generation stops instead of emitting a module that does not compile, or
/// one where the name means something else than its XML.
struct ConstantNames<'a> {
    interface: &'a str,
    names: HashSet<String>,
}

impl<'a> ConstantNames<'a> {
    fn new(interface: &'a str) -> Self {
        Self {
            interface,
            names: HashSet::new(),
        }
    }

    fn claim(&mut self, name: &str) {
        assert!(
            self.names.insert(name.to_string()),
            "{} generates the constant {} twice",
            self.interface,
            name
        );
    }

    fn claim_opcode(&mut self, opcode_name: &Ident) {
        self.claim(&opcode_name.to_string());
        self.claim(&format!("{}_SINCE", opcode_name));
    }
}

/// The enums that an `int` argument refers to, as `(interface, enum)`.
///
/// An enum's constants take the type of the arguments that carry it, so a
/// handler compares them with its decoded argument without a cast. Wayland
/// declares no enum type of its own; every enum that no `int` argument
/// refers to (bitfields, error codes, array entries) is `u32`. Only
/// arguments in the same protocol file count, since `generate` sees one
/// file at a time.
fn int_enums(protocol: &Protocol) -> HashSet<(String, String)> {
    let mut int_enums = HashSet::new();
    for item in &protocol.items {
        let ProtocolItem::Interface(interface) = item else {
            continue;
        };
        for item in &interface.items {
            let (InterfaceItem::Request(message) | InterfaceItem::Event(message)) = item else {
                continue;
            };
            for arg in args(&message.items) {
                let (Some(enum_name), "int") = (&arg.enum_name, arg.typ.as_str()) else {
                    continue;
                };
                let (enum_interface, enum_name) = enum_name
                    .split_once('.')
                    .unwrap_or((&interface.name, enum_name));
                int_enums.insert((enum_interface.to_string(), enum_name.to_string()));
            }
        }
    }
    int_enums
}

/// Generate one constant per enum entry, named `<ENUM>_<ENTRY>` inside the
/// interface module (libwayland's `<INTERFACE>_<ENUM>_<ENTRY>`), and
/// `<enum>_since`, the interface version that introduced a value, or `None`
/// for a value the XML does not define. Two entries with one value make one
/// `_since` arm, with the older version. For a bitfield, `_since` knows each
/// single flag, not their combinations.
fn generate_enum(
    enumeration: &Enum,
    is_int: bool,
    constant_names: &mut ConstantNames,
) -> TokenStream {
    let value_type = if is_int {
        quote! { i32 }
    } else {
        quote! { u32 }
    };
    let enum_since = enumeration.since.unwrap_or(1);
    let mut constants = Vec::new();
    let mut since_by_value = BTreeMap::new();
    for item in &enumeration.items {
        let EnumItem::Entry(entry) = item else {
            continue;
        };
        let name = format!("{}_{}", enumeration.name, entry.name).to_uppercase();
        constant_names.claim(&name);
        let name = format_ident!("{}", name);
        let raw_value = enum_value(&entry.value);
        let value = if is_int {
            let value = i32::try_from(raw_value).unwrap_or_else(|_| {
                panic!("{}.{} does not fit an int", enumeration.name, entry.name)
            });
            quote! { #value }
        } else {
            quote! { #raw_value }
        };
        let since = entry.since.unwrap_or(enum_since);
        constants.push(quote! { pub const #name: #value_type = #value; });
        let (_, oldest) = since_by_value.entry(raw_value).or_insert((value, since));
        *oldest = since.min(*oldest);
    }
    let since_arms = since_by_value
        .into_values()
        .map(|(value, since)| quote! { #value => Some(#since), });
    let since_fn = format_ident!("{}_since", enumeration.name);
    quote! {
        #(#constants)*
        pub fn #since_fn(value: #value_type) -> Option<u32> {
            match value {
                #(#since_arms)*
                _ => None,
            }
        }
    }
}

/// Parse an enum entry value: decimal, `0x` hexadecimal, or `<a> << <b>`.
fn enum_value(value: &str) -> u32 {
    let parse = |number: &str| {
        let number = number.trim();
        match number.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => number.parse(),
        }
        .unwrap_or_else(|_| panic!("invalid enum value {value:?}"))
    };
    match value.split_once("<<") {
        Some((base, shift)) => parse(base)
            .checked_shl(parse(shift))
            .unwrap_or_else(|| panic!("invalid enum value {value:?}")),
        None => parse(value),
    }
}

fn args(items: &[MessageItem]) -> impl Iterator<Item = &Arg> {
    items.iter().filter_map(|item| match item {
        MessageItem::Arg(arg) => Some(arg),
        MessageItem::Description(_) => None,
    })
}

/// Generate the object-version guard required by Wayland's `since`
/// attribute. Requests use the guest object's negotiated version, while
/// events use the host object's version.
fn generate_version_validation(since: Option<u32>, host_to_guest: bool) -> TokenStream {
    let Some(required) = since else {
        return quote! {};
    };

    let lookup = if host_to_guest {
        quote! { ctx.shadow_table.host_object_version(msg.sender_id) }
    } else {
        quote! { ctx.shadow_table.guest_object_version(msg.sender_id) }
    };

    quote! {
        let object_version = #lookup
            .ok_or(ProtocolError::InvalidSender(msg.sender_id))?;
        if object_version < #required {
            return Err(ProtocolError::UnsupportedVersion {
                object_id: msg.sender_id,
                required: #required,
                actual: object_version,
            });
        }
    }
}

/// Generate pre-handler validation for IDs supplied by a guest request.
///
/// Wayland clients own the IDs in requests. A repeated, null, or
/// server-reserved ID is a protocol error and must not be allowed to overwrite
/// an existing shadow-table entry. Each object argument is validated and
/// translated in one lookup, bound to [`translated_object_ident`]: a nullable
/// one to the host ID to send, else to its [`Link`].
fn generate_request_validations(items: &[MessageItem]) -> Vec<TokenStream> {
    let mut validations = Vec::new();
    for arg in args(items) {
        let name = sanitize_ident(&arg.name);
        match arg.typ.as_str() {
            "new_id" => {
                let id = if arg.interface.is_none() {
                    quote! { #name.2 }
                } else {
                    quote! { #name }
                };
                validations.push(quote! {
                    if !ctx.shadow_table.is_guest_id_available(#id) {
                        return Err(ProtocolError::InvalidObjectId(#id));
                    }
                });
            }
            "object" => {
                let Some(interface) = &arg.interface else {
                    panic!(
                        "request argument {} names an object of no interface, which no vendored protocol has",
                        arg.name
                    );
                };
                let host = translated_object_ident(arg);
                let link = quote! {
                    ctx.shadow_table
                        .request_object(#name, #interface)
                        .ok_or(ProtocolError::InvalidObjectId(#name))?
                };
                validations.push(if arg.allow_null.unwrap_or(false) {
                    quote! {
                        let #host = if #name == 0 {
                            0
                        } else {
                            match #link {
                                Link::Host(host_id) => host_id,
                                // A local object, such as an inert output,
                                // has no host counterpart: the host gets
                                // null, as for no object.
                                Link::Local => 0,
                            }
                        };
                    }
                } else {
                    quote! { let #host = #link; }
                });
            }
            _ => {}
        }
    }
    validations
}

/// Generate validation for IDs carried by a host event. A host-generated
/// object must have a fresh server ID. Each object argument of an interface
/// must name a live object with a guest counterpart, or the event is
/// dropped; it is validated and translated to the guest ID in one lookup,
/// bound to [`translated_object_ident`]. An object of no interface is
/// resolved only when the event is forwarded.
fn generate_event_validations(items: &[MessageItem]) -> Vec<TokenStream> {
    let mut validations = Vec::new();
    for arg in args(items) {
        let name = sanitize_ident(&arg.name);
        match arg.typ.as_str() {
            "new_id" if arg.interface.is_some() => {
                validations.push(quote! {
                    if !ctx.shadow_table.accepts_host_new_id(#name) {
                        return Err(ProtocolError::InvalidObjectId(#name));
                    }
                });
            }
            // An object of no interface (`wl_display.error`'s object) may
            // name any object, including one only the proxy uses, so its
            // handler must see the event; only forwarding resolves it.
            "object" => {
                let Some(interface) = &arg.interface else {
                    continue;
                };
                let guest = translated_object_ident(arg);
                let lookup = quote! { ctx.shadow_table.event_object(#name, #interface) };
                validations.push(if arg.allow_null.unwrap_or(false) {
                    quote! {
                        let #guest = if #name == 0 {
                            0
                        } else {
                            let Some(guest_id) = #lookup else {
                                return Ok(None);
                            };
                            guest_id
                        };
                    }
                } else {
                    quote! {
                        let Some(#guest) = #lookup else {
                            return Ok(None);
                        };
                    }
                });
            }
            _ => {}
        }
    }
    validations
}

/// The binding that holds an object argument translated by its message's
/// validation (see [`generate_request_validations`] and
/// [`generate_event_validations`]).
fn translated_object_ident(arg: &Arg) -> Ident {
    format_ident!("translated_{}", arg.name)
}

fn generate_field_names(items: &[MessageItem]) -> TokenStream {
    let mut names = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            names.push(sanitize_ident(&arg.name));
        }
    }
    quote! { #(#names),* }
}

fn generate_fields(items: &[MessageItem]) -> TokenStream {
    let mut fields = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            let name = sanitize_ident(&arg.name);
            let ty = map_type(arg);
            fields.push(quote! { #name: #ty });
        }
    }
    quote! { #(#fields),* }
}

/// The parameters of a handler method. Types are fully qualified, because
/// the delegation macro expands them in the handler's module.
///
/// A handler borrows the message's descriptors: the dispatcher owns them,
/// forwards them or closes them, and a handler that keeps one clones it.
fn generate_handler_args(items: &[MessageItem]) -> TokenStream {
    let mut args = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            let name = format_ident!("_{}", sanitize_ident(&arg.name));
            let ty = map_type(arg);
            if arg.typ == "string" {
                args.push(quote! { #name: &#ty });
            } else if arg.typ == "array" {
                args.push(quote! { #name: &[u8] });
            } else if arg.typ == "new_id" && arg.interface.is_none() {
                args.push(quote! { #name: &(String, u32, u32) });
            } else if arg.typ == "fd" {
                args.push(quote! { #name: std::os::fd::BorrowedFd<'_> });
            } else {
                args.push(quote! { #name: #ty });
            }
        }
    }
    quote! { #(#args),* }
}

fn generate_forwarding_call_args(items: &[MessageItem]) -> TokenStream {
    let mut args = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            let name = format_ident!("_{}", sanitize_ident(&arg.name));
            args.push(quote! { #name });
        }
    }
    quote! { #(#args),* }
}

fn sanitize_ident(name: &str) -> Ident {
    match name {
        "type" | "move" | "loop" | "box" | "crate" | "match" | "fn" | "impl" | "trait" | "pub"
        | "mod" | "use" | "self" | "super" | "in" | "where" | "for" | "while" | "if" | "else"
        | "let" | "struct" | "enum" | "const" | "static" | "mut" | "ref" | "extern" | "unsafe"
        | "return" | "break" | "continue" | "async" | "await" | "dyn" | "abstract" | "become"
        | "do" | "final" | "macro" | "override" | "priv" | "typeof" | "unsized" | "virtual"
        | "yield" | "try" => format_ident!("r#{}", name),
        _ => format_ident!("{}", name),
    }
}

fn snake_to_camel(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut c = word.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}
