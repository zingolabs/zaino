//! Proc-macros for `zaino-source`.
//!
//! [`macro@resilient_port`] derives a *resilient* port from a single-attempt
//! `OneShot*` port: it reads the annotated trait's method signatures and emits
//! the canonical (unqualified) twin trait plus a blanket impl of it for
//! `Resilient<V>`, so the retry ladder and the `QueryError -> SourceError`
//! translation are written once (in `Resilient::with_retry`) and the twin's
//! signature is never restated.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    spanned::Spanned, FnArg, GenericArgument, ItemTrait, Pat, PathArguments, ReturnType, TraitItem,
    Type,
};

/// Derive the resilient twin of a single-attempt `OneShot*` source port.
///
/// Placed on a `OneShotX` trait, it emits the original trait unchanged, plus a
/// canonical `X` trait whose methods return
/// [`SourceError`](zaino_source::SourceError) instead of
/// [`QueryError`](zaino_source::QueryError), plus
/// `impl X for Resilient<V> where V: OneShotX`. The twin name is the annotated
/// name with the `OneShot` prefix stripped.
///
/// Every method's return type must be
/// `impl Future<Output = Result<T, QueryError<E>>> + Send`; the twin returns
/// `impl Future<Output = Result<T, SourceError<E>>> + Send`. Arguments must be
/// `Clone` — the retry loop may call the delegate more than once.
///
/// Non-idempotent ports (a raw-transaction send) are simply left un-annotated:
/// exclusion is the absence of the attribute, not a special case inside it.
#[proc_macro_attribute]
pub fn resilient_port(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let one_shot = syn::parse_macro_input!(item as ItemTrait);
    match expand(&one_shot) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand(one_shot: &ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let one_shot_ident = &one_shot.ident;
    let twin_ident = {
        let name = one_shot_ident.to_string();
        let stripped = name.strip_prefix("OneShot").ok_or_else(|| {
            syn::Error::new(
                one_shot_ident.span(),
                "#[resilient_port] expects a trait named `OneShot*`",
            )
        })?;
        format_ident!("{}", stripped)
    };

    let mut twin_methods = Vec::new();
    let mut impl_methods = Vec::new();

    for item in &one_shot.items {
        let TraitItem::Fn(method) = item else {
            return Err(syn::Error::new(
                item.span(),
                "#[resilient_port] supports only method items",
            ));
        };
        let sig = &method.sig;
        let name = &sig.ident;

        // Success/error payloads out of `impl Future<Output = Result<T, QueryError<E>>> + Send`.
        let (ok_ty, err_ty) = extract_payloads(&sig.output)?;

        // The typed args (everything after `&self`), reused for the signature
        // and cloned for the delegating call.
        let mut arg_decls = Vec::new();
        let mut arg_names = Vec::new();
        for input in sig.inputs.iter().skip(1) {
            let FnArg::Typed(pat_ty) = input else {
                return Err(syn::Error::new(
                    input.span(),
                    "unexpected receiver argument",
                ));
            };
            let Pat::Ident(pat_ident) = pat_ty.pat.as_ref() else {
                return Err(syn::Error::new(
                    pat_ty.pat.span(),
                    "#[resilient_port] needs simple `name: Type` arguments",
                ));
            };
            let ident = &pat_ident.ident;
            let ty = &pat_ty.ty;
            arg_decls.push(quote! { #ident: #ty });
            arg_names.push(ident.clone());
        }

        let doc = format!(
            "Resilient [`{one_shot}::{name}`](crate::{one_shot}::{name}): retries transient \
             transport failures, surfacing [`SourceError::Unavailable`](crate::SourceError::Unavailable) \
             once the retry ladder is spent.",
            one_shot = one_shot_ident,
        );

        twin_methods.push(quote! {
            #[doc = #doc]
            fn #name(&self #(, #arg_decls)*)
                -> impl ::core::future::Future<
                    Output = ::core::result::Result<#ok_ty, crate::SourceError<#err_ty>>,
                > + Send;
        });

        impl_methods.push(quote! {
            async fn #name(&self #(, #arg_decls)*)
                -> ::core::result::Result<#ok_ty, crate::SourceError<#err_ty>> {
                self.with_retry(|| self.inner().#name(#(#arg_names.clone()),*)).await
            }
        });
    }

    let twin_doc = format!(
        "Resilient counterpart of [`{0}`](crate::{0}). Implemented only by \
         [`Resilient`](crate::Resilient); binding it means retries are already handled below.",
        one_shot_ident,
    );

    Ok(quote! {
        // The single-attempt port, unchanged.
        #one_shot

        #[doc = #twin_doc]
        pub trait #twin_ident: crate::resilient::sealed::Sealed + Send + Sync {
            #(#twin_methods)*
        }

        #[allow(clippy::clone_on_copy)]
        impl<V> #twin_ident for crate::Resilient<V>
        where
            V: #one_shot_ident + Send + Sync,
        {
            #(#impl_methods)*
        }
    })
}

/// Pull `T` and `E` out of `impl Future<Output = Result<T, QueryError<E>>> + Send`.
fn extract_payloads(output: &ReturnType) -> syn::Result<(Type, Type)> {
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new(
            output.span(),
            "#[resilient_port] method must return a future",
        ));
    };
    let span = ty.span();
    let err = |msg: &str| syn::Error::new(span, msg);

    // impl Future<Output = ...> + Send
    let Type::ImplTrait(imp) = ty.as_ref() else {
        return Err(err("expected `impl Future<Output = ...> + Send`"));
    };
    let future_args = imp
        .bounds
        .iter()
        .find_map(|bound| match bound {
            syn::TypeParamBound::Trait(t) => t
                .path
                .segments
                .last()
                .and_then(|seg| (seg.ident == "Future").then_some(&seg.arguments)),
            _ => None,
        })
        .ok_or_else(|| err("expected a `Future` bound"))?;

    // <Output = Result<T, QueryError<E>>>
    let PathArguments::AngleBracketed(future_args) = future_args else {
        return Err(err("`Future` bound needs an `Output = ...` binding"));
    };
    let output_ty = future_args
        .args
        .iter()
        .find_map(|arg| match arg {
            GenericArgument::AssocType(assoc) if assoc.ident == "Output" => Some(&assoc.ty),
            _ => None,
        })
        .ok_or_else(|| err("`Future` bound needs an `Output = ...` binding"))?;

    // Result<T, QueryError<E>>
    let (ok_ty, query_err_ty) = result_args(output_ty)
        .ok_or_else(|| err("`Future`'s Output must be `Result<T, QueryError<E>>`"))?;

    // QueryError<E>  ->  E
    let err_ty = sole_generic(query_err_ty, "QueryError")
        .ok_or_else(|| err("expected the error to be `QueryError<E>`"))?;

    Ok((ok_ty.clone(), err_ty.clone()))
}

/// The two type arguments of a `Result<_, _>`.
fn result_args(ty: &Type) -> Option<(&Type, &Type)> {
    let Type::Path(path) = ty else { return None };
    let seg = path.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    let mut types = args.args.iter().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    Some((types.next()?, types.next()?))
}

/// The sole type argument of `Wrapper<E>`, checking the wrapper's name.
fn sole_generic<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    let Type::Path(path) = ty else { return None };
    let seg = path.path.segments.last()?;
    if seg.ident != wrapper {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}
