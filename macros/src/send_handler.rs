//! `#[send_handler]`: the send-only form of a manual `Handler` impl.
use super::*;

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let krate: Option<syn::Path> = if attr.is_empty() {
        None
    } else {
        let parser = |input: syn::parse::ParseStream| {
            input.parse::<syn::Token![crate]>()?;
            input.parse::<syn::Token![=]>()?;
            let path: syn::Path = input.parse()?;
            if !input.is_empty() {
                return Err(input.error("expected only `crate = path`"));
            }
            Ok(path)
        };
        match syn::parse::Parser::parse(parser, attr) {
            Ok(path) => Some(path),
            Err(error) => {
                return syn::Error::new(
                    error.span(),
                    format!("#[send_handler] takes no arguments or `crate = path`: {error}"),
                )
                .to_compile_error()
                .into()
            }
        }
    };
    let mut impl_block = parse_macro_input!(item as ItemImpl);
    let is_handler = impl_block
        .trait_
        .as_ref()
        .and_then(|(_, path, _)| path.segments.last())
        .is_some_and(|segment| segment.ident == "Handler");
    if !is_handler {
        return syn::Error::new_spanned(
            &impl_block.self_ty,
            "#[send_handler] on an item applies to an `impl Handler<M> for Actor` block; \
             inside an #[actor] block, put it on the handler method instead",
        )
        .to_compile_error()
        .into();
    }
    let Some(method) = impl_block.items.iter_mut().find_map(|item| match item {
        ImplItem::Fn(method) if method.sig.ident == "handle" => Some(method),
        _ => None,
    }) else {
        return syn::Error::new_spanned(
            &impl_block.self_ty,
            "#[send_handler] impl must define `handle`",
        )
        .to_compile_error()
        .into();
    };
    if method.sig.inputs.len() != 3 {
        return syn::Error::new_spanned(
            &method.sig,
            "#[send_handler] `handle` takes (&mut self, msg: M, ctx: &Context<Self>), without the reply parameter",
        )
        .to_compile_error()
        .into();
    }
    if let ReturnType::Type(_, ty) = &method.sig.output {
        if !is_unit_type(ty) {
            return syn::Error::new_spanned(ty, "#[send_handler] `handle` returns nothing")
                .to_compile_error()
                .into();
        }
    }
    let reply = format_ident!("__spawned_reply");
    // The `Handler` path's module (`tasks`/`threads`) re-exports
    // `PendingReply`; a bare `Handler` falls back to the crate path.
    let handler_module = impl_block.trait_.as_ref().and_then(|(_, path, _)| {
        (path.segments.len() > 1).then(|| {
            let mut module = path.clone();
            module.segments.pop();
            module.segments.pop_punct();
            module
        })
    });
    let reply_ty = match (krate, handler_module) {
        (Some(krate), _) => quote! { #krate::PendingReply<'_, ()> },
        (None, Some(module)) => quote! { #module::PendingReply<'_, ()> },
        (None, None) => quote! { spawned_concurrency::PendingReply<'_, ()> },
    };
    let reply_param: FnArg = syn::parse_quote! {
        #reply: #reply_ty
    };
    method.sig.inputs.insert(2, reply_param);
    method.sig.output = ReturnType::Default;
    let body = &method.block;
    let run = if method.sig.asyncness.is_some() {
        quote! { async move #body.await }
    } else {
        quote! { (move || #body)() }
    };
    method.block = syn::parse_quote! {{
        let () = #run;
        let _ = #reply.reply(());
    }};
    quote! { #impl_block }.into()
}
