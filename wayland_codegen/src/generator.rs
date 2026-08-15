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

pub fn generate(protocol: &Protocol) -> String {
    let mut parts = Vec::new();
    let mut interface_names = Vec::new();
    let mut handler_trait_names = Vec::new();
    let mut dispatch_request_cases = Vec::new();
    let mut dispatch_event_cases = Vec::new();
    let mut consume_event_cases = Vec::new();

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

            parts.push(generate_interface(interface));
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

            use std::os::unix::io::RawFd;
            use crate::wire::{WireMessage, MessageBuilder, Action, ProtocolError};
            use crate::state::Context;

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
            ) -> Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError> {
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
            ) -> Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError> {
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
        "fd" => quote! { RawFd },
        _ => quote! { u32 },
    }
}

fn map_type_fq(arg: &Arg) -> TokenStream {
    match arg.typ.as_str() {
        "fd" => quote! { std::os::unix::io::RawFd },
        // For other types, map_type returns primitives or standard library types (String, Vec)
        // or tuples of them, so it's safe to reuse.
        _ => map_type(arg),
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

fn generate_interface(interface: &Interface) -> TokenStream {
    let mod_name = format_ident!("{}", interface.name);
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

    let mut req_idx = 0u16;
    let mut evt_idx = 0u16;

    for item in &interface.items {
        match item {
            InterfaceItem::Request(req) => {
                let opcode_name = format_ident!("REQ_{}", req.name.to_uppercase());
                request_opcodes.push(quote! { pub const #opcode_name: u16 = #req_idx; });

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
                        } else {
                            handler_call_args.push(quote! { #name });
                        }

                        match arg.typ.as_str() {
                            "object" => {
                                let is_nullable = arg.allow_null.unwrap_or(false);
                                if is_nullable {
                                    mapping_and_writing.push(quote! {
                                        let host_id = if #name == 0 {
                                            0
                                        } else if let Some(id) = ctx.shadow_table.get_host_id(#name) {
                                            id
                                        } else {
                                            log::debug!(
                                                "Dropping request due to missing mapping for nullable argument {}",
                                                #arg_name_str
                                            );
                                            return Ok(None);
                                        };
                                        builder.write_u32(host_id);
                                    });
                                } else {
                                    let req_name_str = &req.name;
                                    mapping_and_writing.push(quote! {
                                        let host_id = if let Some(id) = ctx.shadow_table.get_host_id(#name) {
                                            id
                                        } else {
                                            log::debug!("Dropping request {} due to missing mapping for non-nullable argument {}", #req_name_str, #arg_name_str);
                                            return Ok(None);
                                        };
                                        builder.write_u32(host_id);
                                    });
                                }
                            }
                            "new_id" => {
                                if let Some(ref interface_name) = arg.interface {
                                    mapping_and_writing.push(quote! {
                                        let object_version = ctx
                                            .shadow_table
                                            .guest_object_version(msg.sender_id)
                                            .unwrap_or(u32::MAX);
                                        let host_id = ctx.shadow_table.allocate_host_id();
                                        ctx.shadow_table.map_id(#name, host_id);
                                        ctx.shadow_table.track_interface_with_version(
                                            #name,
                                            #interface_name.to_string(),
                                            object_version,
                                        );
                                        ctx.shadow_table.set_host_version(host_id, object_version);
                                        builder.write_u32(host_id);
                                    });
                                } else {
                                    mapping_and_writing.push(quote! {
                                        let (ref interface_name, version, id) = #name;
                                        builder.write_string(interface_name);
                                        builder.write_u32(version);
                                        let host_id = ctx.shadow_table.allocate_host_id();
                                        ctx.shadow_table.map_id(id, host_id);
                                        ctx.shadow_table.track_interface_with_version(
                                            id,
                                            interface_name.clone(),
                                            version,
                                        );
                                        ctx.shadow_table.set_host_version(host_id, version);
                                        builder.write_u32(host_id);
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

                let is_destructor = req.msg_type.as_deref() == Some("destructor");
                let destructor_cleanup = if is_destructor {
                    quote! {
                        if ctx.shadow_table.is_guest_server_id(msg.sender_id) {
                            // Server-created objects do not receive
                            // wl_display.delete_id when the client destroys
                            // them. The forwarded request already captured the
                            // host sender ID, so release the translation now
                            // and allow the host to reuse its ID in a later
                            // ordered event.
                            ctx.shadow_table.remove_id(msg.sender_id);
                        } else {
                            ctx.shadow_table.mark_pending_destroy(msg.sender_id);
                        }
                    }
                } else {
                    quote! {}
                };
                let local_destructor_cleanup = if is_destructor {
                    quote! {
                        if is_local_only {
                            crate::handler::display::queue_local_delete_id(ctx, msg.sender_id);
                        }
                    }
                } else {
                    quote! {}
                };

                dispatch_request_arms.push(quote! {
                    Request::#var_name { #(#field_names_list),* } => {
                        if ctx.shadow_table.is_pending_destroy_guest(msg.sender_id) {
                            return Err(ProtocolError::InvalidObjectId(msg.sender_id));
                        }
                        #request_version_validation
                        #(#request_validations)*
                        let host_sender_id = ctx.shadow_table.get_host_id(msg.sender_id);
                        if host_sender_id.is_none()
                            && !ctx.shadow_table.is_local_only_guest_object(msg.sender_id)
                        {
                            return Err(ProtocolError::InvalidObjectId(msg.sender_id));
                        }
                        let is_local_only = host_sender_id.is_none()
                            && ctx.shadow_table.is_local_only_guest_object(msg.sender_id);
                        let handler_action = handler.#method_name(ctx, #(#handler_call_args),*);
                        if handler_action == Action::Forward {
                            #[allow(unused_mut)]
                            let mut builder = MessageBuilder::new();
                            #(#mapping_and_writing)*
                            let Some(host_sender_id) = host_sender_id else {
                                log::debug!(
                                    "Dropping request from unmapped sender {}",
                                    msg.sender_id
                                );
                                #local_destructor_cleanup
                                return Ok(None);
                            };
                            let full_msg =
                                builder.try_build_message(host_sender_id, #req_idx)?;
                            #destructor_cleanup
                            return Ok(Some((full_msg, builder.fds)));
                        }
                        #local_destructor_cleanup
                    }
                });

                req_idx += 1;
            }
            InterfaceItem::Event(evt) => {
                let opcode_name = format_ident!("EVT_{}", evt.name.to_uppercase());
                event_opcodes.push(quote! { pub const #opcode_name: u16 = #evt_idx; });

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
                        let arg_name_str = &arg.name;
                        field_names_list.push(name.clone());

                        if arg.typ == "string"
                            || arg.typ == "array"
                            || (arg.typ == "new_id" && arg.interface.is_none())
                        {
                            handler_call_args.push(quote! { &#name });
                        } else {
                            handler_call_args.push(quote! { #name });
                        }

                        match arg.typ.as_str() {
                            "object" => {
                                let is_nullable = arg.allow_null.unwrap_or(false);
                                if is_nullable {
                                    mapping_and_writing.push(quote! {
                                        let guest_id = if #name == 0 {
                                            0
                                        } else if let Some(id) = ctx.shadow_table.get_guest_id(#name) {
                                            id
                                        } else {
                                            log::debug!(
                                                "Dropping event due to missing mapping for nullable argument {}",
                                                #arg_name_str
                                            );
                                            return Ok(None);
                                        };
                                        builder.write_u32(guest_id);
                                    });
                                } else {
                                    let evt_name_str = &evt.name;
                                    mapping_and_writing.push(quote! {
                                        let guest_id = if let Some(id) = ctx.shadow_table.get_guest_id(#name) {
                                            id
                                        } else {
                                            log::debug!("Dropping event {} due to missing mapping for non-nullable argument {}", #evt_name_str, #arg_name_str);
                                            return Ok(None);
                                        };
                                        builder.write_u32(guest_id);
                                    });
                                }
                            }
                            "new_id" => {
                                if let Some(ref interface_name) = arg.interface {
                                    mapping_and_writing.push(quote! {
                                        let object_version = ctx
                                            .shadow_table
                                            .host_object_version(msg.sender_id)
                                            .unwrap_or(u32::MAX);
                                        let guest_id = ctx.shadow_table.allocate_guest_server_id();
                                        ctx.shadow_table.map_id(guest_id, #name);
                                        ctx.shadow_table.track_interface_with_version(
                                            guest_id,
                                            #interface_name.to_string(),
                                            object_version,
                                        );
                                        ctx.shadow_table.set_host_version(#name, object_version);
                                        builder.write_u32(guest_id);
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
                        ctx.shadow_table.remove_id(guest_sender_id);
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
                            #[allow(unused_mut)]
                            let mut builder = MessageBuilder::new();
                            #(#mapping_and_writing)*
                            let guest_sender_id = if let Some(id) = mapped_guest_sender_id {
                                id
                            } else {
                                log::debug!(
                                    "Dropping event from unmapped sender {}",
                                    msg.sender_id
                                );
                                return Ok(None);
                            };
                            let full_msg =
                                builder.try_build_message(guest_sender_id, #evt_idx)?;
                            #destructor_cleanup
                            return Ok(Some((full_msg, builder.fds)));
                        }
                    }
                });

                evt_idx += 1;
            }
            _ => {}
        }
    }

    quote! {
        pub mod #mod_name {
            use super::*;

            #(#request_opcodes)*
            #(#event_opcodes)*

            #[derive(Debug)]
            pub enum Request {
                #(#request_variants),*
            }

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
            ) -> Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError> {
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
            ) -> Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError> {
                if !ctx.shadow_table.is_event_sender_known(msg.sender_id) {
                    log::debug!(
                        "Dropping event from untracked host sender {}",
                        msg.sender_id
                    );
                    return Ok(None);
                }
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
                let sig_args = generate_handler_args_fq(args_items);
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
            .ok_or(ProtocolError::InvalidObjectId(msg.sender_id))?;
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
/// an existing shadow-table entry. Object arguments are checked here as well so
/// an unknown non-null object cannot later be silently translated to 0.
fn generate_request_validations(items: &[MessageItem]) -> Vec<TokenStream> {
    let mut validations = Vec::new();
    for item in items {
        let MessageItem::Arg(arg) = item else {
            continue;
        };
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
                let is_nullable = arg.allow_null.unwrap_or(false);
                if let Some(interface) = &arg.interface {
                    if is_nullable {
                        validations.push(quote! {
                            if #name != 0
                                && (ctx.shadow_table.is_pending_destroy_guest(#name)
                                    || !ctx.shadow_table.guest_object_matches(#name, #interface)
                                    || ctx.shadow_table.get_host_id(#name).is_none())
                            {
                                return Err(ProtocolError::InvalidObjectId(#name));
                            }
                        });
                    } else {
                        validations.push(quote! {
                            if ctx.shadow_table.is_pending_destroy_guest(#name)
                                || !ctx.shadow_table.guest_object_matches(#name, #interface)
                                || ctx.shadow_table.get_host_id(#name).is_none()
                            {
                                return Err(ProtocolError::InvalidObjectId(#name));
                            }
                        });
                    }
                } else if is_nullable {
                    validations.push(quote! {
                        if #name != 0
                            && (ctx.shadow_table.is_pending_destroy_guest(#name)
                                || ctx.shadow_table.get_host_id(#name).is_none())
                        {
                            return Err(ProtocolError::InvalidObjectId(#name));
                        }
                    });
                } else {
                    validations.push(quote! {
                        if ctx.shadow_table.is_pending_destroy_guest(#name)
                            || ctx.shadow_table.get_host_id(#name).is_none()
                        {
                            return Err(ProtocolError::InvalidObjectId(#name));
                        }
                    });
                }
            }
            _ => {}
        }
    }
    validations
}

/// Generate validation for IDs carried by a host event. A host-generated
/// object is handled by the event mapping code below, while object arguments
/// must already refer to a known host object before the handler runs.
fn generate_event_validations(items: &[MessageItem]) -> Vec<TokenStream> {
    let mut validations = Vec::new();
    for item in items {
        let MessageItem::Arg(arg) = item else {
            continue;
        };
        let name = sanitize_ident(&arg.name);
        match arg.typ.as_str() {
            "new_id" if arg.interface.is_some() => {
                validations.push(quote! {
                    if !ctx.shadow_table.is_host_id_available(#name) {
                        return Err(ProtocolError::InvalidObjectId(#name));
                    }
                });
            }
            "object" => {
                let is_nullable = arg.allow_null.unwrap_or(false);
                if let Some(interface) = &arg.interface {
                    if is_nullable {
                        validations.push(quote! {
                            if #name != 0
                                && (ctx.shadow_table.is_pending_destroy_host(#name)
                                    || !ctx.shadow_table.host_object_matches(#name, #interface)
                                    || ctx.shadow_table.get_guest_id(#name).is_none())
                            {
                                return Ok(None);
                            }
                        });
                    } else {
                        validations.push(quote! {
                            if ctx.shadow_table.is_pending_destroy_host(#name)
                                || !ctx.shadow_table.host_object_matches(#name, #interface)
                                || ctx.shadow_table.get_guest_id(#name).is_none()
                            {
                                return Ok(None);
                            }
                        });
                    }
                } else if is_nullable {
                    validations.push(quote! {
                        if #name != 0 && ctx.shadow_table.get_guest_id(#name).is_none() {
                            return Ok(None);
                        }
                    });
                } else {
                    validations.push(quote! {
                        if ctx.shadow_table.get_guest_id(#name).is_none() {
                            return Ok(None);
                        }
                    });
                }
            }
            _ => {}
        }
    }
    validations
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
            } else {
                args.push(quote! { #name: #ty });
            }
        }
    }
    quote! { #(#args),* }
}

fn generate_handler_args_fq(items: &[MessageItem]) -> TokenStream {
    let mut args = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            let name = format_ident!("_{}", sanitize_ident(&arg.name));
            let ty = map_type_fq(arg);
            if arg.typ == "string" {
                args.push(quote! { #name: &#ty });
            } else if arg.typ == "array" {
                args.push(quote! { #name: &[u8] });
            } else if arg.typ == "new_id" && arg.interface.is_none() {
                args.push(quote! { #name: &(String, u32, u32) });
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
