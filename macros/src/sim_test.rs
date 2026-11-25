//! `#[sim_test]`: a test run under the deterministic simulator.
use super::*;

const OPTIONS: &str =
    "sim_test accepts flavor, worker_threads, seeds, explore, iterations, sim_only and crate";

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let options = parse_macro_input!(attr with syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated);
    let function = parse_macro_input!(item as syn::ItemFn);
    let mut multi_thread = false;
    let mut workers: Option<syn::Expr> = None;
    let mut seeds: Option<syn::Expr> = None;
    let mut explore: Option<syn::Expr> = None;
    let mut iterations: Option<syn::Expr> = None;
    let mut sim_only = false;
    let mut krate: syn::Path = syn::parse_quote!(::spawned_concurrency);
    for option in options {
        let option = match option {
            syn::Meta::Path(path) if path.is_ident("sim_only") => {
                sim_only = true;
                continue;
            }
            syn::Meta::NameValue(option) => option,
            other => {
                return syn::Error::new_spanned(other, OPTIONS)
                    .to_compile_error()
                    .into()
            }
        };
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
            "crate" => match option.value {
                syn::Expr::Path(path) => krate = path.path,
                other => {
                    return syn::Error::new_spanned(other, "expected `crate = path`")
                        .to_compile_error()
                        .into()
                }
            },
            _ => {
                return syn::Error::new_spanned(&option.path, OPTIONS)
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
    let test = quote! {
        #(#attrs)*
        #[test]
        #vis fn #name() {
            #krate::__private::SimTest::new()
                .named(env!("CARGO_PKG_NAME"), module_path!(), stringify!(#name))
                #flavor
                #seeds
                #explore
                #iterations
                .run(|#seed: u64| async move #body)
        }
    };
    if sim_only {
        // The cfg is evaluated by spawned-concurrency, which declares it, so
        // the caller needs neither `#[cfg(spawned_sim)]` nor the
        // `unexpected_cfgs` lint configuration.
        quote! { #krate::__private::sim_only! { #test } }.into()
    } else {
        test.into()
    }
}
