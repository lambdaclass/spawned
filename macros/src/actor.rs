//! `#[actor]`: `Actor` and `Handler` impls from an annotated impl block.
use super::*;

// Only inline a body whose suspension and wrapping behavior is visible here.
// Unknown macros/attributes can introduce awaits or wrappers after expansion.
pub(super) fn await_free_handler(method: &ImplItemFn) -> bool {
    fn inert_attribute(attr: &Attribute) -> bool {
        ["cfg", "doc", "allow", "warn", "deny", "forbid", "expect"]
            .iter()
            .any(|name| attr.path().is_ident(name))
    }
    // An explicit/shared receiver can carry different borrow promises than
    // Handler's &mut self. Keep its original call adapter unchanged.
    let ordinary_mut_receiver = matches!(method.sig.inputs.first(),
        Some(FnArg::Receiver(r)) if r.attrs.is_empty()
            && r.mutability.is_some()
            && r.reference.as_ref().is_some_and(|(_, lifetime)| lifetime.is_none())
            && r.colon_token.is_none());
    let plain_parameters = method.sig.inputs.iter().all(|p| match p {
        FnArg::Receiver(r) => r.attrs.is_empty(),
        FnArg::Typed(p) => p.attrs.is_empty(),
    });
    if !ordinary_mut_receiver
        || !plain_parameters
        || !method.sig.generics.params.is_empty()
        || method.sig.generics.where_clause.is_some()
        || method.sig.asyncness.is_none()
        || method.sig.unsafety.is_some()
        || method.sig.abi.is_some()
        || !method.attrs.iter().all(inert_attribute)
    {
        return false;
    }
    struct Check {
        blocked: bool,
    }
    impl<'ast> syn::visit::Visit<'ast> for Check {
        fn visit_expr_await(&mut self, _: &'ast syn::ExprAwait) {
            self.blocked = true;
        }
        fn visit_expr_yield(&mut self, _: &'ast syn::ExprYield) {
            self.blocked = true;
        }
        fn visit_macro(&mut self, _: &'ast syn::Macro) {
            self.blocked = true;
        }
        fn visit_item(&mut self, _: &'ast syn::Item) {
            // A copied static (including one inside a nested function) has
            // a different identity from the inherent method's original.
            self.blocked = true;
        }
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {
            // Anonymous types also have observable identity through TypeId.
            self.blocked = true;
        }
        fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {
            self.blocked = true;
        }
        fn visit_type_impl_trait(&mut self, _: &'ast syn::TypeImplTrait) {
            self.blocked = true;
        }
        fn visit_attribute(&mut self, attr: &'ast Attribute) {
            if !inert_attribute(attr) {
                self.blocked = true;
            }
        }
    }
    let mut check = Check { blocked: false };
    syn::visit::Visit::visit_block(&mut check, &method.block);
    syn::visit::Visit::visit_signature(&mut check, &method.sig);
    !check.blocked
}

pub(super) fn is_handler_attribute(attr: &Attribute) -> bool {
    let path = attr.path();
    path.is_ident("handler")
        || path.is_ident("send_handler")
        || path.is_ident("request_handler")
        || path.is_ident("deferred_handler")
}

pub(super) fn is_actor_method_attribute(attr: &Attribute) -> bool {
    let path = attr.path();
    is_handler_attribute(attr)
        || path.is_ident("started")
        || path.is_ident("stopped")
        || path.is_ident("exit_received")
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
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
    // The inferred runtime also names the traits the expansion implements,
    // so the caller needs no `Actor`/`Handler`/`Context` import. With no
    // method to infer from (an empty block, or only `impl Future` and
    // cfg-gated methods), the bare names resolve through the caller's
    // imports, as before.
    let mut runtime = None;
    if let Some(first) = runtime_methods.next() {
        let asynchronous = first.asyncness.is_some();
        runtime = Some(if asynchronous {
            quote! { spawned_concurrency::tasks }
        } else {
            quote! { spawned_concurrency::threads }
        });
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

    if runtime.is_none()
        && impl_block.items.iter().any(|item| matches!(item,
            ImplItem::Fn(method) if method.attrs.iter().any(is_actor_method_attribute)
                && matches!(&method.sig.output, ReturnType::Type(_, ty) if matches!(**ty, Type::ImplTrait(_)))))
    {
        // Only tasks handlers return `impl Future` from a plain `fn`.
        runtime = Some(quote! { spawned_concurrency::tasks });
    }
    let (actor_trait, handler_trait, context_ty) = match &runtime {
        Some(path) => (
            quote! { #path::Actor },
            quote! { #path::Handler },
            quote! { #path::Context },
        ),
        None => (quote! { Actor }, quote! { Handler }, quote! { Context }),
    };

    let self_ty = &impl_block.self_ty;
    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

    // --- Parse named parameters: #[actor(protocol = X)], #[actor(protocol(X, Y))],
    // the deferred-reply bounds `max_outstanding = N`, `max_native_jobs = N`,
    // and the lint opt-outs `allow_shared_state` and `allow_panic_abort` ---
    let mut max_outstanding: Option<syn::Expr> = None;
    let mut max_native_jobs: Option<syn::Expr> = None;
    let mut allow_shared_state = false;
    let mut allow_panic_abort = false;
    // `inspect` (through `Inspect`) or `inspect = debug` (the `Debug` form):
    // `Actor::FORMAT_STATUS` for `get_status`.
    let mut inspect: Option<Option<Ident>> = None;
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
                if key == "max_outstanding" || key == "max_native_jobs" {
                    let _: syn::Token![=] = input.parse()?;
                    let value: syn::Expr = input.parse()?;
                    let slot = if key == "max_outstanding" {
                        &mut max_outstanding
                    } else {
                        &mut max_native_jobs
                    };
                    if slot.replace(value).is_some() {
                        return Err(syn::Error::new(key.span(), format!("`{key}` given twice")));
                    }
                } else if key == "inspect" {
                    let how = if input.peek(syn::Token![=]) {
                        let _: syn::Token![=] = input.parse()?;
                        let how: Ident = input.parse()?;
                        if how != "debug" {
                            return Err(syn::Error::new(
                                how.span(),
                                "expected `inspect` or `inspect = debug`",
                            ));
                        }
                        Some(how)
                    } else {
                        None
                    };
                    if inspect.replace(how).is_some() {
                        return Err(syn::Error::new(key.span(), "`inspect` given twice"));
                    }
                } else if key == "allow_shared_state" {
                    allow_shared_state = true;
                } else if key == "allow_panic_abort" {
                    allow_panic_abort = true;
                } else if key != "protocol" {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown parameter, expected `protocol`, `max_outstanding`, \
                         `max_native_jobs`, `inspect`, `allow_shared_state` or \
                         `allow_panic_abort`",
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
                if is_started || is_stopped || method.attrs.iter().any(is_handler_attribute) {
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
                if method.attrs.iter().any(is_handler_attribute) {
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
                // `started()` returns `()`, a `Result` or an `Init`: the impl's
                // return type refines the trait's `impl Into<Init<Self>>` on
                // purpose (unified start, 0.9).
                m.attrs
                    .push(syn::parse_quote!(#[allow(refining_impl_trait)]));
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
                if method.attrs.iter().any(is_handler_attribute) {
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
    let mut lifetime_name = "__spawned_reply_turn".to_owned();
    while impl_block
        .generics
        .lifetimes()
        .any(|p| p.lifetime.ident == lifetime_name)
    {
        lifetime_name.push('_');
    }
    let turn_lifetime = syn::Lifetime::new(
        &format!("'{lifetime_name}"),
        proc_macro2::Span::mixed_site(),
    );

    for item in &mut impl_block.items {
        if let ImplItem::Fn(method) = item {
            let handler_idx = method.attrs.iter().position(is_handler_attribute);

            if let Some(idx) = handler_idx {
                let deferred = method.attrs[idx].path().is_ident("deferred_handler");
                let send = method.attrs[idx].path().is_ident("send_handler");
                if method.attrs[idx].path().is_ident("handler") {
                    return syn::Error::new_spanned(
                        &method.attrs[idx],
                        "#[handler] was removed in 0.8: use #[send_handler] (returns `()`, no reply) or #[request_handler] (answers with the return value)",
                    )
                    .to_compile_error()
                    .into();
                }
                method.attrs.remove(idx);
                if let Some(extra) = method.attrs.iter().find(|a| is_handler_attribute(a)) {
                    return syn::Error::new_spanned(
                        extra,
                        "a method can carry only one of #[send_handler], #[request_handler] and #[deferred_handler]",
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
                        !is_handler_attribute(a)
                            && !a.path().is_ident("started")
                            && !a.path().is_ident("stopped")
                    })
                    .cloned()
                    .collect();

                let method_name = &method.sig.ident;
                if method.sig.asyncness.is_some() {
                    has_async = true;
                }

                // Validate parameters: (&mut self, msg, ctx), or for a deferred
                // handler (&mut self, msg, reply, ctx).
                let param_count = method.sig.inputs.len();
                let ctx_index = if deferred { 3 } else { 2 };
                if deferred && param_count != 4 {
                    return syn::Error::new_spanned(
                        &method.sig,
                        format!(
                            "#[deferred_handler] method must have 4 parameters (&mut self, msg: M, reply: PendingReply<'_, R>, ctx: &Context<Self>), found {param_count}"
                        ),
                    )
                    .to_compile_error()
                    .into();
                }
                if !deferred && param_count != 3 {
                    return syn::Error::new_spanned(
                        &method.sig,
                        format!(
                            "handler method must have 3 parameters (&mut self, msg: M, ctx: &Context<Self>), found {param_count}"
                        ),
                    )
                    .to_compile_error()
                    .into();
                }
                if deferred {
                    if let ReturnType::Type(_, ty) = &method.sig.output {
                        if !is_unit_type(ty) {
                            return syn::Error::new_spanned(
                                ty,
                                "#[deferred_handler] answers through its PendingReply; remove the return type",
                            )
                            .to_compile_error()
                            .into();
                        }
                    }
                }
                // A send handler that returned a value would silently answer a
                // `request` with it. (`impl Future` returns are checked by the
                // `Result = ()` bound of the message instead.)
                if send {
                    if let ReturnType::Type(_, ty) = &method.sig.output {
                        if !is_unit_type(ty) && !matches!(**ty, Type::ImplTrait(_)) {
                            return syn::Error::new_spanned(
                                ty,
                                "#[send_handler] methods return `()`; use #[request_handler] to answer a request with a value",
                            )
                            .to_compile_error()
                            .into();
                        }
                    }
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
                    None => unreachable!("the parameters were checked above"),
                }
                if let Some(FnArg::Typed(ctx)) = method.sig.inputs.iter().nth(ctx_index) {
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

                // The generated `handle` is the envelope dispatch: it takes the
                // request's reply obligation and answers it with the method's
                // result (a deferred handler receives the obligation itself).
                let reply_ty = quote! {
                    spawned_concurrency::PendingReply<'_, <#msg_ty as spawned_concurrency::message::Message>::Result>
                };
                // Mixed-site locals cannot shadow free names in a user body
                // that is specialized into the generated dispatch adapter.
                let msg_name = Ident::new("__spawned_msg", proc_macro2::Span::mixed_site());
                let reply_name = Ident::new("__spawned_reply", proc_macro2::Span::mixed_site());
                let ctx_name = Ident::new(
                    "__spawned_ctx",
                    proc_macro2::Span::mixed_site().located_at(
                        method
                            .sig
                            .inputs
                            .iter()
                            .nth(ctx_index)
                            .expect("validated context arity")
                            .span(),
                    ),
                );
                // The parameter list carries the user's parentheses span.
                let mut params = proc_macro2::Group::new(
                    proc_macro2::Delimiter::Parenthesis,
                    quote! { &mut self, #msg_name: #msg_ty, #reply_name: #reply_ty, #ctx_name: &#context_ty<Self> },
                );
                params.set_span(method.sig.paren_token.span.join());
                // Span the generated impl at the method so trait errors
                // (conflicting impls, signature mismatches) point at it.
                let span = method.sig.span();
                // The reply's type must equal the message's `Result`; a
                // mismatch is reported at the user's return type.
                let ret_span = match &method.sig.output {
                    ReturnType::Default => method.sig.paren_token.span.join(),
                    ReturnType::Type(_, ty) => ty.span(),
                };
                // A tasks handler may also be a plain `fn` returning
                // `impl Future`; its result is awaited like an `async fn`'s.
                let returns_future = matches!(
                    &method.sig.output,
                    ReturnType::Type(_, ty) if matches!(**ty, Type::ImplTrait(_))
                );
                let asynchronous = method.sig.asyncness.is_some() || returns_future;
                let immediate = asynchronous && !deferred && await_free_handler(method);
                let call = match (deferred, asynchronous) {
                    (true, true) => {
                        quote! { self.#method_name(#msg_name, #reply_name, #ctx_name).await }
                    }
                    (true, false) => {
                        quote! { self.#method_name(#msg_name, #reply_name, #ctx_name) }
                    }
                    (false, true) if immediate => {
                        let body = &method.block;
                        let msg_pat = match method
                            .sig
                            .inputs
                            .iter()
                            .nth(1)
                            .expect("validated message arity")
                        {
                            FnArg::Typed(arg) => &arg.pat,
                            _ => unreachable!(),
                        };
                        let (ctx_pat, ctx_ty) = match method
                            .sig
                            .inputs
                            .iter()
                            .nth(ctx_index)
                            .expect("validated context arity")
                        {
                            FnArg::Typed(arg) => (&arg.pat, &arg.ty),
                            _ => unreachable!(),
                        };
                        let output = match &method.sig.output {
                            ReturnType::Default => quote! { () },
                            ReturnType::Type(_, ty) => quote! { #ty },
                        };
                        // Dispatch has already begun when this adapter is
                        // constructed. Keep the inherent async method lazy,
                        // but evaluate this await-free dispatch without a
                        // heap-stored future. A closure preserves return/?.
                        let answer = quote::quote_spanned! { ret_span =>
                            let _ = #reply_name.reply(value);
                        };
                        quote! {
                            // Function/closure parameters retain ignored fields
                            // until the body returns. A destructuring let here
                            // would drop them before the handler body runs.
                            let value = (|#msg_pat: #msg_ty, #ctx_pat: #ctx_ty| -> #output #body)(
                                #msg_name, #ctx_name
                            );
                            #answer
                        }
                    }
                    (false, true) => {
                        let value = quote::quote_spanned! { ret_span => self.#method_name(#msg_name, #ctx_name).await };
                        quote! { let _ = #reply_name.reply(#value); }
                    }
                    (false, false) => {
                        let value = quote::quote_spanned! { ret_span => self.#method_name(#msg_name, #ctx_name) };
                        quote! { let _ = #reply_name.reply(#value); }
                    }
                };

                // The trait path takes the handler's span, so a bound error
                // (`M: Message`) points at the method, as a literal would.
                let handler_trait = respan(handler_trait.clone(), span);
                let handler_impl = if immediate {
                    quote::quote_spanned! { span =>
                        #(#extra_attrs)*
                        impl #impl_generics #handler_trait<#msg_ty> for #self_ty #where_clause {
                            async fn handle #params {
                                let _ = #reply_name.reply(self.#method_name(#msg_name, #ctx_name).await);
                            }

                            // Inlined into the envelope's dispatch with the
                            // handler body and reply(); left to the heuristics,
                            // whether that happens depends on how the crate is
                            // split into modules (+3.6% instructions per send
                            // after #278).
                            #[inline]
                            fn __spawned_handle_immediate<#turn_lifetime>(
                                &mut self,
                                #msg_name: #msg_ty,
                                #reply_name: spawned_concurrency::PendingReply<#turn_lifetime, <#msg_ty as spawned_concurrency::message::Message>::Result>,
                                #ctx_name: &#context_ty<Self>,
                            ) -> ::core::option::Option<(#msg_ty, spawned_concurrency::PendingReply<#turn_lifetime, <#msg_ty as spawned_concurrency::message::Message>::Result>)> {
                                // Keep payload destructors inside the original
                                // future, whose drop is fenced by DeathFirst.
                                // A refused result is dropped by reply() in
                                // either adapter, after the method returned;
                                // unlike a parameter, it adds no unwind capture.
                                // This constant branch disappears for messages
                                // without drop glue.
                                if ::core::mem::needs_drop::<#msg_ty>() {
                                    return ::core::option::Option::Some((#msg_name, #reply_name));
                                }
                                #call
                                ::core::option::Option::None
                            }
                        }
                    }
                } else if asynchronous {
                    quote::quote_spanned! { span =>
                        #(#extra_attrs)*
                        impl #impl_generics #handler_trait<#msg_ty> for #self_ty #where_clause {
                            async fn handle #params {
                                #call
                            }
                        }
                    }
                } else {
                    quote::quote_spanned! { span =>
                        #(#extra_attrs)*
                        impl #impl_generics #handler_trait<#msg_ty> for #self_ty #where_clause {
                            fn handle #params {
                                #call
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

    let bound_consts = {
        let outstanding = max_outstanding.map(|n| quote! { const MAX_OUTSTANDING: usize = #n; });
        let native = max_native_jobs
            .map(|n| quote! { const MAX_NATIVE_JOBS: ::core::option::Option<usize> = ::core::option::Option::Some(#n); });
        let format_status = inspect.map(|how| match how {
            None => quote! {
                const FORMAT_STATUS: ::core::option::Option<fn(&Self) -> ::std::string::String> =
                    ::core::option::Option::Some(
                        <Self as spawned_concurrency::introspection::Inspect>::format_status,
                    );
            },
            Some(debug) => quote::quote_spanned! { debug.span() =>
                const FORMAT_STATUS: ::core::option::Option<fn(&Self) -> ::std::string::String> =
                    ::core::option::Option::Some(|state: &Self| ::std::format!("{state:#?}"));
            },
        });
        quote! { #outstanding #native #format_status }
    };

    let actor_impl = quote! {
        #protocol_doc
        impl #impl_generics #actor_trait for #self_ty #where_clause {
            #bound_consts
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

/// `tokens` with every span set to `span` (an interpolated path keeps its
/// own spans inside `quote_spanned!`).
fn respan(tokens: proc_macro2::TokenStream, span: proc_macro2::Span) -> proc_macro2::TokenStream {
    tokens
        .into_iter()
        .map(|mut tree| {
            if let proc_macro2::TokenTree::Group(group) = &tree {
                let mut new =
                    proc_macro2::Group::new(group.delimiter(), respan(group.stream(), span));
                new.set_span(span);
                tree = proc_macro2::TokenTree::Group(new);
            } else {
                tree.set_span(span);
            }
            tree
        })
        .collect()
}
