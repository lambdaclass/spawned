use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::Parse, parse_macro_input, Attribute, FnArg, GenericArgument, Ident, ImplItem,
    ImplItemFn, ItemImpl, ItemTrait, Pat, PathArguments, ReturnType, TraitItem, Type, TypePath,
};

// --- Helpers for #[protocol] ---

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                result.push('_');
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
    AsyncRequest(Box<Type>),
    SyncRequest(Box<Type>),
}

fn classify_return_type(ret: &ReturnType) -> MethodKind {
    match ret {
        ReturnType::Default => MethodKind::Send,
        ReturnType::Type(_, ty) => {
            if let Some(inner) = extract_response_inner(ty) {
                return MethodKind::AsyncRequest(inner);
            }
            if let Some(inner) = extract_result_inner(ty) {
                if is_unit_type(&inner) {
                    return MethodKind::Send;
                }
                return MethodKind::SyncRequest(inner);
            }
            MethodKind::Send
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
                if s == "crate" || s == "super" || s == "self" {
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
        _ => ty.clone(),
    }
}

/// Generates a blanket `impl Protocol for ActorRef<A>` and `impl ToXRef for ActorRef<A>`
/// for a given runtime path (tasks or threads).
fn generate_blanket_impl(
    trait_name: &Ident,
    mod_name: &Ident,
    ref_name: &Ident,
    converter_trait: &Ident,
    converter_method: &Ident,
    methods: &[ProtocolMethodInfo],
    runtime_path: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let handler_bounds: Vec<_> = methods
        .iter()
        .map(|m| {
            let sn = &m.struct_name;
            quote! { #runtime_path::Handler<#mod_name::#sn> }
        })
        .collect();

    let method_impls: Vec<_> = methods
        .iter()
        .map(|m| {
            let method_name = &m.method_name;
            let field_names = &m.field_names;
            let params: Vec<_> = m.params.iter().collect();
            let ret_ty = &m.ret_type;
            let doc_attrs = &m.doc_attrs;

            let struct_name = &m.struct_name;
            let msg_construct = if field_names.is_empty() {
                quote! { #mod_name::#struct_name }
            } else {
                quote! { #mod_name::#struct_name { #(#field_names),* } }
            };

            match &m.kind {
                MethodKind::Send => {
                    quote! {
                        #(#doc_attrs)*
                        fn #method_name(&self, #(#params),*) #ret_ty {
                            self.send(#msg_construct)
                        }
                    }
                }
                MethodKind::AsyncRequest(_) => {
                    quote! {
                        #(#doc_attrs)*
                        fn #method_name(&self, #(#params),*) #ret_ty {
                            spawned_concurrency::tasks::Response::from(
                                self.request_raw(#msg_construct),
                            )
                        }
                    }
                }
                MethodKind::SyncRequest(_) => {
                    quote! {
                        #(#doc_attrs)*
                        fn #method_name(&self, #(#params),*) #ret_ty {
                            self.request(#msg_construct)
                        }
                    }
                }
            }
        })
        .collect();

    quote! {
        impl<__A: #runtime_path::Actor #(+ #handler_bounds)*> #trait_name
            for #runtime_path::ActorRef<__A>
        {
            #(#method_impls)*
        }

        impl<__A: #runtime_path::Actor #(+ #handler_bounds)*> #converter_trait
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
    doc_attrs: Vec<Attribute>,
}

#[proc_macro_attribute]
pub fn protocol(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let trait_def = parse_macro_input!(item as ItemTrait);
    let trait_name = &trait_def.ident;
    let trait_vis = &trait_def.vis;

    let base_name = strip_protocol_suffix(&trait_name.to_string());
    let mod_name = format_ident!("{}", to_snake_case(&trait_name.to_string()));
    let ref_name = format_ident!("{}Ref", base_name);
    let converter_trait = format_ident!("To{}Ref", base_name);
    let converter_method = format_ident!("to_{}_ref", to_snake_case(&base_name));

    let mut methods: Vec<ProtocolMethodInfo> = Vec::new();
    let mut has_async_request = false;
    let mut has_sync_request = false;

    for item in &trait_def.items {
        if let TraitItem::Fn(method) = item {
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
                    }
                }
                params.push(arg.clone());
            }

            let kind = classify_return_type(&method.sig.output);
            match &kind {
                MethodKind::AsyncRequest(_) => has_async_request = true,
                MethodKind::SyncRequest(_) => has_sync_request = true,
                MethodKind::Send => {}
            }

            let doc_attrs: Vec<Attribute> = method
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("doc"))
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
                doc_attrs,
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
            let doc_attrs = &m.doc_attrs;
            let msg_result_ty: Type = match &m.kind {
                MethodKind::Send => syn::parse_quote! { () },
                MethodKind::AsyncRequest(inner) | MethodKind::SyncRequest(inner) => {
                    qualify_type_with_super(inner)
                }
            };

            if field_names.is_empty() {
                quote! {
                    #(#doc_attrs)*
                    #[derive(Clone)]
                    pub struct #struct_name;
                    impl Message for #struct_name {
                        type Result = #msg_result_ty;
                    }
                }
            } else {
                quote! {
                    #(#doc_attrs)*
                    #[derive(Clone)]
                    pub struct #struct_name {
                        #(pub #field_names: #qualified_field_types,)*
                    }
                    impl Message for #struct_name {
                        type Result = #msg_result_ty;
                    }
                }
            }
        })
        .collect();

    // Generate blanket impls based on protocol mode
    let blanket_impls = if has_async_request {
        let tasks = quote! { spawned_concurrency::tasks };
        generate_blanket_impl(
            trait_name,
            &mod_name,
            &ref_name,
            &converter_trait,
            &converter_method,
            &methods,
            &tasks,
        )
    } else if has_sync_request {
        let threads = quote! { spawned_concurrency::threads };
        generate_blanket_impl(
            trait_name,
            &mod_name,
            &ref_name,
            &converter_trait,
            &converter_method,
            &methods,
            &threads,
        )
    } else {
        // Send-only: generate for both runtimes
        let tasks = quote! { spawned_concurrency::tasks };
        let threads = quote! { spawned_concurrency::threads };
        let tasks_impl = generate_blanket_impl(
            trait_name,
            &mod_name,
            &ref_name,
            &converter_trait,
            &converter_method,
            &methods,
            &tasks,
        );
        let threads_impl = generate_blanket_impl(
            trait_name,
            &mod_name,
            &ref_name,
            &converter_trait,
            &converter_method,
            &methods,
            &threads,
        );
        quote! { #tasks_impl #threads_impl }
    };

    let ref_doc = format!(
        "Type-erased reference to any actor implementing [`{trait_name}`].\n\n\
         Use this type to store protocol references without depending on the concrete actor type."
    );

    let output = quote! {
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

/// Attribute macro for actor impl blocks.
///
/// Generates `impl Actor for T` automatically. Use `#[started]` and `#[stopped]`
/// on methods to override lifecycle callbacks.
///
/// Use `protocol = TraitName` to assert the actor implements a protocol:
/// ```ignore
/// #[actor(protocol = RoomProtocol)]
/// ```
/// For multiple protocols use the list form:
/// ```ignore
/// #[actor(protocol(RoomProtocol, AnotherProtocol))]
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
