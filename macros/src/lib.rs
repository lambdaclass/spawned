use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, FnArg, ImplItem, ItemImpl, Pat, ReturnType, Type};

/// Attribute macro for actor impl blocks.
///
/// Place `#[actor]` on an `impl MyActor` block containing methods annotated
/// with `#[send_handler]` or `#[request_handler]`. For each annotated method,
/// the macro generates a corresponding `impl Handler<M> for MyActor` block.
///
/// Use `#[send_handler]` for fire-and-forget messages (no return value):
///
/// ```ignore
/// #[send_handler]
/// async fn on_deposit(&mut self, msg: Deposit, ctx: &Context<Self>) { ... }
/// ```
///
/// Use `#[request_handler]` for request-response messages (returns a value):
///
/// ```ignore
/// #[request_handler]
/// async fn on_balance(&mut self, msg: GetBalance, ctx: &Context<Self>) -> u64 { ... }
/// ```
///
/// Sync handlers (for the `threads` module) omit `async`:
///
/// ```ignore
/// #[send_handler]
/// fn on_deposit(&mut self, msg: Deposit, ctx: &Context<Self>) { ... }
/// ```
///
/// The generic `#[handler]` attribute is also supported for backwards
/// compatibility and works for both send and request handlers.
#[proc_macro_attribute]
pub fn actor(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut impl_block = parse_macro_input!(item as ItemImpl);

    let self_ty = &impl_block.self_ty;
    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

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

                let method_name = &method.sig.ident;
                let is_async = method.sig.asyncness.is_some();

                // Extract message type from 2nd parameter (index 1, after &mut self)
                let msg_ty = match method.sig.inputs.iter().nth(1) {
                    Some(FnArg::Typed(pat_type)) => {
                        if let Pat::Ident(pat_ident) = &*pat_type.pat {
                            if pat_ident.ident == "_" || pat_ident.ident.to_string().starts_with('_') {
                                // Still use the type
                            }
                        }
                        &*pat_type.ty
                    }
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

                let handler_impl = if is_async {
                    quote! {
                        impl #impl_generics Handler<#msg_ty> for #self_ty #where_clause {
                            async fn handle(&mut self, msg: #msg_ty, ctx: &Context<Self>) -> #ret_ty {
                                self.#method_name(msg, ctx).await
                            }
                        }
                    }
                } else {
                    quote! {
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

    let output = quote! {
        #impl_block
        #(#handler_impls)*
    };

    output.into()
}
