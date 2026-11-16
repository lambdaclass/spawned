//! `#[actor]` lints: shared-state and panic-abort warnings as `#[deprecated]` uses.
use super::*;

// --- #[actor] lints ---
//
// `#[actor]` sees only the impl block it annotates, not the struct, so the
// shared-state lint is syntactic over what that block contains: constructor
// and handler signatures, `Self { .. }` literals, `let` types and
// `Arc::new(Mutex::new(..))`-style expressions. Proc-macro diagnostics are
// unstable, so each warning is a use of a `#[deprecated]` item spanned at the
// offending tokens: rustc prints the note there, `-D warnings` makes it an
// error, and nothing reaches the compiled program.

pub(super) const SHARED_STATE_NOTE: &str = "actors should own their state: shared mutable state \
(`Arc<Mutex<_>>`, `Arc<RwLock<_>>`, `Arc<Atomic*>`, `Rc<RefCell<_>>`) breaks the isolation \
supervision relies on (a restarted actor inherits whatever the old one left behind the lock), \
and a panic while holding a std lock poisons it for every holder; send messages to the owning \
actor instead (or, once available, read a published snapshot, ROADMAP rank 10); if the \
sharing is deliberate, use `#[actor(allow_shared_state)]`";

pub(super) const PANIC_ABORT_NOTE: &str =
    "built with `panic = \"abort\"`: a panicking handler aborts \
the whole process instead of stopping one actor, so supervisors, links, monitors and \
`stopped()` never see the failure; build actor binaries with `panic = \"unwind\"` (the \
default), or use `#[actor(allow_panic_abort)]` if aborting is deliberate";

/// Smart pointers that share their contents between owners.
pub(super) fn is_shared_pointer(ident: &Ident) -> bool {
    ident == "Arc" || ident == "Rc"
}

/// Interior-mutable types from std, tokio, parking_lot and similar crates,
/// matched by their last path segment.
pub(super) fn is_interior_mutable(ident: &Ident) -> bool {
    ident == "Mutex"
        || ident == "RwLock"
        || ident == "RefCell"
        || ident.to_string().starts_with("Atomic")
}

/// `Arc<Mutex<_>>`, `std::sync::Arc<tokio::sync::RwLock<_>>`, `Rc<RefCell<_>>`, ...
pub(super) fn is_shared_mutable_type(path: &syn::Path) -> bool {
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
pub(super) fn call_type_segment(func: &syn::Expr) -> Option<&Ident> {
    let syn::Expr::Path(path) = func else {
        return None;
    };
    let segments = &path.path.segments;
    (segments.len() >= 2).then(|| &segments[segments.len() - 2].ident)
}

/// `Arc::new(Mutex::new(..))`, `Rc::new(RefCell::new(..))`,
/// `Arc::new(AtomicU64::default())`, ...
pub(super) fn is_shared_mutable_constructor(call: &syn::ExprCall) -> bool {
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
pub(super) struct SharedStateFinder {
    pub(super) spans: Vec<proc_macro2::Span>,
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
pub(super) fn deprecation_warning(
    name: &str,
    note: &str,
    span: proc_macro2::Span,
) -> proc_macro2::TokenStream {
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
pub(super) static PANIC_ABORT_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn actor_lints(
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
    if !allow_panic_abort && !PANIC_ABORT_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
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
