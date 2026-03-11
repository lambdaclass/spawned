use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::Parse, parse_macro_input, Attribute, FnArg, GenericArgument, Ident, ImplItem,
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
                if matches!(s.as_str(), "crate" | "super" | "self" | "std" | "core" | "alloc") {
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
}

/// Generates a blanket `impl Protocol for ActorRef<A>` and `impl ToXRef for ActorRef<A>`
/// for a given runtime path (tasks or threads).
///
/// For cfg-gated methods, we generate marker traits that conditionally require
/// the Handler bounds. This avoids putting extra where-clause bounds on individual
/// methods (which Rust rejects as "impl has stricter requirements than trait").
fn generate_blanket_impl(
    info: &ProtocolInfo,
    methods: &[ProtocolMethodInfo],
    runtime_path: &proc_macro2::TokenStream,
    mode: RuntimeMode,
) -> proc_macro2::TokenStream {
    let ProtocolInfo { trait_name, mod_name, ref_name, converter_trait, converter_method } = info;

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

    // Generate method implementations (no where clauses needed — bounds come from
    // marker traits or the impl block).
    let method_impls: Vec<_> = methods
        .iter()
        .map(|m| {
            let method_name = &m.method_name;
            let field_names = &m.field_names;
            let params: Vec<_> = m.params.iter().collect();
            let ret_ty = &m.ret_type;
            let method_attrs = &m.method_attrs;

            let struct_name = &m.struct_name;
            let msg_construct = if field_names.is_empty() {
                quote! { #mod_name::#struct_name }
            } else {
                quote! { #mod_name::#struct_name { #(#field_names),* } }
            };

            match &m.kind {
                MethodKind::Send => {
                    let is_unit_return = match ret_ty {
                        ReturnType::Default => true,
                        ReturnType::Type(_, ty) => is_unit_type(ty),
                    };
                    let body = if is_unit_return {
                        quote! { let _ = self.send(#msg_construct); }
                    } else {
                        quote! { self.send(#msg_construct) }
                    };
                    quote! {
                        #(#method_attrs)*
                        fn #method_name(&self, #(#params),*) #ret_ty {
                            #body
                        }
                    }
                }
                MethodKind::Request(_) => {
                    let body = match mode {
                        RuntimeMode::Tasks => quote! {
                            spawned_concurrency::Response::from(
                                self.request_raw(#msg_construct),
                            )
                        },
                        RuntimeMode::Threads => quote! {
                            spawned_concurrency::Response::ready(
                                self.request(#msg_construct),
                            )
                        },
                    };
                    quote! {
                        #(#method_attrs)*
                        fn #method_name(&self, #(#params),*) #ret_ty {
                            #body
                        }
                    }
                }
            }
        })
        .collect();

    quote! {
        #(#marker_trait_defs)*

        impl<__A: #runtime_path::Actor #(+ #handler_bounds)* #(#marker_trait_bounds)*> #trait_name
            for #runtime_path::ActorRef<__A>
        {
            #(#method_impls)*
        }

        impl<__A: #runtime_path::Actor #(+ #handler_bounds)* #(#marker_trait_bounds)*> #converter_trait
            for #runtime_path::ActorRef<__A>
        {
            fn #converter_method(&self) -> #ref_name {
                ::std::sync::Arc::new(self.clone())
            }
        }
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
/// | `Response<T>` | Request | Both | `.await.unwrap()` (tasks, no timeout) / `.unwrap()` (threads, 5s timeout) |
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
#[proc_macro_attribute]
pub fn protocol(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let trait_def = parse_macro_input!(item as ItemTrait);
    let trait_name = &trait_def.ident;
    let trait_vis = &trait_def.vis;

    if !trait_def.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &trait_def.generics,
            "generic type parameters on protocol traits are not supported",
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

    for item in &trait_def.items {
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

            let method_name = method.sig.ident.clone();
            let struct_name = format_ident!("{}", to_pascal_case(&method_name.to_string()));

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

    // Always generate blanket impls for both runtimes
    let tasks = quote! { spawned_concurrency::tasks };
    let threads = quote! { spawned_concurrency::threads };
    let proto_info = ProtocolInfo {
        trait_name,
        mod_name: &mod_name,
        ref_name: &ref_name,
        converter_trait: &converter_trait,
        converter_method: &converter_method,
    };
    let tasks_impl = generate_blanket_impl(&proto_info, &methods, &tasks, RuntimeMode::Tasks);
    let threads_impl = generate_blanket_impl(&proto_info, &methods, &threads, RuntimeMode::Threads);
    let blanket_impls = quote! { #tasks_impl #threads_impl };

    let ref_doc = format!(
        "Type-erased reference to any actor implementing [`{trait_name}`].\n\n\
         Use this type to store protocol references without depending on the concrete actor type."
    );

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
/// For multiple protocols:
/// ```ignore
/// #[actor(protocol(RoomProtocol, UserProtocol))]
/// impl ChatUser { /* ... */ }
/// ```
///
/// # Handler Attributes
///
/// | Attribute | Use for |
/// |-----------|---------|
/// | `#[request_handler]` | Messages that expect a reply (`Response<T>` or `Result<T, ActorError>`) |
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

    let self_ty = &impl_block.self_ty;
    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

    // --- Parse named parameters from #[actor(protocol = X)] or #[actor(protocol(X, Y))] ---
    let bridge_traits: Vec<Ident> = if attr.is_empty() {
        Vec::new()
    } else {
        let parser = |input: syn::parse::ParseStream| -> syn::Result<Vec<Ident>> {
            let mut protocols = Vec::new();
            while !input.is_empty() {
                let key: Ident = input.parse()?;
                if key != "protocol" {
                    return Err(syn::Error::new(key.span(), "unknown parameter, expected `protocol`"));
                }
                if input.peek(syn::Token![=]) {
                    // protocol = TraitName
                    let _: syn::Token![=] = input.parse()?;
                    protocols.push(input.parse()?);
                } else {
                    // protocol(Trait1, Trait2)
                    let content;
                    syn::parenthesized!(content in input);
                    let punctuated = content.parse_terminated(Ident::parse, syn::Token![,])?;
                    protocols.extend(punctuated);
                }
                if input.peek(syn::Token![,]) {
                    let _: syn::Token![,] = input.parse()?;
                }
            }
            Ok(protocols)
        };
        match syn::parse::Parser::parse(parser, attr) {
            Ok(traits) => traits,
            Err(e) => return e.to_compile_error().into(),
        }
    };

    // --- Extract #[started] and #[stopped] lifecycle methods ---
    let mut started_method: Option<ImplItemFn> = None;
    let mut stopped_method: Option<ImplItemFn> = None;
    let mut has_async = false;

    let mut items_to_keep = Vec::new();
    for item in impl_block.items.drain(..) {
        if let ImplItem::Fn(ref method) = item {
            let is_started = method.attrs.iter().any(|a| a.path().is_ident("started"));
            let is_stopped = method.attrs.iter().any(|a| a.path().is_ident("stopped"));

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
                if !matches!(method.sig.inputs.first(), Some(FnArg::Receiver(r)) if r.mutability.is_some()) {
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
                if !matches!(method.sig.inputs.first(), Some(FnArg::Receiver(r)) if r.mutability.is_some()) {
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

                // Collect remaining attributes (e.g. #[cfg(...)]) to propagate
                // to the generated Handler impl block.
                let extra_attrs: Vec<_> = method.attrs.iter().filter(|a| {
                    !a.path().is_ident("handler")
                        && !a.path().is_ident("send_handler")
                        && !a.path().is_ident("request_handler")
                        && !a.path().is_ident("started")
                        && !a.path().is_ident("stopped")
                }).cloned().collect();

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

                // Extract return type (default to () if omitted)
                let ret_ty: Box<Type> = match &method.sig.output {
                    ReturnType::Default => syn::parse_quote! { () },
                    ReturnType::Type(_, ty) => ty.clone(),
                };

                let handler_impl = if method.sig.asyncness.is_some() {
                    quote! {
                        #(#extra_attrs)*
                        impl #impl_generics Handler<#msg_ty> for #self_ty #where_clause {
                            async fn handle(&mut self, msg: #msg_ty, ctx: &Context<Self>) -> #ret_ty {
                                self.#method_name(msg, ctx).await
                            }
                        }
                    }
                } else {
                    quote! {
                        #(#extra_attrs)*
                        impl #impl_generics Handler<#msg_ty> for #self_ty #where_clause {
                            fn handle(&mut self, msg: #msg_ty, ctx: &Context<Self>) -> #ret_ty {
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
    let lifecycle_methods: Vec<&ImplItemFn> = [started_method.as_ref(), stopped_method.as_ref()]
        .into_iter()
        .flatten()
        .collect();

    let protocol_doc = if bridge_traits.is_empty() {
        quote! {}
    } else {
        let lines: Vec<String> = bridge_traits
            .iter()
            .map(|t| format!("- [`{t}`]"))
            .collect();
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
                    fn _check() {
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
    };

    output.into()
}
