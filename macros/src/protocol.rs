//! `#[protocol]`: message structs, blanket protocol impls and request plumbing from a trait.
use super::*;

// --- Helpers for #[protocol] ---

pub(super) fn to_snake_case(s: &str) -> String {
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

pub(super) fn to_pascal_case(s: &str) -> String {
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

pub(super) fn strip_protocol_suffix(name: &str) -> String {
    name.strip_suffix("Protocol").unwrap_or(name).to_string()
}

// A method-level cfg_attr has not been expanded when the enclosing protocol
// macro runs. Reject conditional timeout attributes even for false predicates;
// otherwise Rust sees an unknown attribute, or silently omits the timeout.
pub(super) fn conditional_timeout(meta: &syn::Meta) -> bool {
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

pub(super) enum MethodKind {
    Send,
    Request(Box<Type>),
}

#[derive(Clone, Copy)]
pub(super) enum RuntimeMode {
    Tasks,
    Threads,
}

pub(super) fn classify_return_type(ret: &ReturnType) -> Result<MethodKind, &Type> {
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

pub(super) fn extract_response_inner(ty: &Type) -> Option<Box<Type>> {
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

pub(super) fn extract_result_inner(ty: &Type) -> Option<Box<Type>> {
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
pub(super) fn unsupported_param_type(ty: &Type) -> Option<syn::Error> {
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

pub(super) fn is_unit_type(ty: &Type) -> bool {
    if let Type::Tuple(tuple) = ty {
        return tuple.elems.is_empty();
    }
    false
}

/// Returns true for types available without explicit import (prelude + primitives).
/// These can't be accessed via `super::` so must be left unqualified.
pub(super) fn is_prelude_or_primitive(name: &str) -> bool {
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
pub(super) fn qualify_type_with_super(ty: &Type) -> Type {
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
pub(super) fn qualify_path_with_super(path: &mut syn::Path) {
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

pub(super) struct ProtocolInfo<'a> {
    pub(super) trait_name: &'a Ident,
    pub(super) mod_name: &'a Ident,
    pub(super) ref_name: &'a Ident,
    pub(super) converter_trait: &'a Ident,
    pub(super) converter_method: &'a Ident,
    pub(super) target_trait: &'a Ident,
}

/// One transport signature used in declarations, runtime impls and delegation.
pub(super) fn transport_signature(
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
pub(super) fn generate_target_impl(
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
                // The timeout was chosen before the message was built; the
                // deadline is fixed now, at send, and travels in the slot.
                RuntimeMode::Tasks => quote! { spawned_concurrency::Response::from(target.send_request(message, spawned_concurrency::Deadline::after(timeout))) },
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
pub(super) fn generate_protocol_adapters(
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

pub(super) struct ProtocolMethodInfo {
    pub(super) method_name: Ident,
    pub(super) struct_name: Ident,
    pub(super) field_names: Vec<Ident>,
    pub(super) field_types: Vec<Type>,
    pub(super) kind: MethodKind,
    pub(super) params: Vec<FnArg>,
    pub(super) ret_type: ReturnType,
    /// Doc + cfg attributes to propagate to blanket impl methods.
    pub(super) method_attrs: Vec<Attribute>,
    pub(super) cfg_attrs: Vec<Attribute>,
    pub(super) timeout: syn::Expr,
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
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
