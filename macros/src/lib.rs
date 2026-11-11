#![forbid(unsafe_code)]
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, Attribute, FnArg, GenericArgument, Ident, ImplItem,
    ImplItemFn, ItemImpl, ItemTrait, Pat, PathArguments, ReturnType, TraitItem, Type, TypePath,
};

// --- Helpers for #[protocol] ---

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_uppercase() {
            // Insert underscore before uppercase if:
            // - not at start, AND
            // - previous char is lowercase, OR next char is lowercase (handles acronyms)
            if i > 0 {
                let prev_lower = chars[i - 1].is_lowercase();
                let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
                if prev_lower || next_lower {
                    result.push('_');
                }
            }
            result.push(ch.to_ascii_lowercase());
        } else {
            result.push(ch);
        }
    }
    result
}

fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect()
}

fn strip_protocol_suffix(name: &str) -> String {
    name.strip_suffix("Protocol").unwrap_or(name).to_string()
}

// A method-level cfg_attr has not been expanded when the enclosing protocol
// macro runs. Reject conditional timeout attributes even for false predicates;
// otherwise Rust sees an unknown attribute, or silently omits the timeout.
fn conditional_timeout(meta: &syn::Meta) -> bool {
    if let syn::Meta::List(list) = meta {
        if list.path.is_ident("cfg_attr") {
            use syn::parse::Parser;
            let arguments =
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated;
            if let Ok(arguments) = arguments.parse2(list.tokens.clone()) {
                return arguments.iter().skip(1).any(|meta| {
                    meta.path().is_ident("request_timeout") || conditional_timeout(meta)
                });
            }
        }
    }
    false
}

enum MethodKind {
    Send,
    Request(Box<Type>),
}

#[derive(Clone, Copy)]
enum RuntimeMode {
    Tasks,
    Threads,
}

fn classify_return_type(ret: &ReturnType) -> Result<MethodKind, &Type> {
    match ret {
        ReturnType::Default => Ok(MethodKind::Send),
        ReturnType::Type(_, ty) => {
            if is_unit_type(ty) {
                return Ok(MethodKind::Send);
            }
            if let Some(inner) = extract_response_inner(ty) {
                return Ok(MethodKind::Request(inner));
            }
            if let Some(inner) = extract_result_inner(ty) {
                if is_unit_type(&inner) {
                    return Ok(MethodKind::Send);
                }
                // Result<T, ActorError> where T ≠ () is no longer supported;
                // use Response<T> which works in both modes.
                return Err(ty);
            }
            Err(ty)
        }
    }
}

fn extract_response_inner(ty: &Type) -> Option<Box<Type>> {
    if let Type::Path(TypePath { path, .. }) = ty {
        let seg = path.segments.last()?;
        if seg.ident == "Response" {
            if let PathArguments::AngleBracketed(args) = &seg.arguments {
                if let Some(GenericArgument::Type(inner)) = args.args.first() {
                    return Some(Box::new(inner.clone()));
                }
            }
        }
    }
    None
}

fn extract_result_inner(ty: &Type) -> Option<Box<Type>> {
    if let Type::Path(TypePath { path, .. }) = ty {
        let seg = path.segments.last()?;
        if seg.ident == "Result" {
            if let PathArguments::AngleBracketed(args) = &seg.arguments {
                if let Some(GenericArgument::Type(inner)) = args.args.first() {
                    return Some(Box::new(inner.clone()));
                }
            }
        }
    }
    None
}

/// Protocol parameters become fields of a `'static` message struct. Report
/// borrows and `impl Trait` here; rustc's own errors point into the expansion.
fn unsupported_param_type(ty: &Type) -> Option<syn::Error> {
    const OWNED: &str = "protocol parameters become message fields and must be owned ('static); \
                         use an owned type such as String or Arc<T> instead of a borrow";
    match ty {
        Type::Reference(r) => match &r.lifetime {
            Some(lifetime) if lifetime.ident == "static" => unsupported_param_type(&r.elem),
            _ => Some(syn::Error::new_spanned(r, OWNED)),
        },
        Type::ImplTrait(t) => Some(syn::Error::new_spanned(
            t,
            "`impl Trait` parameters are not supported in protocol methods; \
             use a concrete type or Box<dyn Trait>",
        )),
        Type::Path(TypePath { qself, path }) => {
            if let Some(error) = qself.as_ref().and_then(|q| unsupported_param_type(&q.ty)) {
                return Some(error);
            }
            path.segments
                .iter()
                .find_map(|segment| match &segment.arguments {
                    PathArguments::AngleBracketed(args) => {
                        args.args.iter().find_map(|arg| match arg {
                            GenericArgument::Type(inner) => unsupported_param_type(inner),
                            GenericArgument::Lifetime(lifetime) if lifetime.ident != "static" => {
                                Some(syn::Error::new_spanned(lifetime, OWNED))
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
        }
        Type::Tuple(t) => t.elems.iter().find_map(unsupported_param_type),
        Type::Array(a) => unsupported_param_type(&a.elem),
        Type::Slice(s) => unsupported_param_type(&s.elem),
        Type::Paren(p) => unsupported_param_type(&p.elem),
        Type::Group(g) => unsupported_param_type(&g.elem),
        // fn pointers and trait objects may use higher-ranked borrows.
        _ => None,
    }
}

fn is_unit_type(ty: &Type) -> bool {
    if let Type::Tuple(tuple) = ty {
        return tuple.elems.is_empty();
    }
    false
}

/// Returns true for types available without explicit import (prelude + primitives).
/// These can't be accessed via `super::` so must be left unqualified.
fn is_prelude_or_primitive(name: &str) -> bool {
    matches!(
        name,
        // Primitives
        "bool" | "char" | "str"
            | "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
            | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
            | "f32" | "f64"
            // Prelude types (Rust 2021)
            | "Box" | "String" | "Vec"
            | "Option" | "Some" | "None"
            | "Result" | "Ok" | "Err"
            | "ToString" | "ToOwned"
    )
}

/// Qualify a type with `super::` so it resolves to the parent module's scope.
/// This prevents name collisions when a generated struct name shadows an imported type
/// via `use super::*`.
///
/// Prelude/primitive types are left unqualified (they can't be accessed via `super::`).
/// User-defined types get `super::` prepended: `Event` → `super::Event`.
/// Generic args are recursively qualified: `Vec<Event>` → `Vec<super::Event>`.
fn qualify_type_with_super(ty: &Type) -> Type {
    match ty {
        Type::Path(TypePath { qself, path }) => {
            // Leave qualified paths as-is: <X as T>::Y, ::abs::path, crate::, super::, self::
            if qself.is_some() || path.leading_colon.is_some() {
                return ty.clone();
            }
            if let Some(first) = path.segments.first() {
                let s = first.ident.to_string();
                if matches!(
                    s.as_str(),
                    "crate" | "super" | "self" | "std" | "core" | "alloc"
                ) {
                    return ty.clone();
                }
            }

            // Recursively qualify generic arguments in all segments
            let qualified_segments: syn::punctuated::Punctuated<_, _> = path
                .segments
                .iter()
                .map(|seg| {
                    let mut new_seg = seg.clone();
                    if let PathArguments::AngleBracketed(ref mut args) = new_seg.arguments {
                        for arg in &mut args.args {
                            if let GenericArgument::Type(ref mut inner) = arg {
                                *inner = qualify_type_with_super(inner);
                            }
                        }
                    }
                    new_seg
                })
                .collect();

            // Only prepend super:: for non-prelude types
            if let Some(first) = path.segments.first() {
                if is_prelude_or_primitive(&first.ident.to_string()) {
                    return Type::Path(TypePath {
                        qself: None,
                        path: syn::Path {
                            leading_colon: None,
                            segments: qualified_segments,
                        },
                    });
                }
            }

            // Prepend super:: for user-defined types
            let mut segments = syn::punctuated::Punctuated::new();
            segments.push(syn::PathSegment {
                ident: format_ident!("super"),
                arguments: PathArguments::None,
            });
            for seg in qualified_segments {
                segments.push(seg);
            }

            Type::Path(TypePath {
                qself: None,
                path: syn::Path {
                    leading_colon: None,
                    segments,
                },
            })
        }
        Type::Reference(r) => {
            let mut new = r.clone();
            new.elem = Box::new(qualify_type_with_super(&r.elem));
            Type::Reference(new)
        }
        Type::Tuple(t) => {
            let mut new = t.clone();
            for elem in &mut new.elems {
                *elem = qualify_type_with_super(elem);
            }
            Type::Tuple(new)
        }
        Type::Array(a) => {
            let mut new = a.clone();
            *new.elem = qualify_type_with_super(&a.elem);
            Type::Array(new)
        }
        Type::Slice(s) => {
            let mut new = s.clone();
            *new.elem = qualify_type_with_super(&s.elem);
            Type::Slice(new)
        }
        Type::Paren(p) => {
            let mut new = p.clone();
            *new.elem = qualify_type_with_super(&p.elem);
            Type::Paren(new)
        }
        Type::TraitObject(t) => {
            let mut new = t.clone();
            for bound in &mut new.bounds {
                if let syn::TypeParamBound::Trait(tb) = bound {
                    qualify_path_with_super(&mut tb.path);
                }
            }
            Type::TraitObject(new)
        }
        Type::ImplTrait(t) => {
            let mut new = t.clone();
            for bound in &mut new.bounds {
                if let syn::TypeParamBound::Trait(tb) = bound {
                    qualify_path_with_super(&mut tb.path);
                }
            }
            Type::ImplTrait(new)
        }
        Type::BareFn(f) => {
            let mut new = f.clone();
            for arg in &mut new.inputs {
                arg.ty = qualify_type_with_super(&arg.ty);
            }
            if let ReturnType::Type(_, ref mut ty) = new.output {
                **ty = qualify_type_with_super(ty);
            }
            Type::BareFn(new)
        }
        _ => ty.clone(),
    }
}

/// Qualify a path's generic arguments with `super::`, used for trait bounds
/// in `dyn Trait` and `impl Trait` types.
fn qualify_path_with_super(path: &mut syn::Path) {
    for seg in &mut path.segments {
        match &mut seg.arguments {
            PathArguments::AngleBracketed(ref mut args) => {
                for arg in &mut args.args {
                    if let GenericArgument::Type(ref mut inner) = arg {
                        *inner = qualify_type_with_super(inner);
                    }
                }
            }
            PathArguments::Parenthesized(ref mut args) => {
                for input in &mut args.inputs {
                    *input = qualify_type_with_super(input);
                }
                if let syn::ReturnType::Type(_, ref mut ty) = args.output {
                    **ty = qualify_type_with_super(ty);
                }
            }
            PathArguments::None => {}
        }
    }
}

struct ProtocolInfo<'a> {
    trait_name: &'a Ident,
    mod_name: &'a Ident,
    ref_name: &'a Ident,
    converter_trait: &'a Ident,
    converter_method: &'a Ident,
    target_trait: &'a Ident,
}

/// One transport signature used in declarations, runtime impls and delegation.
fn transport_signature(
    info: &ProtocolInfo,
    method: &ProtocolMethodInfo,
    index: usize,
) -> (Ident, proc_macro2::TokenStream) {
    let name = format_ident!("__dispatch_{}", index);
    let module = info.mod_name;
    let message = &method.struct_name;
    let (timeout, result) = match &method.kind {
        MethodKind::Send => (
            quote! {},
            quote! { ::std::result::Result<(), spawned_concurrency::ActorError> },
        ),
        MethodKind::Request(result) => (
            quote! { , timeout: ::std::time::Duration },
            quote! { spawned_concurrency::Response<#result> },
        ),
    };
    let signature =
        quote! { fn #name(target: &Self, message: #module::#message #timeout) -> #result };
    (name, signature)
}

/// Runtime-specific transport primitives, shared by plain and configured handles.
/// Cfg-gated Handler bounds are represented by one marker set per runtime.
fn generate_target_impl(
    info: &ProtocolInfo,
    methods: &[ProtocolMethodInfo],
    runtime_path: &proc_macro2::TokenStream,
    mode: RuntimeMode,
) -> proc_macro2::TokenStream {
    let ProtocolInfo {
        trait_name,
        mod_name,
        target_trait,
        ..
    } = info;

    // Unconditional methods: Handler bounds go directly on the impl block.
    let handler_bounds: Vec<_> = methods
        .iter()
        .filter(|m| m.cfg_attrs.is_empty())
        .map(|m| {
            let sn = &m.struct_name;
            quote! { #runtime_path::Handler<#mod_name::#sn> }
        })
        .collect();

    // Group cfg-gated methods by their cfg predicate.
    // Each unique group gets a marker trait that conditionally requires the Handler bounds.
    let mut cfg_groups: Vec<(String, Vec<&ProtocolMethodInfo>)> = Vec::new();
    for m in methods.iter().filter(|m| !m.cfg_attrs.is_empty()) {
        let key: String = m
            .cfg_attrs
            .iter()
            .map(|a| quote!(#a).to_string())
            .collect::<Vec<_>>()
            .join(",");
        if let Some(group) = cfg_groups.iter_mut().find(|(k, _)| k == &key) {
            group.1.push(m);
        } else {
            cfg_groups.push((key, vec![m]));
        }
    }

    let mode_suffix = match mode {
        RuntimeMode::Tasks => format_ident!("Tasks"),
        RuntimeMode::Threads => format_ident!("Threads"),
    };

    let mut marker_trait_defs = Vec::new();
    let mut marker_trait_bounds = Vec::new();

    for (i, (_key, group_methods)) in cfg_groups.iter().enumerate() {
        let marker_name = format_ident!("__{}Cfg{}{}", trait_name, i, mode_suffix);
        let cfg_attrs = &group_methods[0].cfg_attrs;
        let group_handler_bounds: Vec<_> = group_methods
            .iter()
            .map(|m| {
                let sn = &m.struct_name;
                quote! { #runtime_path::Handler<#mod_name::#sn> }
            })
            .collect();

        // Combine cfg predicates and negate for the fallback impl.
        // #[cfg(A)] #[cfg(B)] -> active: all(A, B), inactive: not(all(A, B))
        let cfg_predicates: Vec<proc_macro2::TokenStream> = cfg_attrs
            .iter()
            .filter(|a| a.path().is_ident("cfg"))
            .filter_map(|a| a.parse_args::<proc_macro2::TokenStream>().ok())
            .collect();

        let (positive_cfg, negated_cfg) = if cfg_predicates.len() == 1 {
            let pred = &cfg_predicates[0];
            (quote! { #[cfg(#pred)] }, quote! { #[cfg(not(#pred))] })
        } else {
            (
                quote! { #[cfg(all(#(#cfg_predicates),*))] },
                quote! { #[cfg(not(all(#(#cfg_predicates),*)))] },
            )
        };

        marker_trait_defs.push(quote! {
            #positive_cfg
            #[doc(hidden)]
            trait #marker_name: #(#group_handler_bounds)+* {}
            #positive_cfg
            impl<__T: #(#group_handler_bounds)+*> #marker_name for __T {}

            #negated_cfg
            #[doc(hidden)]
            trait #marker_name {}
            #negated_cfg
            impl<__T> #marker_name for __T {}
        });

        marker_trait_bounds.push(quote! { + #marker_name });
    }

    let method_impls = methods.iter().enumerate().map(|(index, m)| {
        let (_, signature) = transport_signature(info, m, index);
        let cfg = &m.cfg_attrs;
        let body = match &m.kind {
            MethodKind::Send => quote! { target.send(message) },
            MethodKind::Request(_) => match mode {
                RuntimeMode::Tasks => quote! { spawned_concurrency::Response::from_with_timeout(target.request_raw(message), timeout) },
                RuntimeMode::Threads => quote! { spawned_concurrency::Response::ready(target.request_with_timeout(message, timeout)) },
            },
        };
        quote! { #(#cfg)* #signature { #body } }
    });
    quote! {
        #(#marker_trait_defs)*
        impl<__A: #runtime_path::Actor #(+ #handler_bounds)* #(#marker_trait_bounds)*>
            #target_trait for #runtime_path::ActorRef<__A>
        {
            #(#method_impls)*
        }
    }
}

/// Share transport bodies and one method template while keeping public protocol
/// impls concrete. Supertraits constrain each nominal target independently.
fn generate_protocol_adapters(
    info: &ProtocolInfo,
    methods: &[ProtocolMethodInfo],
    definition: &ItemTrait,
) -> proc_macro2::TokenStream {
    let ProtocolInfo {
        trait_name,
        mod_name,
        ref_name,
        converter_trait,
        converter_method,
        target_trait,
    } = info;
    let signatures = methods.iter().enumerate().map(|(index, m)| {
        let (_, signature) = transport_signature(info, m, index);
        let cfg = &m.cfg_attrs;
        quote! { #(#cfg)* #signature; }
    });
    let delegates = methods.iter().enumerate().map(|(index, m)| {
        let (name, signature) = transport_signature(info, m, index);
        let cfg = &m.cfg_attrs;
        let timeout = match &m.kind { MethodKind::Send => quote! {}, MethodKind::Request(_) => quote! { , timeout } };
        quote! {
            #(#cfg)* #signature {
                <__T as #target_trait>::#name(
                    spawned_concurrency::response::RequestOptions::actor_ref(target), message #timeout
                )
            }
        }
    });
    let method_impls: Vec<_> = methods.iter().enumerate().map(|(index, m)| {
        let method = &m.method_name;
        let dispatch = format_ident!("__dispatch_{}", index);
        let params = &m.params;
        let attrs = &m.method_attrs;
        let result = &m.ret_type;
        let message = &m.struct_name;
        let fields = &m.field_names;
        let construct = if fields.is_empty() {
            quote! { #mod_name::#message }
        } else {
            quote! { #mod_name::#message { #(#fields),* } }
        };
        let body = match &m.kind {
            MethodKind::Send => {
                let call = quote! { <Self as #target_trait>::#dispatch(self, #construct) };
                let unit = match result {
                    ReturnType::Default => true,
                    ReturnType::Type(_, ty) => is_unit_type(ty),
                };
                if unit { quote! { let _ = #call; } } else { call }
            }
            MethodKind::Request(_) => {
                let timeout = &m.timeout;
                let binding = syn::Ident::new("__spawned_request_timeout", proc_macro2::Span::mixed_site());
                quote! {
                    // Evaluate before message construction can move arguments;
                    // an explicit override does not evaluate the default expression.
                    let #binding: ::std::time::Duration = match <Self as #target_trait>::__timeout_override(self) {
                        Some(timeout) => timeout,
                        None => #timeout,
                    };
                    <Self as #target_trait>::#dispatch(self, #construct, #binding)
                }
            }
        };
        quote! {
            #(#attrs)*
            fn #method(&self, #(#params),*) #result { #body }
        }
    }).collect();
    // Keep public impls nominal. A blanket impl over the hidden target trait
    // conflicts with existing Box<T>/&T protocol forwarding implementations.
    let mut public_impls = Vec::new();
    for (runtime, asynchronous) in [
        (quote! { spawned_concurrency::tasks }, true),
        (quote! { spawned_concurrency::threads }, false),
    ] {
        let actor = quote! { #runtime::ActorRef<__A> };
        for target in [
            actor.clone(),
            quote! { spawned_concurrency::response::RequestOptions<#actor> },
        ] {
            let mut predicates: Vec<_> = definition
                .generics
                .where_clause
                .iter()
                .flat_map(|clause| clause.predicates.iter())
                .map(|p| quote! { #p })
                .collect();
            let supertraits = &definition.supertraits;
            if !supertraits.is_empty() {
                predicates.push(quote! { Self: #supertraits });
            }
            let implementation = quote! {
                impl<__A: #runtime::Actor> #trait_name for #target
                    where Self: #target_trait, #(#predicates,)*
                { #(#method_impls)* }
                impl<__A: #runtime::Actor> #converter_trait for #target
                    where Self: #target_trait + #trait_name
                {
                    fn #converter_method(&self) -> #ref_name { ::std::sync::Arc::new(self.clone()) }
                }
            };
            public_impls.push(if asynchronous {
                quote! { spawned_concurrency::__spawned_async_items! { #implementation } }
            } else {
                implementation
            });
        }
    }
    quote! {
        #[doc(hidden)]
        trait #target_trait: Clone + Send + Sync + 'static {
            fn __timeout_override(_target: &Self) -> Option<::std::time::Duration> { None }
            #(#signatures)*
        }
        impl<__T: #target_trait> #target_trait for spawned_concurrency::response::RequestOptions<__T> {
            fn __timeout_override(target: &Self) -> Option<::std::time::Duration> {
                Some(spawned_concurrency::response::RequestOptions::timeout(target))
            }
            #(#delegates)*
        }
        #(#public_impls)*
    }
}

struct ProtocolMethodInfo {
    method_name: Ident,
    struct_name: Ident,
    field_names: Vec<Ident>,
    field_types: Vec<Type>,
    kind: MethodKind,
    params: Vec<FnArg>,
    ret_type: ReturnType,
    /// Doc + cfg attributes to propagate to blanket impl methods.
    method_attrs: Vec<Attribute>,
    cfg_attrs: Vec<Attribute>,
    timeout: syn::Expr,
}

/// Generates message types and blanket implementations from a protocol trait.
///
/// `#[protocol]` transforms a trait definition into a full message-passing interface.
/// Each trait method becomes a message struct, and any `ActorRef<A>` where `A` handles
/// those messages automatically implements the trait — so you call methods directly on
/// the actor reference.
///
/// # What It Generates
///
/// For a trait `FooProtocol`, the macro generates:
///
/// 1. **Message structs** in a `foo_protocol` submodule — one per method, with public
///    fields matching the method parameters. Each implements `Message`.
///    Unit structs (methods with no parameters beyond `&self`) automatically derive `Clone`.
/// 2. **Type alias** `pub type FooRef = Arc<dyn FooProtocol>` — a type-erased reference.
/// 3. **Converter trait** `ToFooRef` with a `to_foo_ref(&self) -> FooRef` method.
/// 4. **Blanket impls** — `impl FooProtocol for ActorRef<A>` and `impl ToFooRef for ActorRef<A>`
///    for any actor `A` that handles all the generated message types.
///
/// # Type Resolution in Generated Structs
///
/// The generated message module uses `use super::*` to access types from the
/// parent scope. Types used in method parameters are qualified with `super::`
/// in the generated structs — this means any type you reference in a protocol
/// method signature must be in scope where the `#[protocol]` trait is defined.
///
/// Prelude types (`String`, `Vec`, `Option`, `Result`, `Box`, `bool`, `u32`, etc.)
/// and fully qualified paths (`std::`, `core::`, `alloc::`) are used as-is without
/// `super::` prefixing.
///
/// # Return Type Conventions
///
/// The return type on each method determines the message kind:
///
/// | Return type | Kind | Runtime | Caller behavior |
/// |-------------|------|---------|-----------------|
/// | `Response<T>` | Request | Both | `.await.unwrap()` (tasks) / `.unwrap()` (threads) — 5s default timeout |
/// | `Result<(), ActorError>` | Send | Both | Returns send result |
/// | *(none)* / `-> ()` | Send | Both | Fire-and-forget (discards send result) |
///
/// # Naming
///
/// - **Module**: trait name → `snake_case` (e.g., `ChatRoomProtocol` → `chat_room_protocol`)
/// - **Structs**: method name → `PascalCase` (e.g., `send_message` → `SendMessage`)
/// - **XRef alias**: strips `Protocol` suffix → `{Base}Ref` (e.g., `ChatRoomProtocol` → `ChatRoomRef`)
/// - **Converter**: `To{Base}Ref` with `to_{snake_base}_ref()` method
///
/// # Example
///
/// ```ignore
/// use spawned_concurrency::Response;
/// use spawned_concurrency::protocol;
///
/// #[protocol]
/// pub trait CounterProtocol: Send + Sync {
///     fn increment(&self, amount: u64);                     // send (fire-and-forget)
///     fn get_count(&self) -> Response<u64>;                 // request (both modes)
/// }
///
/// // Generated:
/// // - pub mod counter_protocol { pub struct Increment { pub amount: u64 }, pub struct GetCount }
/// // - pub type CounterRef = Arc<dyn CounterProtocol>;
/// // - pub trait ToCounterRef { fn to_counter_ref(&self) -> CounterRef; }
/// // - impl CounterProtocol for ActorRef<A> where A: Handler<Increment> + Handler<GetCount>
/// ```
/// # Request timeouts
///
/// `#[protocol(timeout = Duration::from_secs(30))]` sets a protocol default;
/// `#[request_timeout(Duration::from_secs(2))]` overrides a request method.
/// Without either setting, requests keep their five-second default.
/// `actor.with_request_timeout(limit).method(...)` takes precedence and also
/// supports conversion with the generated `ToXRef` trait. Configure before
/// erasing the concrete handle. These are reply-wait limits, not cancellation
/// or inherited deadlines; accepted work may still execute after timeout.
/// Native protocol calls block immediately; async response timers start on poll.
#[proc_macro_attribute]
pub fn protocol(attr: TokenStream, item: TokenStream) -> TokenStream {
    let options = parse_macro_input!(attr with syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated);
    let mut protocol_timeout = None;
    for option in options {
        if !option.path().is_ident("timeout") {
            return syn::Error::new_spanned(
                option.path(),
                "unsupported protocol option; expected timeout = Duration expression",
            )
            .to_compile_error()
            .into();
        }
        let syn::Meta::NameValue(option) = option else {
            return syn::Error::new_spanned(option, "expected `timeout = <Duration expression>`")
                .to_compile_error()
                .into();
        };
        if protocol_timeout.replace(option.value).is_some() {
            return syn::Error::new_spanned(option.path, "duplicate protocol timeout")
                .to_compile_error()
                .into();
        }
    }
    let mut trait_def = parse_macro_input!(item as ItemTrait);
    let trait_name = &trait_def.ident;
    let trait_vis = &trait_def.vis;

    if !trait_def.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &trait_def.generics,
            "generic parameters (types, lifetimes, consts) on protocol traits are not supported",
        )
        .to_compile_error()
        .into();
    }

    let base_name = strip_protocol_suffix(&trait_name.to_string());
    let mod_name = format_ident!("{}", to_snake_case(&trait_name.to_string()));
    let ref_name = format_ident!("{}Ref", base_name);
    let converter_trait = format_ident!("To{}Ref", base_name);
    let converter_method = format_ident!("to_{}_ref", to_snake_case(&base_name));

    let mut methods: Vec<ProtocolMethodInfo> = Vec::new();

    for item in &mut trait_def.items {
        if !matches!(item, TraitItem::Fn(_)) {
            return syn::Error::new_spanned(
                item,
                "protocol traits may only contain methods; \
                 associated types, constants, and other items are not supported",
            )
            .to_compile_error()
            .into();
        }
        if let TraitItem::Fn(method) = item {
            if let Some(attribute) = method.attrs.iter().find(|a| conditional_timeout(&a.meta)) {
                return syn::Error::new_spanned(
                    attribute,
                    "conditional request_timeout is not supported; use #[request_timeout(...)] directly",
                ).to_compile_error().into();
            }
            if method.sig.asyncness.is_some() {
                return syn::Error::new_spanned(
                    &method.sig,
                    "protocol methods must not be async; \
                     use Response<T> as the return type for requests",
                )
                .to_compile_error()
                .into();
            }

            // Verify first param is &self
            match method.sig.inputs.first() {
                Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_none() => {}
                _ => {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "protocol methods must take `&self` as the first parameter",
                    )
                    .to_compile_error()
                    .into();
                }
            }

            let sig = &method.sig;
            let qualifier = [
                (sig.unsafety.as_ref().map(|t| quote!(#t)), "unsafe"),
                (sig.abi.as_ref().map(|t| quote!(#t)), "extern"),
                (sig.constness.as_ref().map(|t| quote!(#t)), "const"),
            ]
            .into_iter()
            .find_map(|(token, name)| Some((token?, name)));
            if let Some((token, name)) = qualifier {
                return syn::Error::new_spanned(
                    token,
                    format!("`{name}` protocol methods are not supported; use a plain `fn`"),
                )
                .to_compile_error()
                .into();
            }
            if !method.sig.generics.params.is_empty() {
                return syn::Error::new_spanned(
                    &method.sig.generics,
                    "protocol methods cannot have generic parameters; \
                     each method becomes one concrete message struct",
                )
                .to_compile_error()
                .into();
            }
            for arg in method.sig.inputs.iter().skip(1) {
                if let FnArg::Typed(pat_type) = arg {
                    if let Some(error) = unsupported_param_type(&pat_type.ty) {
                        return error.to_compile_error().into();
                    }
                }
            }

            let method_name = method.sig.ident.clone();
            let struct_name = format_ident!("{}", to_pascal_case(&method_name.to_string()));
            // cfg-gated methods may legitimately share a name across exclusive
            // predicates; only unconditional collisions are reported here.
            let conditional = |attrs: &[Attribute]| {
                attrs
                    .iter()
                    .any(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
            };
            if !conditional(&method.attrs) {
                if let Some(previous) = methods
                    .iter()
                    .find(|m| m.cfg_attrs.is_empty() && m.struct_name == struct_name)
                {
                    let message = if previous.method_name == method_name {
                        format!("duplicate protocol method `{method_name}`")
                    } else {
                        format!(
                            "protocol method `{method_name}` generates message struct \
                             `{struct_name}`, which `{}` already uses; rename one of them",
                            previous.method_name
                        )
                    };
                    return syn::Error::new_spanned(&method.sig.ident, message)
                        .to_compile_error()
                        .into();
                }
            }

            let mut field_names: Vec<Ident> = Vec::new();
            let mut field_types: Vec<Type> = Vec::new();
            let mut params: Vec<FnArg> = Vec::new();

            for arg in method.sig.inputs.iter().skip(1) {
                if let FnArg::Typed(pat_type) = arg {
                    if let Pat::Ident(pat_ident) = &*pat_type.pat {
                        field_names.push(pat_ident.ident.clone());
                        field_types.push((*pat_type.ty).clone());
                    } else {
                        return syn::Error::new_spanned(
                            &pat_type.pat,
                            "protocol methods only support simple identifier patterns \
                             (e.g., `name: Type`)",
                        )
                        .to_compile_error()
                        .into();
                    }
                }
                params.push(arg.clone());
            }

            let kind = match classify_return_type(&method.sig.output) {
                Ok(kind) => kind,
                Err(ty) => {
                    return syn::Error::new_spanned(
                        ty,
                        "unsupported return type in protocol method; \
                         use Response<T> for requests (works in both async and sync modes), \
                         Result<(), ActorError> for sends, or no return type for fire-and-forget",
                    )
                    .to_compile_error()
                    .into();
                }
            };

            let mut method_timeout = None;
            for attr in method
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("request_timeout"))
            {
                if !matches!(kind, MethodKind::Request(_)) {
                    return syn::Error::new_spanned(
                        attr,
                        "request_timeout is only valid on Response<T> methods",
                    )
                    .to_compile_error()
                    .into();
                }
                let expression = match attr.parse_args::<syn::Expr>() {
                    Ok(expression) => expression,
                    Err(error) => return error.to_compile_error().into(),
                };
                if method_timeout.replace(expression).is_some() {
                    return syn::Error::new_spanned(attr, "duplicate request_timeout")
                        .to_compile_error()
                        .into();
                }
            }
            method
                .attrs
                .retain(|a| !a.path().is_ident("request_timeout"));
            let timeout = method_timeout
                .or_else(|| protocol_timeout.clone())
                .unwrap_or_else(
                    || syn::parse_quote! { spawned_concurrency::response::DEFAULT_REQUEST_TIMEOUT },
                );

            let method_attrs: Vec<Attribute> = method
                .attrs
                .iter()
                .filter(|a| {
                    a.path().is_ident("doc")
                        || a.path().is_ident("cfg")
                        || a.path().is_ident("cfg_attr")
                })
                .cloned()
                .collect();
            let cfg_attrs: Vec<Attribute> = method
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
                .cloned()
                .collect();

            methods.push(ProtocolMethodInfo {
                method_name,
                struct_name,
                field_names,
                field_types,
                kind,
                params,
                ret_type: method.sig.output.clone(),
                method_attrs,
                cfg_attrs,
                timeout,
            });
        }
    }

    // Generate message structs
    // Field types and result types are qualified with `super::` to prevent
    // name collisions when a generated struct name shadows an imported type.
    // e.g., `fn event(&self, event: Event)` generates `struct Event { pub event: super::Event }`
    let msg_structs: Vec<_> = methods
        .iter()
        .map(|m| {
            let struct_name = &m.struct_name;
            let field_names = &m.field_names;
            let qualified_field_types: Vec<Type> =
                m.field_types.iter().map(qualify_type_with_super).collect();
            let method_attrs = &m.method_attrs;
            let cfg_attrs = &m.cfg_attrs;
            let msg_result_ty: Type = match &m.kind {
                MethodKind::Send => syn::parse_quote! { () },
                MethodKind::Request(inner) => qualify_type_with_super(inner),
            };

            if field_names.is_empty() {
                quote! {
                    #(#method_attrs)*
                    #[derive(Clone)]
                    pub struct #struct_name;
                    #(#cfg_attrs)*
                    impl Message for #struct_name {
                        type Result = #msg_result_ty;
                    }
                }
            } else {
                quote! {
                    #(#method_attrs)*
                    pub struct #struct_name {
                        #(pub #field_names: #qualified_field_types,)*
                    }
                    #(#cfg_attrs)*
                    impl Message for #struct_name {
                        type Result = #msg_result_ty;
                    }
                }
            }
        })
        .collect();

    // Gate async impls using the dependency feature namespace, not the consumer.
    let target_trait = format_ident!("__{}Target", trait_name);
    let tasks = quote! { spawned_concurrency::tasks };
    let threads = quote! { spawned_concurrency::threads };
    let proto_info = ProtocolInfo {
        trait_name,
        mod_name: &mod_name,
        ref_name: &ref_name,
        converter_trait: &converter_trait,
        converter_method: &converter_method,
        target_trait: &target_trait,
    };
    let tasks_impl = generate_target_impl(&proto_info, &methods, &tasks, RuntimeMode::Tasks);
    let threads_impl = generate_target_impl(&proto_info, &methods, &threads, RuntimeMode::Threads);
    let adapters = generate_protocol_adapters(&proto_info, &methods, &trait_def);
    let blanket_impls = quote! {
        #adapters
        spawned_concurrency::__spawned_async_items! { #tasks_impl }
        #threads_impl
    };

    let ref_doc = format!(
        "Type-erased reference to any actor implementing [`{trait_name}`].\n\n\
         Use this type to store protocol references without depending on the concrete actor type."
    );

    // Protocol traits are consumed via blanket impls and dyn references,
    // so rustc may report methods as unused during development.
    let output = quote! {
        #[allow(dead_code)]
        #trait_def

        #[doc = #ref_doc]
        #trait_vis type #ref_name = ::std::sync::Arc<dyn #trait_name>;

        #trait_vis mod #mod_name {
            use super::*;
            use spawned_concurrency::message::Message;
            #(#msg_structs)*
        }

        #trait_vis trait #converter_trait {
            fn #converter_method(&self) -> #ref_name;
        }

        impl #converter_trait for #ref_name {
            fn #converter_method(&self) -> #ref_name {
                ::std::sync::Arc::clone(self)
            }
        }

        #blanket_impls
    };

    output.into()
}

fn is_handler_attribute(attr: &Attribute) -> bool {
    let path = attr.path();
    path.is_ident("handler") || path.is_ident("send_handler") || path.is_ident("request_handler")
}

fn is_actor_method_attribute(attr: &Attribute) -> bool {
    let path = attr.path();
    is_handler_attribute(attr)
        || path.is_ident("started")
        || path.is_ident("stopped")
        || path.is_ident("exit_received")
}

/// Generates `impl Actor` and `Handler<M>` implementations from an annotated impl block.
///
/// Place `#[actor]` on an `impl MyStruct` block. The macro extracts annotated methods
/// and generates the boilerplate needed to run the struct as an actor.
///
/// # Protocol Assertion
///
/// Use `protocol = TraitName` to verify at compile time that the actor fully implements
/// a protocol (i.e., that `ActorRef<Self>` implements the protocol trait):
///
/// ```ignore
/// #[actor(protocol = NameServerProtocol)]
/// impl NameServer { /* ... */ }
/// ```
///
/// The runtime (tasks vs threads) is inferred from whether any handler in the block
/// is `async fn`. If all handlers are defined outside the `#[actor]` block, put at
/// least one handler inside so the macro can detect the correct runtime.
///
/// For multiple protocols:
/// ```ignore
/// #[actor(protocol(RoomProtocol, UserProtocol))]
/// impl ChatUser { /* ... */ }
/// ```
///
/// # Lints
///
/// `#[actor]` warns (compile time only) when the impl block builds or accepts
/// shared mutable state (`Arc<Mutex<_>>`, `Arc<RwLock<_>>`, `Arc<Atomic*>`,
/// `Rc<RefCell<_>>`), and once per crate built with `panic = "abort"`. Opt out
/// with `#[actor(allow_shared_state)]` and `#[actor(allow_panic_abort)]`.
///
/// # Handler Attributes
///
/// | Attribute | Use for |
/// |-----------|---------|
/// | `#[request_handler]` | Messages that expect a reply (`Response<T>`) |
/// | `#[send_handler]` | Fire-and-forget messages |
/// | `#[handler]` | Generic handler (works for either kind) |
///
/// Handler signature: `fn name(&mut self, msg: MessageType, ctx: &Context<Self>) -> ReturnType`
///
/// The return type must match the message's `Message::Result`. For request handlers
/// returning `()`, omit the return type.
///
/// # Lifecycle Hooks
///
/// | Attribute | Called |
/// |-----------|--------|
/// | `#[started]` | After the actor starts, before processing messages |
/// | `#[stopped]` | After the actor stops processing messages |
/// | `#[exit_received]` | A linked exit delivered while trapping exits |
///
/// Both receive `&mut self` and `&Context<Self>`. Use async or sync to match your runtime.
///
/// # Example
///
/// ```ignore
/// use spawned_concurrency::tasks::{Actor, Context, Handler};
/// use spawned_concurrency::actor;
///
/// pub struct MyActor { count: u64 }
///
/// #[actor(protocol = CounterProtocol)]
/// impl MyActor {
///     pub fn new() -> Self { MyActor { count: 0 } }
///
///     #[started]
///     async fn on_start(&mut self, _ctx: &Context<Self>) {
///         tracing::info!("Actor started");
///     }
///
///     #[send_handler]
///     async fn handle_increment(&mut self, msg: Increment, _ctx: &Context<Self>) {
///         self.count += msg.amount;
///     }
///
///     #[request_handler]
///     async fn handle_get_count(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
///         self.count
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn actor(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut impl_block = parse_macro_input!(item as ItemImpl);

    if let Some((_, path, _)) = &impl_block.trait_ {
        return syn::Error::new_spanned(
            path,
            "#[actor] must be applied to an inherent impl block (`impl MyActor { ... }`), \
             not a trait impl",
        )
        .to_compile_error()
        .into();
    }

    // Tasks actors use `async fn` for every handler and hook, threads actors
    // plain `fn`; the runtime is inferred from them. A sync method returning
    // `impl Future` is valid in tasks mode, and cfg-gated methods may differ.
    let mut runtime_methods = impl_block.items.iter().filter_map(|item| match item {
        ImplItem::Fn(method)
            if method.attrs.iter().any(is_actor_method_attribute)
                && !method.attrs.iter().any(|a| a.path().is_ident("cfg"))
                && !matches!(&method.sig.output, ReturnType::Type(_, ty) if matches!(**ty, Type::ImplTrait(_))) =>
        {
            Some(&method.sig)
        }
        _ => None,
    });
    if let Some(first) = runtime_methods.next() {
        let asynchronous = first.asyncness.is_some();
        if let Some(other) = runtime_methods.find(|sig| sig.asyncness.is_some() != asynchronous) {
            return syn::Error::new_spanned(
                other,
                "cannot mix async and sync handlers or lifecycle hooks in one #[actor] block; \
                 tasks actors use `async fn` throughout, threads actors use plain `fn`",
            )
            .to_compile_error()
            .into();
        }
    }

    let self_ty = &impl_block.self_ty;
    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

    // --- Parse named parameters: #[actor(protocol = X)], #[actor(protocol(X, Y))],
    // and the lint opt-outs `allow_shared_state` and `allow_panic_abort` ---
    let mut allow_shared_state = false;
    let mut allow_panic_abort = false;
    let bridge_traits: Vec<Ident> = if attr.is_empty() {
        Vec::new()
    } else {
        // A protocol is named by a single identifier; report paths explicitly
        // instead of failing on the `::` token.
        fn protocol_name(input: syn::parse::ParseStream) -> syn::Result<Ident> {
            let path: syn::Path = input.parse()?;
            path.get_ident().cloned().ok_or_else(|| {
                syn::Error::new_spanned(
                    &path,
                    "protocol must be named by a single identifier; \
                     import the trait first (`use path::to::Protocol;`)",
                )
            })
        }
        let parser = |input: syn::parse::ParseStream| -> syn::Result<Vec<Ident>> {
            let mut protocols = Vec::new();
            while !input.is_empty() {
                let key: Ident = input.parse()?;
                if key == "allow_shared_state" {
                    allow_shared_state = true;
                } else if key == "allow_panic_abort" {
                    allow_panic_abort = true;
                } else if key != "protocol" {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown parameter, expected `protocol`, `allow_shared_state` \
                         or `allow_panic_abort`",
                    ));
                } else if input.peek(syn::Token![=]) {
                    // protocol = TraitName
                    let _: syn::Token![=] = input.parse()?;
                    protocols.push(protocol_name(input)?);
                } else if input.peek(syn::token::Paren) {
                    // protocol(Trait1, Trait2)
                    let content;
                    syn::parenthesized!(content in input);
                    let punctuated = content.parse_terminated(protocol_name, syn::Token![,])?;
                    protocols.extend(punctuated);
                } else {
                    return Err(syn::Error::new(
                        key.span(),
                        "expected `protocol = Trait` or `protocol(TraitA, TraitB)`",
                    ));
                }
                if !input.is_empty() {
                    let _: syn::Token![,] = input.parse().map_err(|e| {
                        syn::Error::new(e.span(), "expected `,` between #[actor] parameters")
                    })?;
                }
            }
            Ok(protocols)
        };
        match syn::parse::Parser::parse(parser, attr) {
            Ok(traits) => traits,
            Err(e) => return e.to_compile_error().into(),
        }
    };

    // --- Compile-time lints (ROADMAP rank 6b); no runtime code is generated ---
    let lints = actor_lints(&impl_block, allow_shared_state, allow_panic_abort);

    // --- Extract #[started] and #[stopped] lifecycle methods ---
    let mut started_method: Option<ImplItemFn> = None;
    let mut stopped_method: Option<ImplItemFn> = None;
    let mut exit_method: Option<ImplItemFn> = None;
    let mut has_async = false;

    let mut items_to_keep = Vec::new();
    for item in impl_block.items.drain(..) {
        if let ImplItem::Fn(ref method) = item {
            let is_started = method.attrs.iter().any(|a| a.path().is_ident("started"));
            let is_stopped = method.attrs.iter().any(|a| a.path().is_ident("stopped"));

            let is_exit = method
                .attrs
                .iter()
                .any(|a| a.path().is_ident("exit_received"));
            if is_exit {
                if exit_method.is_some() {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "only one #[exit_received] method is allowed per actor",
                    )
                    .to_compile_error()
                    .into();
                }
                if is_started
                    || is_stopped
                    || method.attrs.iter().any(|a| {
                        a.path().is_ident("handler")
                            || a.path().is_ident("send_handler")
                            || a.path().is_ident("request_handler")
                    })
                {
                    return syn::Error::new_spanned(&method.sig, "#[exit_received] cannot be combined with other lifecycle or handler attributes").to_compile_error().into();
                }
                if method.sig.inputs.len() != 3
                    || !matches!(method.sig.inputs.first(), Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_some())
                {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[exit_received] must take (&mut self, Exit, &Context<Self>)",
                    )
                    .to_compile_error()
                    .into();
                }
                let mut m = method.clone();
                m.attrs.retain(|a| !a.path().is_ident("exit_received"));
                m.vis = syn::Visibility::Inherited;
                m.sig.ident = format_ident!("exit_received");
                if m.sig.asyncness.is_some() {
                    has_async = true;
                }
                exit_method = Some(m);
                continue;
            }

            if is_started && is_stopped {
                return syn::Error::new_spanned(
                    &method.sig,
                    "#[started] and #[stopped] cannot be combined on one method",
                )
                .to_compile_error()
                .into();
            }

            if is_started {
                if started_method.is_some() {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "only one #[started] method is allowed per actor",
                    )
                    .to_compile_error()
                    .into();
                }
                if method.attrs.iter().any(|a| {
                    a.path().is_ident("handler")
                        || a.path().is_ident("send_handler")
                        || a.path().is_ident("request_handler")
                }) {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[started] cannot be combined with handler attributes",
                    )
                    .to_compile_error()
                    .into();
                }
                // Expect: fn started(&mut self, ctx: &Context<Self>)
                if method.sig.inputs.len() != 2 {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[started] method must take exactly (&mut self, &Context<Self>)",
                    )
                    .to_compile_error()
                    .into();
                }
                if !matches!(method.sig.inputs.first(), Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_some())
                {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[started] method's first parameter must be `&mut self`",
                    )
                    .to_compile_error()
                    .into();
                }
                let mut m = method.clone();
                m.attrs.retain(|a| !a.path().is_ident("started"));
                m.vis = syn::Visibility::Inherited;
                m.sig.ident = format_ident!("started");
                if m.sig.asyncness.is_some() {
                    has_async = true;
                }
                started_method = Some(m);
                continue;
            }

            if is_stopped {
                if stopped_method.is_some() {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "only one #[stopped] method is allowed per actor",
                    )
                    .to_compile_error()
                    .into();
                }
                if method.attrs.iter().any(|a| {
                    a.path().is_ident("handler")
                        || a.path().is_ident("send_handler")
                        || a.path().is_ident("request_handler")
                }) {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[stopped] cannot be combined with handler attributes",
                    )
                    .to_compile_error()
                    .into();
                }
                // Expect: fn stopped(&mut self, ctx: &Context<Self>)
                if method.sig.inputs.len() != 2 {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[stopped] method must take exactly (&mut self, &Context<Self>)",
                    )
                    .to_compile_error()
                    .into();
                }
                if !matches!(method.sig.inputs.first(), Some(FnArg::Receiver(r)) if r.reference.is_some() && r.mutability.is_some())
                {
                    return syn::Error::new_spanned(
                        &method.sig,
                        "#[stopped] method's first parameter must be `&mut self`",
                    )
                    .to_compile_error()
                    .into();
                }
                let mut m = method.clone();
                m.attrs.retain(|a| !a.path().is_ident("stopped"));
                m.vis = syn::Visibility::Inherited;
                m.sig.ident = format_ident!("stopped");
                if m.sig.asyncness.is_some() {
                    has_async = true;
                }
                stopped_method = Some(m);
                continue;
            }
        }
        items_to_keep.push(item);
    }
    impl_block.items = items_to_keep;

    // --- Process handler methods ---
    let mut handler_impls = Vec::new();

    for item in &mut impl_block.items {
        if let ImplItem::Fn(method) = item {
            let handler_idx = method.attrs.iter().position(|attr| {
                attr.path().is_ident("handler")
                    || attr.path().is_ident("send_handler")
                    || attr.path().is_ident("request_handler")
            });

            if let Some(idx) = handler_idx {
                method.attrs.remove(idx);
                if let Some(extra) = method.attrs.iter().find(|a| is_handler_attribute(a)) {
                    return syn::Error::new_spanned(
                        extra,
                        "a method can carry only one of #[handler], #[send_handler] and #[request_handler]",
                    )
                    .to_compile_error()
                    .into();
                }
                if !method.sig.generics.params.is_empty() {
                    return syn::Error::new_spanned(
                        &method.sig.generics,
                        "handler methods cannot be generic; write one handler per message type",
                    )
                    .to_compile_error()
                    .into();
                }

                // Collect remaining attributes (e.g. #[cfg(...)]) to propagate
                // to the generated Handler impl block.
                let extra_attrs: Vec<_> = method
                    .attrs
                    .iter()
                    .filter(|a| {
                        !a.path().is_ident("handler")
                            && !a.path().is_ident("send_handler")
                            && !a.path().is_ident("request_handler")
                            && !a.path().is_ident("started")
                            && !a.path().is_ident("stopped")
                    })
                    .cloned()
                    .collect();

                let method_name = &method.sig.ident;
                if method.sig.asyncness.is_some() {
                    has_async = true;
                }

                // Validate handler has exactly 3 parameters: &mut self, msg, ctx
                let param_count = method.sig.inputs.len();
                if param_count != 3 {
                    return syn::Error::new_spanned(
                        &method.sig,
                        format!(
                            "handler method must have 3 parameters (&mut self, msg: M, ctx: &Context<Self>), found {param_count}"
                        ),
                    )
                    .to_compile_error()
                    .into();
                }

                // `&self` is accepted too; a missing or by-value receiver is not.
                match method.sig.inputs.first() {
                    Some(FnArg::Receiver(r))
                        if r.reference.is_some() || r.colon_token.is_some() => {}
                    Some(first) => {
                        return syn::Error::new_spanned(
                            first,
                            "handler method's first parameter must be `&mut self`",
                        )
                        .to_compile_error()
                        .into();
                    }
                    None => unreachable!("three parameters were checked above"),
                }
                if let Some(FnArg::Typed(ctx)) = method.sig.inputs.iter().nth(2) {
                    if !matches!(&*ctx.ty, Type::Reference(r) if r.mutability.is_none()) {
                        return syn::Error::new_spanned(
                            &ctx.ty,
                            "handler context parameter must be `&Context<Self>`",
                        )
                        .to_compile_error()
                        .into();
                    }
                }

                // Extract message type from 2nd parameter (index 1, after &mut self)
                let msg_ty = match method.sig.inputs.iter().nth(1) {
                    Some(FnArg::Typed(pat_type)) => &*pat_type.ty,
                    _ => {
                        return syn::Error::new_spanned(
                            &method.sig,
                            "handler method must have signature: fn(&mut self, msg: M, ctx: &Context<Self>) -> R",
                        )
                        .to_compile_error()
                        .into();
                    }
                };

                // Keep an omitted return type omitted: rustc then suggests
                // adding `-> R` after the user's signature for a request reply.
                let output = match &method.sig.output {
                    ReturnType::Default => quote! {},
                    ReturnType::Type(_, ty) => quote! { -> #ty },
                };
                // The parameter list carries the user's parentheses span, so
                // that suggestion lands after the user's `)`.
                let mut params = proc_macro2::Group::new(
                    proc_macro2::Delimiter::Parenthesis,
                    quote! { &mut self, msg: #msg_ty, ctx: &Context<Self> },
                );
                params.set_span(method.sig.paren_token.span.join());
                // Span the generated impl at the method so trait errors
                // (conflicting impls, signature mismatches) point at it.
                let span = method.sig.span();

                let handler_impl = if method.sig.asyncness.is_some() {
                    quote::quote_spanned! { span =>
                        #(#extra_attrs)*
                        impl #impl_generics Handler<#msg_ty> for #self_ty #where_clause {
                            async fn handle #params #output {
                                self.#method_name(msg, ctx).await
                            }
                        }
                    }
                } else {
                    quote::quote_spanned! { span =>
                        #(#extra_attrs)*
                        impl #impl_generics Handler<#msg_ty> for #self_ty #where_clause {
                            fn handle #params #output {
                                self.#method_name(msg, ctx)
                            }
                        }
                    }
                };

                handler_impls.push(handler_impl);
            }
        }
    }

    // --- Generate impl Actor ---
    let lifecycle_methods: Vec<&ImplItemFn> = [
        started_method.as_ref(),
        stopped_method.as_ref(),
        exit_method.as_ref(),
    ]
    .into_iter()
    .flatten()
    .collect();

    let protocol_doc = if bridge_traits.is_empty() {
        quote! {}
    } else {
        let lines: Vec<String> = bridge_traits.iter().map(|t| format!("- [`{t}`]")).collect();
        let doc_body = format!(
            "# Protocol\n\n\
             When started, `ActorRef<{ty}>` implements:\n\n\
             {lines}\n\n\
             See the protocol trait docs for the full API.",
            ty = quote!(#self_ty),
            lines = lines.join("\n"),
        );
        quote! { #[doc = #doc_body] }
    };

    let actor_impl = quote! {
        #protocol_doc
        impl #impl_generics Actor for #self_ty #where_clause {
            #(#lifecycle_methods)*
        }
    };

    // --- Generate bridge assertions ---
    let runtime_path = if has_async {
        quote! { spawned_concurrency::tasks }
    } else {
        quote! { spawned_concurrency::threads }
    };

    let bridge_asserts: Vec<_> = bridge_traits
        .iter()
        .map(|trait_name| {
            quote! {
                const _: () = {
                    fn _assert_bridge<__T: #trait_name>() {}
                    fn _check #impl_generics () #where_clause {
                        _assert_bridge::<#runtime_path::ActorRef<#self_ty>>();
                    }
                };
            }
        })
        .collect();

    let output = quote! {
        #actor_impl
        #impl_block
        #(#handler_impls)*
        #(#bridge_asserts)*
        #lints
    };

    output.into()
}

// --- #[actor] lints ---
//
// `#[actor]` sees only the impl block it annotates, not the struct, so the
// shared-state lint is syntactic over what that block contains: constructor
// and handler signatures, `Self { .. }` literals, `let` types and
// `Arc::new(Mutex::new(..))`-style expressions. Proc-macro diagnostics are
// unstable, so each warning is a use of a `#[deprecated]` item spanned at the
// offending tokens: rustc prints the note there, `-D warnings` makes it an
// error, and nothing reaches the compiled program.

const SHARED_STATE_NOTE: &str = "actors should own their state: shared mutable state \
(`Arc<Mutex<_>>`, `Arc<RwLock<_>>`, `Arc<Atomic*>`, `Rc<RefCell<_>>`) breaks the isolation \
supervision relies on (a restarted actor inherits whatever the old one left behind the lock), \
and a panic while holding a std lock poisons it for every holder; send messages to the owning \
actor instead (or, once available, read a published snapshot, ROADMAP rank 10); if the \
sharing is deliberate, use `#[actor(allow_shared_state)]`";

const PANIC_ABORT_NOTE: &str = "built with `panic = \"abort\"`: a panicking handler aborts \
the whole process instead of stopping one actor, so supervisors, links, monitors and \
`stopped()` never see the failure; build actor binaries with `panic = \"unwind\"` (the \
default), or use `#[actor(allow_panic_abort)]` if aborting is deliberate";

/// Smart pointers that share their contents between owners.
fn is_shared_pointer(ident: &Ident) -> bool {
    ident == "Arc" || ident == "Rc"
}

/// Interior-mutable types from std, tokio, parking_lot and similar crates,
/// matched by their last path segment.
fn is_interior_mutable(ident: &Ident) -> bool {
    ident == "Mutex"
        || ident == "RwLock"
        || ident == "RefCell"
        || ident.to_string().starts_with("Atomic")
}

/// `Arc<Mutex<_>>`, `std::sync::Arc<tokio::sync::RwLock<_>>`, `Rc<RefCell<_>>`, ...
fn is_shared_mutable_type(path: &syn::Path) -> bool {
    let Some(outer) = path.segments.last() else {
        return false;
    };
    if !is_shared_pointer(&outer.ident) {
        return false;
    }
    let PathArguments::AngleBracketed(args) = &outer.arguments else {
        return false;
    };
    matches!(
        args.args.first(),
        Some(GenericArgument::Type(Type::Path(inner)))
            if inner.path.segments.last().is_some_and(|s| is_interior_mutable(&s.ident))
    )
}

/// The type segment of an associated call such as `Mutex::new` or
/// `std::sync::Arc::<T>::new`.
fn call_type_segment(func: &syn::Expr) -> Option<&Ident> {
    let syn::Expr::Path(path) = func else {
        return None;
    };
    let segments = &path.path.segments;
    (segments.len() >= 2).then(|| &segments[segments.len() - 2].ident)
}

/// `Arc::new(Mutex::new(..))`, `Rc::new(RefCell::new(..))`,
/// `Arc::new(AtomicU64::default())`, ...
fn is_shared_mutable_constructor(call: &syn::ExprCall) -> bool {
    let syn::Expr::Path(func) = &*call.func else {
        return false;
    };
    let wraps_in_pointer = func.path.segments.last().is_some_and(|s| s.ident == "new")
        && call_type_segment(&call.func).is_some_and(is_shared_pointer)
        && call.args.len() == 1;
    wraps_in_pointer
        && matches!(
            call.args.first(),
            Some(syn::Expr::Call(inner))
                if call_type_segment(&inner.func).is_some_and(is_interior_mutable)
        )
}

#[derive(Default)]
struct SharedStateFinder {
    spans: Vec<proc_macro2::Span>,
}

impl<'ast> syn::visit::Visit<'ast> for SharedStateFinder {
    fn visit_type_path(&mut self, ty: &'ast TypePath) {
        if is_shared_mutable_type(&ty.path) {
            self.spans.push(ty.span());
        } else {
            syn::visit::visit_type_path(self, ty);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if is_shared_mutable_constructor(call) {
            self.spans.push(call.span());
        } else {
            syn::visit::visit_expr_call(self, call);
        }
    }
}

/// A warning at `span`: the use of a local `#[deprecated]` unit struct.
fn deprecation_warning(name: &str, note: &str, span: proc_macro2::Span) -> proc_macro2::TokenStream {
    let name = Ident::new(name, span);
    quote::quote_spanned! { span =>
        const _: () = {
            #[deprecated(note = #note)]
            struct #name;
            let _ = #name;
        };
    }
}

/// Set once the panic-abort warning has been emitted in this compilation. A
/// proc macro is loaded once per rustc invocation, so this makes the warning
/// once per crate rather than once per actor. If a compiler ever reloaded the
/// macro, the warning would only repeat.
static PANIC_ABORT_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn actor_lints(
    impl_block: &ItemImpl,
    allow_shared_state: bool,
    allow_panic_abort: bool,
) -> proc_macro2::TokenStream {
    let mut lints = proc_macro2::TokenStream::new();
    if !allow_shared_state {
        let mut finder = SharedStateFinder::default();
        syn::visit::Visit::visit_item_impl(&mut finder, impl_block);
        for span in finder.spans {
            lints.extend(deprecation_warning(
                "SharedMutableStateInActor",
                SHARED_STATE_NOTE,
                span,
            ));
        }
    }
    if !allow_panic_abort
        && !PANIC_ABORT_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        let warning = deprecation_warning(
            "PanicAbortInActorCrate",
            PANIC_ABORT_NOTE,
            proc_macro2::Span::call_site(),
        );
        lints.extend(quote! {
            #[cfg(panic = "abort")]
            #warning
        });
    }
    lints
}

// --- #[sim_test] ---

/// Seeded async test: on Tokio in normal builds, over a seed range on the
/// deterministic simulator under `--cfg spawned_sim` (see
/// `spawned_rt::tasks::SimTest`). Accepts `#[tokio::test]`'s `flavor` and
/// `worker_threads` (used only on Tokio), `seeds = N` (simulation seeds
/// `0..N`), `explore = "pct:d=3"` (simulation scheduling strategy: `random`,
/// `pct[:d=D][,k=K]` or `dfs[:bound=B][,depth=N]`) and `iterations = N` (runs
/// for that strategy). The function may take one `u64` argument, the seed.
///
/// ```ignore
/// #[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
/// async fn restarts_child(seed: u64) { /* ... */ }
///
/// #[spawned_macros::sim_test(explore = "pct:d=3", iterations = 1000)]
/// async fn no_lost_update() { /* ... */ }
/// ```
#[proc_macro_attribute]
pub fn sim_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let options = parse_macro_input!(attr with syn::punctuated::Punctuated::<syn::MetaNameValue, syn::Token![,]>::parse_terminated);
    let function = parse_macro_input!(item as syn::ItemFn);
    let mut multi_thread = false;
    let mut workers: Option<syn::Expr> = None;
    let mut seeds: Option<syn::Expr> = None;
    let mut explore: Option<syn::Expr> = None;
    let mut iterations: Option<syn::Expr> = None;
    for option in options {
        let key = option
            .path
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        match key.as_str() {
            "flavor" => {
                let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(flavor),
                    ..
                }) = &option.value
                else {
                    return syn::Error::new_spanned(&option.value, "expected a string flavor")
                        .to_compile_error()
                        .into();
                };
                match flavor.value().as_str() {
                    "multi_thread" => multi_thread = true,
                    "current_thread" => multi_thread = false,
                    other => {
                        return syn::Error::new_spanned(flavor, format!("unknown flavor {other}"))
                            .to_compile_error()
                            .into()
                    }
                }
            }
            "worker_threads" => workers = Some(option.value),
            "seeds" => seeds = Some(option.value),
            "explore" => explore = Some(option.value),
            "iterations" => iterations = Some(option.value),
            _ => {
                return syn::Error::new_spanned(
                    &option.path,
                    "sim_test accepts flavor, worker_threads, seeds, explore and iterations",
                )
                .to_compile_error()
                .into()
            }
        }
    }
    if function.sig.asyncness.is_none() {
        return syn::Error::new_spanned(function.sig.fn_token, "sim_test needs an async fn")
            .to_compile_error()
            .into();
    }
    if !matches!(function.sig.output, ReturnType::Default) {
        return syn::Error::new_spanned(&function.sig.output, "sim_test functions return ()")
            .to_compile_error()
            .into();
    }
    let seed = match function.sig.inputs.len() {
        0 => quote! { _ },
        1 => match &function.sig.inputs[0] {
            FnArg::Typed(arg) => {
                let pat = &arg.pat;
                quote! { #pat }
            }
            FnArg::Receiver(r) => {
                return syn::Error::new_spanned(r, "sim_test cannot take self")
                    .to_compile_error()
                    .into()
            }
        },
        _ => {
            return syn::Error::new_spanned(
                &function.sig.inputs,
                "sim_test takes at most one argument, the u64 seed",
            )
            .to_compile_error()
            .into()
        }
    };
    let attrs = &function.attrs;
    let vis = &function.vis;
    let name = &function.sig.ident;
    let body = &function.block;
    let flavor = if multi_thread {
        let workers = workers.unwrap_or_else(|| syn::parse_quote!(2));
        quote! { .multi_thread(#workers) }
    } else {
        quote! {}
    };
    let seeds = seeds.map(|n| quote! { .seeds(0..(#n)) });
    let explore = explore.map(|s| quote! { .explore(#s) });
    let iterations = iterations.map(|n| quote! { .iterations(#n) });
    quote! {
        #(#attrs)*
        #[test]
        #vis fn #name() {
            ::spawned_rt::tasks::SimTest::new()
                .named(env!("CARGO_PKG_NAME"), module_path!(), stringify!(#name))
                #flavor
                #seeds
                #explore
                #iterations
                .run(|#seed: u64| async move #body)
        }
    }
    .into()
}
