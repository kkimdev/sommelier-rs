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
use std::collections::{BTreeMap, HashSet, btree_map};

/// The string arguments that other arguments index by byte offset, as
/// `(interface, message, argument)`. Every other string decodes to a
/// `String` with each sequence that is not UTF-8 replaced by U+FFFD, but a
/// replacement would move these offsets, so these decode to
/// `wire::IndexedString`, which tells the handler that the string was not
/// UTF-8.
pub(crate) const BYTE_INDEXED_STRINGS: &[(&str, &str, &str)] = &[
    // Its cursor and anchor.
    ("zwp_text_input_v1", "set_surrounding_text", "text"),
    ("zwp_text_input_v3", "set_surrounding_text", "text"),
    // The preedit_cursor and preedit_styling events before it.
    ("zwp_text_input_v1", "preedit_string", "text"),
    // Its cursor_begin and cursor_end.
    ("zwp_text_input_v3", "preedit_string", "text"),
];

/// The requests a handler implements whole, as `(interface, request)`:
/// the generated code never forwards one, so its handler method returns
/// nothing and has no default. A request whose `new_id` names no
/// interface must be one, because only its handler knows what it creates;
/// `wl_registry.bind` is the only such request.
pub(crate) const HAND_WRITTEN_REQUESTS: &[(&str, &str)] = &[("wl_registry", "bind")];

/// Panic unless every entry of [`BYTE_INDEXED_STRINGS`] names exactly one
/// string argument of `protocols`, all the protocols the proxy generates,
/// so that a stale entry fails the build instead of leaving its string
/// lossy, and unless every entry of [`HAND_WRITTEN_REQUESTS`] names a
/// request.
pub fn check_byte_indexed_strings(protocols: &[Protocol]) {
    let unmatched = unmatched_entries(protocols, BYTE_INDEXED_STRINGS);
    assert!(
        unmatched.is_empty(),
        "BYTE_INDEXED_STRINGS entries that name no single string argument: {unmatched:?}"
    );
    let unmatched: Vec<_> = HAND_WRITTEN_REQUESTS
        .iter()
        .filter(|&&(interface, request)| {
            !protocols
                .iter()
                .flat_map(|protocol| &protocol.items)
                .filter_map(|item| match item {
                    ProtocolItem::Interface(found) if found.name == interface => Some(found),
                    _ => None,
                })
                .flat_map(|found| &found.items)
                .any(|item| matches!(item, InterfaceItem::Request(found) if found.name == request))
        })
        .collect();
    assert!(
        unmatched.is_empty(),
        "HAND_WRITTEN_REQUESTS entries that name no request: {unmatched:?}"
    );
}

/// Whether `interface.request` is in [`HAND_WRITTEN_REQUESTS`].
fn is_hand_written(interface: &str, request: &str) -> bool {
    HAND_WRITTEN_REQUESTS.contains(&(interface, request))
}

/// The entries of `table` that name no single string argument of
/// `protocols`.
pub(crate) fn unmatched_entries<'t>(
    protocols: &[Protocol],
    table: &[(&'t str, &'t str, &'t str)],
) -> Vec<(&'t str, &'t str, &'t str)> {
    let arguments = |(interface, message, argument): (&str, &str, &str)| {
        protocols
            .iter()
            .flat_map(|protocol| &protocol.items)
            .filter_map(|item| match item {
                ProtocolItem::Interface(found) if found.name == interface => Some(found),
                _ => None,
            })
            .flat_map(|found| &found.items)
            .filter_map(|item| match item {
                InterfaceItem::Request(found) | InterfaceItem::Event(found)
                    if found.name == message =>
                {
                    Some(found)
                }
                _ => None,
            })
            .flat_map(|found| args(&found.items))
            .filter(|arg| arg.name == argument && arg.typ == "string")
            .count()
    };
    table
        .iter()
        .copied()
        .filter(|&entry| arguments(entry) != 1)
        .collect()
}

/// `protocol` with the arguments in [`BYTE_INDEXED_STRINGS`] marked.
fn mark_byte_indexed_strings(protocol: &Protocol) -> Protocol {
    let mut protocol = protocol.clone();
    for item in &mut protocol.items {
        let ProtocolItem::Interface(interface) = item else {
            continue;
        };
        for item in &mut interface.items {
            let (InterfaceItem::Request(message) | InterfaceItem::Event(message)) = item else {
                continue;
            };
            for item in &mut message.items {
                let MessageItem::Arg(arg) = item else {
                    continue;
                };
                let key = (
                    interface.name.as_str(),
                    message.name.as_str(),
                    arg.name.as_str(),
                );
                arg.byte_indexed = BYTE_INDEXED_STRINGS.contains(&key);
                assert!(
                    !arg.byte_indexed || arg.typ == "string",
                    "{key:?} is not a string"
                );
            }
        }
    }
    protocol
}

pub fn generate(protocol: &Protocol) -> String {
    let protocol = &mark_byte_indexed_strings(protocol);
    let mut parts = Vec::new();
    let int_enums = int_enums(protocol);

    for item in &protocol.items {
        if let ProtocolItem::Interface(interface) = item {
            parts.push(generate_interface(interface, &int_enums));
        }
    }

    let protocol_name = format_ident!("{}", protocol.name);

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
            use crate::wire::{
                Action, Destructor, EventId, MessageBuilder, Outgoing, ProtocolError, RequestId,
                ToGuest, ToHost, WireId, WireMessage,
            };
            use crate::state::{
                Context, EventObject, EventSender, GuestId, HostId, Link, RequestObject,
                RequestSender,
            };
            use crate::protocols::Iface;

            #(#parts)*
        }
    };

    // `quote` already emits valid Rust. Running rustfmt here is unnecessary
    // for generated build output. Keep code generation deterministic and
    // leave source formatting to handwritten files.
    expanded.to_string()
}

/// Generate what spans every protocol: `Iface`, the interface of an
/// object, which the object table keeps; the one handler bound,
/// `ProtocolHandlers`; and the flat dispatch of a request or event by its
/// sender's interface.
///
/// The build script passes every protocol the proxy generates, so the
/// interfaces, their names and versions, and both dispatch entry points
/// come from the same XML as the protocol modules, and no interface is
/// routed by a name the proxy spells itself.
pub fn generate_routing(protocols: &[Protocol]) -> String {
    let mut handler_traits = Vec::new();
    let mut interfaces = Vec::new();
    // The proxy routes and versions an object by its interface alone.
    let mut routed_interfaces = HashSet::new();

    for protocol in protocols {
        let protocol_name = format_ident!("{}", protocol.name);
        for item in &protocol.items {
            let ProtocolItem::Interface(interface) = item else {
                continue;
            };
            let name = &interface.name;
            assert!(
                routed_interfaces.insert(name.clone()),
                "interface `{name}` is defined by more than one protocol"
            );
            let module = format_ident!("{}", name);
            let handler_trait = format_ident!("{}Handler", snake_to_camel(name));
            handler_traits.push(quote! { #protocol_name::#module::#handler_trait });
            interfaces.push((
                name.clone(),
                format_ident!("{}", snake_to_camel(name)),
                quote! { #protocol_name::#module },
            ));
        }
    }

    let count = interfaces.len();
    let variants = interfaces.iter().map(|(_, variant, _)| variant);
    let all = interfaces
        .iter()
        .map(|(_, variant, _)| quote! { Self::#variant });
    let name_arms = interfaces
        .iter()
        .map(|(_, variant, module)| quote! { Self::#variant => #module::NAME, });
    let version_arms = interfaces
        .iter()
        .map(|(_, variant, module)| quote! { Self::#variant => #module::VERSION, });
    let from_name_arms = interfaces
        .iter()
        .map(|(name, variant, _)| quote! { #name => Some(Self::#variant), });
    let dispatch_request_arms = interfaces.iter().map(|(_, variant, module)| {
        quote! { Iface::#variant => #module::dispatch_request::<H>(sender, msg, ctx), }
    });
    let decode_arms = interfaces.iter().map(|(_, variant, module)| {
        quote! { Iface::#variant => #module::Event::from_wire(msg).map(Self::#variant), }
    });
    let dispatch_event_arms = interfaces.iter().map(|(_, variant, module)| {
        quote! {
            AnyEvent::#variant(event) => #module::dispatch_event::<H>(sender, event, payload_len, ctx),
        }
    });
    let any_request = generate_any_message(
        &format_ident!("AnyRequest"),
        &format_ident!("Request"),
        &interfaces,
        true,
    );
    let any_event = generate_any_message(
        &format_ident!("AnyEvent"),
        &format_ident!("Event"),
        &interfaces,
        false,
    );
    let expanded = quote! {
        use crate::state::{Context, EventSender, RequestSender};
        use crate::wire::{ProtocolError, ToGuest, ToHost, WireMessage};

        /// An interface of the vendored protocols: what the object table
        /// records of every object, and what a message is dispatched by.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub enum Iface {
            #(#variants),*
        }

        impl Iface {
            /// Every interface, in the order the protocols define them.
            pub const ALL: [Self; #count] = [#(#all),*];

            /// The interface's name, as the wire spells it.
            pub const fn name(self) -> &'static str {
                match self {
                    #(#name_arms)*
                }
            }

            /// The newest version of the interface in the vendored XML,
            /// the newest the generated code can decode.
            pub const fn version(self) -> u32 {
                match self {
                    #(#version_arms)*
                }
            }

            /// The interface the wire names `name`, or `None` for one no
            /// vendored protocol defines.
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    #(#from_name_arms)*
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for Iface {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.name())
            }
        }

        /// The handler traits of every interface: what dispatch needs of
        /// the proxy's handlers.
        pub trait ProtocolHandlers: #(#handler_traits +)* {}

        impl<T: #(#handler_traits +)*> ProtocolHandlers for T {}

        /// Dispatch a guest request from `sender`, which the object table
        /// resolved, by its interface.
        pub fn dispatch_request<H: ProtocolHandlers>(
            sender: RequestSender,
            msg: &mut WireMessage,
            ctx: &mut Context,
        ) -> Result<Option<ToHost>, ProtocolError> {
            match sender.iface {
                #(#dispatch_request_arms)*
            }
        }

        impl AnyEvent {
            /// Decode `msg`, an event from an object of `iface`, whole: its
            /// payload and descriptors.
            pub fn decode(iface: Iface, msg: &mut WireMessage) -> Result<Self, ProtocolError> {
                match iface {
                    #(#decode_arms)*
                }
            }
        }

        /// Dispatch `event`, decoded from `sender`'s message of
        /// `payload_len` bytes, to its interface's handler.
        pub fn dispatch_event<H: ProtocolHandlers>(
            sender: EventSender,
            event: AnyEvent,
            payload_len: usize,
            ctx: &mut Context,
        ) -> Result<Option<ToGuest>, ProtocolError> {
            match event {
                #(#dispatch_event_arms)*
            }
        }

        #any_request
        #any_event
    };

    expanded.to_string()
}

/// Generate `AnyRequest` or `AnyEvent`: a message of any interface. A
/// test peer decodes one by the interface of its sender, so that one
/// assertion covers everything the proxy sent that peer, in order; the
/// proxy decodes each host event into an `AnyEvent` once, for everything
/// that reads it. `test_only` leaves the type out of the proxy.
fn generate_any_message(
    any_name: &Ident,
    message_type: &Ident,
    interfaces: &[(String, Ident, TokenStream)],
    test_only: bool,
) -> TokenStream {
    let cfg = if test_only {
        quote! { #[cfg(test)] }
    } else {
        quote! {}
    };
    let variants = interfaces
        .iter()
        .map(|(_, variant, module)| quote! { #variant(#module::#message_type) });
    let decode_arms = interfaces.iter().map(|(name, variant, module)| {
        quote! { #name => #module::#message_type::from_wire(msg).map(Self::#variant), }
    });
    let forward = |method: TokenStream| {
        interfaces
            .iter()
            .map(|(_, variant, _)| quote! { Self::#variant(message) => message.#method(), })
            .collect::<Vec<_>>()
    };
    let new_object_arms = forward(quote! { new_object });
    let object_args_arms = forward(quote! { object_args });
    let destructor_arms = forward(quote! { is_destructor });
    let since_arms = forward(quote! { since });
    let conversions = interfaces.iter().map(|(_, variant, module)| {
        quote! {
            #[cfg(test)]
            impl From<#module::#message_type> for #any_name {
                fn from(message: #module::#message_type) -> Self {
                    Self::#variant(message)
                }
            }
        }
    });
    quote! {
        #cfg
        #[derive(Debug)]
        #[cfg_attr(test, derive(PartialEq))]
        pub enum #any_name {
            #(#variants),*
        }

        #[cfg(test)]
        impl #any_name {
            /// Decode `msg`, sent by an object of `interface`. An interface
            /// no vendored protocol defines is an invalid sender.
            pub fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError> {
                match interface {
                    #(#decode_arms)*
                    _ => Err(ProtocolError::InvalidSender(msg.sender_id)),
                }
            }

            /// The ID and interface of the object this message creates.
            pub fn new_object(&self) -> Option<(u32, &str)> {
                match self {
                    #(#new_object_arms)*
                }
            }

            /// Each object argument: the ID it names (0 for null), the
            /// interface it declares, if any, and whether it may be null.
            pub fn object_args(&self) -> Vec<(u32, Option<&'static str>, bool)> {
                match self {
                    #(#object_args_arms)*
                }
            }

            /// Whether the message destroys its sender.
            pub fn is_destructor(&self) -> bool {
                match self {
                    #(#destructor_arms)*
                }
            }

            /// The object version that introduced the message.
            pub fn since(&self) -> u32 {
                match self {
                    #(#since_arms)*
                }
            }
        }

        #(#conversions)*
    }
}

fn map_type(arg: &Arg) -> TokenStream {
    match arg.typ.as_str() {
        "int" => quote! { i32 },
        "uint" => quote! { u32 },
        // Keep wl_fixed_t in its raw signed 24.8 representation. A float
        // round-trip can lose low bits while proxying an otherwise opaque
        // protocol value.
        "fixed" => quote! { i32 },
        "string" if arg.byte_indexed && arg.allow_null.unwrap_or(false) => {
            quote! { Option<crate::wire::IndexedString> }
        }
        "string" if arg.byte_indexed => quote! { crate::wire::IndexedString },
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
        "string" if arg.byte_indexed && arg.allow_null.unwrap_or(false) => {
            quote! { msg.read_nullable_indexed_string()? }
        }
        "string" if arg.byte_indexed => quote! { msg.read_indexed_string()? },
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

/// The write of a forwarded argument that needs no translation: every
/// type but `object` and `new_id`, whose IDs dispatch translates itself.
fn map_write_fn(arg: &Arg, name: &Ident) -> TokenStream {
    match arg.typ.as_str() {
        "int" => quote! { builder.write_i32(#name) },
        "uint" => quote! { builder.write_u32(#name) },
        "fixed" => quote! { builder.write_fixed(#name) },
        "string" if arg.byte_indexed && arg.allow_null.unwrap_or(false) => {
            quote! { builder.write_nullable_indexed_string(#name.as_ref()) }
        }
        "string" if arg.byte_indexed => quote! { builder.write_indexed_string(&#name) },
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
    constant_names.claim("NAME");
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
                request_encoders.push(generate_encoder(
                    &interface.name,
                    req,
                    &opcode_name,
                    quote! { RequestId },
                ));

                let var_name = format_ident!("{}", snake_to_camel(&req.name));
                let fields = generate_fields(&req.items);
                request_variants.push(quote! { #var_name { #fields } });

                let decodes = generate_reads(&req.items);
                let field_names = generate_field_names(&req.items);
                let request_validations = generate_request_validations(&req.items);
                let request_version_validation =
                    generate_version_validation(req.since, quote! { sender.guest });
                request_decoders.push(quote! {
                    #req_idx => {
                        #decodes
                        Ok(Request::#var_name { #field_names })
                    }
                });

                request_opcode_match.push(quote! {
                    Request::#var_name { .. } => #req_idx,
                });

                // A handler method, which forwards unless a handler
                // overrides it, or one the handler must write, which
                // forwards nothing.
                let method_name = format_ident!("on_{}", req.name);
                let method_args = generate_handler_args(&req.items, Side::Request);
                let hand_written = is_hand_written(&interface.name, &req.name);
                let destructor = req.msg_type.as_deref() == Some("destructor");
                assert!(
                    !(destructor && hand_written),
                    "{}.{} is a destructor, which the generated code ends",
                    interface.name,
                    req.name
                );
                handler_methods.push(if hand_written {
                    quote! {
                        fn #method_name(ctx: &mut Context, sender: RequestSender, #method_args);
                    }
                } else if destructor {
                    // A destructor's handler says how its object ends, and
                    // the generated code ends it, whatever it says.
                    quote! {
                        fn #method_name(
                            _ctx: &mut Context,
                            _sender: RequestSender,
                            #method_args
                        ) -> Destructor {
                            Destructor::Forward
                        }
                    }
                } else {
                    quote! {
                        fn #method_name(
                            _ctx: &mut Context,
                            _sender: RequestSender,
                            #method_args
                        ) -> Action {
                            Action::Forward
                        }
                    }
                });

                // Dispatch arm
                let mut local_object_checks = Vec::new();
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
                            // The pre-handler validation resolved the object
                            // (see `generate_request_validations`), which
                            // must name a host object unless it is null.
                            "object" if arg.allow_null.unwrap_or(false) => {
                                mapping_and_writing.push(quote! {
                                    builder.write_u32(
                                        #name
                                            .and_then(|object| object.host.host())
                                            .map_or(0, WireId::wire),
                                    );
                                });
                            }
                            "object" => {
                                let host = format_ident!("host_{}", arg.name);
                                let req_name_str = &req.name;
                                local_object_checks.push(quote! {
                                    let Link::Host(#host) = #name.host else {
                                        log::debug!(
                                            "Dropping request {} naming a local object as non-nullable argument {}",
                                            #req_name_str,
                                            #arg_name_str
                                        );
                                        return Ok(None);
                                    };
                                });
                                mapping_and_writing.push(quote! {
                                    builder.write_u32(#host.wire());
                                });
                            }
                            "new_id" => {
                                if let Some(ref interface_name) = arg.interface {
                                    // A child object takes its parent's
                                    // version on each side; a parent
                                    // without one is a bookkeeping fault
                                    // that ends the session. No local is
                                    // bound, so none can shadow a later
                                    // argument.
                                    let iface = iface_variant(interface_name);
                                    mapping_and_writing.push(quote! {
                                        builder.write_u32(
                                            ctx.shadow_table
                                                .create_paired(
                                                    #name,
                                                    #iface,
                                                    sender.version,
                                                    host.version,
                                                )
                                                .wire(),
                                        );
                                    });
                                } else {
                                    // Only its handler knows what it
                                    // creates; the arm forwards nothing.
                                    assert!(
                                        hand_written,
                                        "{}.{} creates an object of no interface, so it must be in HAND_WRITTEN_REQUESTS",
                                        interface.name, req.name
                                    );
                                }
                            }
                            _ => {
                                let write_call = map_write_fn(arg, &name);
                                mapping_and_writing.push(quote! { #write_call; });
                            }
                        }
                    }
                }

                dispatch_request_arms.push(if hand_written {
                    quote! {
                        Request::#var_name { #(#field_names_list),* } => {
                            #request_version_validation
                            #(#request_validations)*
                            H::#method_name(ctx, sender, #(#handler_call_args),*);
                        }
                    }
                } else if destructor {
                    // Every way a destructor's handler returns ends the
                    // object, unless it ended the session, which rolls
                    // nothing back. A local object's destructor completes
                    // in the proxy.
                    quote! {
                        Request::#var_name { #(#field_names_list),* } => {
                            #request_version_validation
                            #(#request_validations)*
                            let ending = H::#method_name(ctx, sender, #(#handler_call_args),*);
                            if ctx.is_fatal() {
                                return Ok(None);
                            }
                            let Some(host) = sender.host else {
                                ctx.local_destroyed(sender.guest);
                                return Ok(None);
                            };
                            match ending {
                                Destructor::HostInUse => {
                                    ctx.guest_destroyed_in_use(sender.guest);
                                    return Ok(None);
                                }
                                Destructor::HostKept => {
                                    if ctx.guest_ended_host_kept(sender.guest) {
                                        ctx.queue_delete_id(sender.guest);
                                    }
                                    return Ok(None);
                                }
                                Destructor::Forward
                                | Destructor::AfterOutput
                                | Destructor::AfterEnd => {}
                            }
                            #(#local_object_checks)*
                            #[allow(unused_mut)]
                            let mut builder = MessageBuilder::with_capacity(msg.payload.len());
                            #(#mapping_and_writing)*
                            let outgoing = builder.try_build(host.id, #req_idx)?;
                            let forward = match ending {
                                Destructor::AfterOutput => {
                                    ctx.send_host(outgoing);
                                    ctx.guest_destroyed(sender.guest);
                                    None
                                }
                                Destructor::AfterEnd => {
                                    ctx.guest_destroyed(sender.guest);
                                    ctx.send_host(outgoing);
                                    None
                                }
                                _ => {
                                    ctx.guest_destroyed(sender.guest);
                                    Some(outgoing)
                                }
                            };
                            return Ok(forward);
                        }
                    }
                } else {
                    quote! {
                        Request::#var_name { #(#field_names_list),* } => {
                            #request_version_validation
                            #(#request_validations)*
                            let handler_action = H::#method_name(ctx, sender, #(#handler_call_args),*);
                            if handler_action == Action::Forward {
                                // Check the sender and every object argument
                                // before mapping any new_id: a dropped request
                                // must not consume or map an ID, since
                                // libwayland-server refuses a gap in client IDs.
                                let Some(host) = sender.host else {
                                    log::debug!(
                                        "Dropping request from local object {}",
                                        sender.guest
                                    );
                                    return Ok(None);
                                };
                                #(#local_object_checks)*
                                // Translating IDs keeps the length; only a
                                // string with replaced bytes grows.
                                #[allow(unused_mut)]
                                let mut builder = MessageBuilder::with_capacity(msg.payload.len());
                                #(#mapping_and_writing)*
                                let outgoing = builder.try_build(host.id, #req_idx)?;
                                return Ok(Some(outgoing));
                            }
                        }
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
                event_encoders.push(generate_encoder(
                    &interface.name,
                    evt,
                    &opcode_name,
                    quote! { EventId },
                ));

                let var_name = format_ident!("{}", snake_to_camel(&evt.name));
                let fields = generate_fields(&evt.items);
                event_variants.push(quote! { #var_name { #fields } });

                let decodes = generate_reads(&evt.items);
                let field_names = generate_field_names(&evt.items);
                let event_validations = generate_event_validations(
                    &format!("{}.{}", interface.name, evt.name),
                    &evt.items,
                );
                let event_version_validation =
                    generate_version_validation(evt.since, quote! { sender.host });
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
                let method_args = generate_handler_args(&evt.items, Side::Event);
                handler_methods.push(quote! {
                    fn #method_name(
                        _ctx: &mut Context,
                        _sender: EventSender,
                        #method_args
                    ) -> Action {
                        Action::Forward
                    }
                });

                // Dispatch arm for event (Host -> Guest)
                let mut unmapped_object_checks = Vec::new();
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
                            // The pre-handler validation resolved the
                            // object (see `generate_event_validations`).
                            "object" if arg.interface.is_some() => {
                                mapping_and_writing.push(if arg.allow_null.unwrap_or(false) {
                                    quote! {
                                        builder.write_u32(
                                            #name.map_or(0, |object| object.guest.wire()),
                                        );
                                    }
                                } else {
                                    quote! { builder.write_u32(#name.guest.wire()); }
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
                                let guest = format_ident!("guest_{}", arg.name);
                                unmapped_object_checks.push(quote! {
                                    let Some(#guest) = ctx.shadow_table.guest_named_by_host(#name) else {
                                        log::debug!(
                                            "Dropping event {} naming an object with no guest counterpart as argument {}",
                                            #evt_name_str,
                                            #arg_name_str
                                        );
                                        return Ok(None);
                                    };
                                });
                                mapping_and_writing.push(quote! {
                                    builder.write_u32(#guest.wire());
                                });
                            }
                            "new_id" => {
                                let Some(interface_name) = &arg.interface else {
                                    panic!(
                                        "event argument {} creates an object of no interface, which no vendored protocol has",
                                        arg.name
                                    );
                                };
                                // A child object takes its parent's version,
                                // bound to no local that could shadow a later
                                // argument.
                                let iface = iface_variant(interface_name);
                                mapping_and_writing.push(quote! {
                                    builder.write_u32(
                                        ctx.shadow_table
                                            .adopt_host_created(#name, #iface, sender.version)
                                            .wire(),
                                    );
                                });
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
                        let mapped_guest_sender_id = sender.guest.map(|guest| guest.id);
                        if H::#method_name(ctx, sender, #(#handler_call_args),*) == Action::Forward {
                            // Check the sender and every object argument
                            // before mapping any new_id: a dropped event must
                            // not consume a guest server ID, since
                            // libwayland-client refuses gaps.
                            let Some(guest_sender_id) = mapped_guest_sender_id else {
                                log::debug!(
                                    "Dropping event from unmapped sender {}",
                                    sender.host
                                );
                                return Ok(None);
                            };
                            #(#unmapped_object_checks)*
                            #[allow(unused_mut)]
                            let mut builder = MessageBuilder::with_capacity(payload_len);
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

    // Handlers name the interface by its module's `NAME`, as the wire and
    // the shadow table do, rather than spelling it again.
    let interface_name = &interface.name;
    // The XML owns the newest version of each interface, as it owns each
    // message's since-version; nothing newer can be decoded.
    let interface_version = interface.version;
    let request_test_impls = generate_test_impls(&format_ident!("Request"), &requests);
    let event_test_impls = generate_test_impls(&format_ident!("Event"), &events);
    quote! {
        pub mod #mod_name {
            use super::*;

            pub const NAME: &str = #interface_name;
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

            /// Dispatch a request from `sender`, whose object the table
            /// resolved before anything of the message was read: as in
            /// libwayland-server, a request from an object that is gone is
            /// reported as such, whatever its opcode and arguments.
            pub fn dispatch_request<H: #handler_trait_name>(
                sender: RequestSender,
                msg: &mut WireMessage,
                ctx: &mut Context,
            ) -> Result<Option<ToHost>, ProtocolError> {
                let req = Request::from_wire(msg)?;
                match req {
                    #(#dispatch_request_arms)*
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
                Ok(None)
            }

            /// Dispatch `evt`, decoded from a message of `payload_len`
            /// bytes that `sender`, a live host object, sent.
            pub fn dispatch_event<H: #handler_trait_name>(
                sender: EventSender,
                evt: Event,
                payload_len: usize,
                ctx: &mut Context,
            ) -> Result<Option<ToGuest>, ProtocolError> {
                match evt {
                    #(#dispatch_event_arms)*
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
                Ok(None)
            }
        }
    }
}

/// Generate what tests need of a decoded message type, `Request` or
/// `Event`: equality, which compares a descriptor by the file it refers
/// to (`test_support::same_file`), since descriptors have no equality of
/// their own; `new_object`, the ID and interface of the object a
/// message creates, which lets a test peer follow the objects it is told
/// about as a Wayland client library does; and the rest of the message's
/// declaration a peer checks it against (`test_support::wire_peer`): the
/// objects it names, whether it destroys its sender, and the version that
/// introduced it. No vendored message creates more than one object
/// (`test_no_message_creates_two_objects`).
fn generate_test_impls(enum_name: &Ident, messages: &[&Message]) -> TokenStream {
    let mut eq_arms = Vec::new();
    let mut new_object_arms = Vec::new();
    let mut object_arms = Vec::new();
    let mut destructor_arms = Vec::new();
    let mut since_arms = Vec::new();
    for message in messages {
        let variant = format_ident!("{}", snake_to_camel(&message.name));
        let objects: Vec<_> = args(&message.items)
            .filter(|arg| arg.typ == "object")
            .collect();
        let object_fields = objects.iter().map(|arg| sanitize_ident(&arg.name));
        let object_values = objects.iter().map(|arg| {
            let field = sanitize_ident(&arg.name);
            let interface = match &arg.interface {
                Some(interface) => quote! { Some(#interface) },
                None => quote! { None },
            };
            let nullable = arg.allow_null.unwrap_or(false);
            quote! { (*#field, #interface, #nullable) }
        });
        object_arms.push(quote! {
            Self::#variant { #(#object_fields,)* .. } => vec![#(#object_values),*],
        });
        let destructor = message.msg_type.as_deref() == Some("destructor");
        destructor_arms.push(quote! { Self::#variant { .. } => #destructor, });
        let since = message.since.unwrap_or(1);
        since_arms.push(quote! { Self::#variant { .. } => #since, });
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

            /// Each object argument: the ID it names (0 for null), the
            /// interface it declares, if any, and whether it may be null.
            pub fn object_args(&self) -> Vec<(u32, Option<&'static str>, bool)> {
                match self {
                    #(#object_arms)*
                    // An interface without such messages has an empty enum.
                    #[allow(unreachable_patterns)]
                    _ => unreachable!(),
                }
            }

            /// Whether the message destroys its sender.
            pub fn is_destructor(&self) -> bool {
                match self {
                    #(#destructor_arms)*
                    // An interface without such messages has an empty enum.
                    #[allow(unreachable_patterns)]
                    _ => unreachable!(),
                }
            }

            /// The object version that introduced the message.
            pub fn since(&self) -> u32 {
                match self {
                    #(#since_arms)*
                    // An interface without such messages has an empty enum.
                    #[allow(unreachable_patterns)]
                    _ => unreachable!(),
                }
            }
        }
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
/// argument types of every message the proxy builds itself. Every ID it
/// names, the sender's included, is of one ID space `I`: a request's a
/// `RequestId` (the proxy's are host IDs) and an event's an `EventId` (the
/// proxy's are guest IDs), so the proxy cannot build a message that names
/// an object of the wrong side, or send it to the wrong peer. An object of
/// no interface, which may name any object or none
/// (`wl_display.error`'s), is a plain `u32`. A message that
/// carries a string or an array can exceed the wire's 16-bit length and
/// returns a `Result`; any other cannot and returns the message directly.
/// The message owns its descriptors, as `OwnedFd`s, so one that cannot be
/// built or is never sent closes them. A string is a `&str`, even one
/// other arguments index: what the proxy builds itself is UTF-8.
fn generate_encoder(
    interface: &str,
    message: &Message,
    opcode: &Ident,
    ids: TokenStream,
) -> TokenStream {
    let name = sanitize_ident(&message.name);
    let mut params = Vec::new();
    let mut writes = Vec::new();
    let mut fds = Vec::new();
    let mut fallible = false;
    // The payload's length, or, with strings and arrays, at least it, so
    // the builder allocates once: four bytes per word, and a string or
    // array's bytes padded to a word.
    let mut words = 0usize;
    let mut variable = Vec::new();
    for arg in args(&message.items) {
        let arg_name = sanitize_ident(&arg.name);
        let nullable = arg.allow_null.unwrap_or(false);
        words += match arg.typ.as_str() {
            "fd" => 0,
            "new_id" if arg.interface.is_none() => 3,
            _ => 1,
        };
        match arg.typ.as_str() {
            "new_id" if arg.interface.is_none() => {
                variable.push(quote! { interface.len() + 4 });
            }
            "string" if nullable => {
                variable.push(quote! { #arg_name.map_or(0, |string| string.len() + 4) });
            }
            "string" => variable.push(quote! { #arg_name.len() + 4 }),
            "array" => variable.push(quote! { #arg_name.len() + 3 }),
            _ => {}
        }
        // Encoders are for messages the proxy builds itself, whose IDs it
        // chose, so a null required object is a proxy bug. Host data is not
        // checked here: forwarding translates and validates it on its own
        // path, and must not pass it through an encoder unchecked.
        if matches!(arg.typ.as_str(), "object" | "new_id")
            && !nullable
            && (arg.typ == "new_id" || arg.interface.is_some())
        {
            let null = format!("{}.{}: {} is null", interface, message.name, arg.name);
            writes.push(quote! { debug_assert_ne!(#arg_name.wire(), 0, #null); });
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
            "object" if arg.interface.is_none() => {
                assert!(
                    !nullable,
                    "{}.{}: nullable object {} of no interface",
                    interface, message.name, arg.name
                );
                params.push(quote! { #arg_name: u32 });
                writes.push(quote! { builder.write_u32(#arg_name); });
            }
            "object" | "new_id" if nullable => {
                params.push(quote! { #arg_name: Option<I> });
                writes.push(quote! { builder.write_u32(#arg_name.map_or(0, WireId::wire)); });
            }
            // `wl_registry.bind` names the interface and version of the
            // object it creates.
            "new_id" if arg.interface.is_none() => {
                fallible = true;
                params.push(quote! { interface: &str, version: u32, #arg_name: I });
                writes.push(quote! {
                    builder.write_string(interface);
                    builder.write_u32(version);
                    builder.write_u32(#arg_name.wire());
                });
            }
            "object" | "new_id" => {
                params.push(quote! { #arg_name: I });
                writes.push(quote! { builder.write_u32(#arg_name.wire()); });
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
    let fixed_bytes = 4 * words;
    // The descriptors go in one exact allocation, as a forward's do.
    let fds = if fds.is_empty() {
        quote! {}
    } else {
        quote! { let builder = builder.with_fds(vec![#(#fds),*]); }
    };
    let (output, message) = if fallible {
        (
            quote! { Result<Outgoing<I>, ProtocolError> },
            quote! { builder.try_build(sender_id, super::#opcode) },
        )
    } else {
        (
            quote! { Outgoing<I> },
            quote! { builder.build(sender_id, super::#opcode) },
        )
    };
    // A dropped encoder result loses the message without a trace. The
    // reason keeps Clippy from calling the attribute redundant on a
    // `Result`.
    quote! {
        #[must_use = "a message that is not queued is never sent"]
        pub fn #name<I: #ids>(sender_id: I, #(#params),*) -> #output {
            #[allow(unused_mut)]
            let mut builder = MessageBuilder::with_capacity(#fixed_bytes #(+ #variable)*);
            #(#writes)*
            #fds
            #message
        }
    }
}

/// The flat constant names of one interface module.
///
/// Opcodes, their `_SINCE` versions, `NAME`, `VERSION` and enum entries
/// share one namespace, so an enum named after a message could produce a
/// name twice.
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
/// interface module (libwayland's `<INTERFACE>_<ENUM>_<ENTRY>`): the wire
/// value, which a message the proxy forwards or builds carries as it is.
///
/// An enum that is not a bitfield also gets its decoded type, a Rust enum
/// named after it (`wl_output::Transform`) with one variant per value
/// (`Flipped90`; `_90` for an entry that starts with a digit). A handler
/// that interprets a value decodes it once with `TryFrom` the wire integer,
/// which returns the integer back when the XML does not define it, so a
/// decoded value is always one the XML defines. `From` gives the wire
/// integer back, and `since` the interface version that introduced the
/// value. An entry with an earlier entry's value is an associated constant
/// of that variant, whose `since` is the older version. A bitfield keeps its
/// constants alone: every combination of its flags is a value.
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
    // Each value's variant and oldest version.
    let mut variants: BTreeMap<u32, (Ident, u32)> = BTreeMap::new();
    let mut aliases = Vec::new();
    let mut taken_variants = HashSet::new();
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
        constants.push(quote! { pub const #name: #value_type = #value; });

        let variant = variant_ident(&entry.name);
        assert!(
            taken_variants.insert(variant.to_string()),
            "{} generates the variant {} twice",
            enumeration.name,
            variant
        );
        let since = entry.since.unwrap_or(enum_since);
        match variants.entry(raw_value) {
            btree_map::Entry::Vacant(vacant) => {
                vacant.insert((variant, since));
            }
            btree_map::Entry::Occupied(mut occupied) => {
                let (first, oldest) = occupied.get_mut();
                *oldest = since.min(*oldest);
                aliases.push(quote! { pub const #variant: Self = Self::#first; });
            }
        }
    }
    if enumeration.bitfield.as_deref() == Some("true") {
        return quote! { #(#constants)* };
    }
    assert!(!variants.is_empty(), "{} has no entries", enumeration.name);

    let type_name = format_ident!("{}", snake_to_camel(&enumeration.name));
    assert!(
        !["Request", "Event"].contains(&type_name.to_string().as_str()),
        "the enum {} names the module's {} type",
        enumeration.name,
        type_name
    );
    let raw_values: Vec<_> = variants.keys().collect();
    let variant_names: Vec<_> = variants.values().map(|(variant, _)| variant).collect();
    let sinces: Vec<_> = variants.values().map(|(_, since)| since).collect();
    let int_conversions = is_int.then(|| {
        quote! {
            impl TryFrom<i32> for #type_name {
                type Error = i32;
                fn try_from(value: i32) -> Result<Self, i32> {
                    u32::try_from(value)
                        .ok()
                        .and_then(|value| Self::try_from(value).ok())
                        .ok_or(value)
                }
            }
            impl From<#type_name> for i32 {
                fn from(value: #type_name) -> i32 {
                    value as i32
                }
            }
        }
    });
    quote! {
        #(#constants)*
        // The XML names the variants.
        #[allow(clippy::enum_variant_names)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u32)]
        pub enum #type_name {
            #(#variant_names = #raw_values,)*
        }
        impl #type_name {
            #(#aliases)*
            /// The interface version that introduced this value.
            pub const fn since(self) -> u32 {
                match self {
                    #(Self::#variant_names => #sinces,)*
                }
            }
        }
        impl TryFrom<u32> for #type_name {
            type Error = u32;
            fn try_from(value: u32) -> Result<Self, u32> {
                match value {
                    #(#raw_values => Ok(Self::#variant_names),)*
                    _ => Err(value),
                }
            }
        }
        impl From<#type_name> for u32 {
            fn from(value: #type_name) -> u32 {
                value as u32
            }
        }
        #int_conversions
    }
}

/// The variant of enum entry `name`: its camel case, after an underscore if
/// it starts with a digit, which no identifier can.
fn variant_ident(name: &str) -> Ident {
    let camel = snake_to_camel(name);
    if camel.starts_with(|c: char| c.is_ascii_digit()) {
        format_ident!("_{}", camel)
    } else {
        format_ident!("{}", camel)
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
fn generate_version_validation(since: Option<u32>, sender_id: TokenStream) -> TokenStream {
    let Some(required) = since else {
        return quote! {};
    };

    quote! {
        if sender.version < #required {
            return Err(ProtocolError::UnsupportedVersion {
                object_id: #sender_id.wire(),
                required: #required,
                actual: sender.version,
            });
        }
    }
}

/// Generate pre-handler validation for IDs supplied by a guest request.
///
/// Wayland clients own the IDs in requests. A repeated, null, or
/// server-reserved ID is a protocol error and must not be allowed to overwrite
/// an existing shadow-table entry. Each ID is validated and typed in one
/// lookup of the object table, which shadows the wire value: a `new_id` as
/// the `GuestId` it may create, an object as the `RequestObject` it names,
/// `None` if it is null and may be.
fn generate_request_validations(items: &[MessageItem]) -> Vec<TokenStream> {
    let mut validations = Vec::new();
    for arg in args(items) {
        let name = sanitize_ident(&arg.name);
        match arg.typ.as_str() {
            "new_id" => {
                let (id, typed) = if arg.interface.is_none() {
                    (quote! { #name.2 }, quote! { (#name.0, #name.1, guest_id) })
                } else {
                    (quote! { #name }, quote! { guest_id })
                };
                validations.push(quote! {
                    let #name = {
                        let Some(guest_id) = ctx.shadow_table.guest_new_id(#id) else {
                            return Err(ProtocolError::InvalidObjectId(#id));
                        };
                        #typed
                    };
                });
            }
            "object" => {
                let Some(interface) = &arg.interface else {
                    panic!(
                        "request argument {} names an object of no interface, which no vendored protocol has",
                        arg.name
                    );
                };
                let iface = iface_variant(interface);
                let object = quote! {
                    ctx.shadow_table
                        .request_object(#name, #iface)
                        .ok_or(ProtocolError::InvalidObjectId(#name))?
                };
                validations.push(if arg.allow_null.unwrap_or(false) {
                    quote! {
                        let #name = if #name == 0 { None } else { Some(#object) };
                    }
                } else {
                    quote! { let #name = #object; }
                });
            }
            _ => {}
        }
    }
    validations
}

/// Generate validation for IDs carried by a host event. A host-generated
/// object must have a fresh server ID, which shadows the wire value as the
/// `HostId` it creates. Each object argument of an interface must name a
/// live object with a guest counterpart, or the event is dropped; it is
/// validated and typed in one lookup, which shadows the wire value as the
/// `EventObject` it names, `None` if it is null and may be. An object of
/// no interface is resolved only when the event is forwarded. `event`
/// names the event (`interface.event`) in what the lookup logs.
fn generate_event_validations(event: &str, items: &[MessageItem]) -> Vec<TokenStream> {
    let mut validations = Vec::new();
    for arg in args(items) {
        let name = sanitize_ident(&arg.name);
        match arg.typ.as_str() {
            "new_id" if arg.interface.is_some() => {
                validations.push(quote! {
                    let Some(#name) = ctx.shadow_table.host_new_id(#name) else {
                        return Err(ProtocolError::InvalidObjectId(#name));
                    };
                });
            }
            // An object of no interface (`wl_display.error`'s object) may
            // name any object, including one only the proxy uses, so its
            // handler must see the event; only forwarding resolves it.
            "object" => {
                let Some(interface) = &arg.interface else {
                    continue;
                };
                let iface = iface_variant(interface);
                let lookup = quote! { ctx.shadow_table.event_object(#name, #iface, #event) };
                validations.push(if arg.allow_null.unwrap_or(false) {
                    quote! {
                        let #name = if #name == 0 {
                            None
                        } else {
                            let Some(object) = #lookup else {
                                return Ok(None);
                            };
                            Some(object)
                        };
                    }
                } else {
                    quote! {
                        let Some(#name) = #lookup else {
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

/// Which side sends a message.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The guest, whose requests name guest objects.
    Request,
    /// The host, whose events name host objects.
    Event,
}

/// The parameters of a handler method, after its sender.
///
/// A handler gets each ID as the object table resolved it before the
/// handler ran, typed by its side: an object of an interface as the
/// `RequestObject` or `EventObject` it names (`Option` if it may be null),
/// a request's `new_id` as the `GuestId` it creates and an event's as the
/// `HostId`. An object of no interface, which may name anything, stays a
/// wire `u32`. A handler borrows the message's descriptors: the dispatcher
/// owns them, forwards them or closes them, and a handler that keeps one
/// clones it.
fn generate_handler_args(items: &[MessageItem], side: Side) -> TokenStream {
    let mut args = Vec::new();
    for item in items {
        if let MessageItem::Arg(arg) = item {
            let name = format_ident!("_{}", sanitize_ident(&arg.name));
            let ty = map_type(arg);
            let nullable = arg.allow_null.unwrap_or(false);
            let object = match side {
                Side::Request => quote! { RequestObject },
                Side::Event => quote! { EventObject },
            };
            if arg.typ == "string" {
                args.push(quote! { #name: &#ty });
            } else if arg.typ == "array" {
                args.push(quote! { #name: &[u8] });
            } else if arg.typ == "new_id" && arg.interface.is_none() {
                args.push(quote! { #name: &(String, u32, GuestId) });
            } else if arg.typ == "new_id" {
                args.push(match side {
                    Side::Request => quote! { #name: GuestId },
                    Side::Event => quote! { #name: HostId },
                });
            } else if arg.typ == "object" && arg.interface.is_some() && nullable {
                args.push(quote! { #name: Option<#object> });
            } else if arg.typ == "object" && arg.interface.is_some() {
                args.push(quote! { #name: #object });
            } else if arg.typ == "fd" {
                args.push(quote! { #name: std::os::fd::BorrowedFd<'_> });
            } else {
                args.push(quote! { #name: #ty });
            }
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

/// The `Iface` variant of interface `name`.
fn iface_variant(name: &str) -> TokenStream {
    let variant = format_ident!("{}", snake_to_camel(name));
    quote! { Iface::#variant }
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
