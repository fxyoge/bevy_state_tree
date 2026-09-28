use std::collections::HashSet;

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use quote::{ToTokens, format_ident, quote};
use syn::{
    Attribute, Data, DeriveInput, Error, Fields, LitStr, Path, parse_macro_input, spanned::Spanned,
};

#[proc_macro_derive(StateTreeEvents, attributes(state_tree))]
/// Declares a domain's closed set of external state-tree events.
pub fn derive_state_tree_events(input: TokenStream) -> TokenStream {
    expand_state_tree_events(parse_macro_input!(input as DeriveInput))
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn expand_state_tree_events(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    expand_state_tree_events_with_runtime(input, runtime_crate()?)
}

fn expand_state_tree_events_with_runtime(
    input: DeriveInput,
    runtime: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "state-tree event enums cannot be generic",
        ));
    }

    let domain = parse_domain(&input.attrs)?;
    let Data::Enum(data) = &input.data else {
        return Err(Error::new(
            input.span(),
            "StateTreeEvents can only be derived for an enum",
        ));
    };
    if data.variants.is_empty() {
        return Err(Error::new(
            input.span(),
            "a state-tree event enum must contain at least one event",
        ));
    }

    let enum_name = &input.ident;
    let mut payloads = HashSet::new();
    let mut name_arms = Vec::new();
    let mut payload_impls = Vec::new();
    let mut conversion_impls = Vec::new();
    let mut route_impls = Vec::new();

    for variant in &data.variants {
        let (authored_name, broadcast) = parse_variant(&variant.attrs, variant.span())?;
        let Fields::Unnamed(fields) = &variant.fields else {
            return Err(Error::new(
                variant.fields.span(),
                "state-tree event variants must contain one unnamed payload",
            ));
        };
        if fields.unnamed.len() != 1 {
            return Err(Error::new(
                fields.span(),
                "state-tree event variants must contain exactly one payload",
            ));
        }
        let payload = &fields.unnamed[0].ty;
        let payload_key = payload.to_token_stream().to_string();
        if !payloads.insert(payload_key) {
            return Err(Error::new(
                payload.span(),
                "a payload type can appear only once in a state-tree event enum",
            ));
        }

        let variant_name = &variant.ident;
        name_arms.push(quote! {
            Self::#variant_name(_) => #authored_name
        });
        payload_impls.push(quote! {
            impl #runtime::StateTreeEventPayload<#enum_name> for #payload {
                const AUTHORED_NAME: &'static str = #authored_name;

                fn into_state_tree_event(self) -> #enum_name {
                    #enum_name::#variant_name(self)
                }

                fn payload(event: &#enum_name) -> ::core::option::Option<&Self> {
                    match event {
                        #enum_name::#variant_name(payload) => ::core::option::Option::Some(payload),
                        _ => ::core::option::Option::None,
                    }
                }
            }
        });
        conversion_impls.push(quote! {
            impl ::core::convert::From<#payload> for #enum_name {
                fn from(payload: #payload) -> Self {
                    Self::#variant_name(payload)
                }
            }
        });
        if broadcast {
            route_impls.push(quote! {
                impl #runtime::BroadcastStateTreeEvent<#enum_name> for #payload {}
            });
        } else {
            route_impls.push(quote! {
                impl #runtime::EntityStateTreeEvent<#enum_name> for #payload {}
            });
        }
    }

    Ok(quote! {
        impl #runtime::StateTreeEvents for #enum_name {
            fn authored_name(&self) -> &'static str {
                match self {
                    #(#name_arms),*
                }
            }
        }

        impl #runtime::StateTreeDomain for #domain {
            type Event = #enum_name;
        }

        #(#payload_impls)*
        #(#conversion_impls)*
        #(#route_impls)*
    })
}

fn runtime_crate() -> syn::Result<proc_macro2::TokenStream> {
    match crate_name("bevy_state_tree") {
        Ok(FoundCrate::Itself) => Ok(quote!(::bevy_state_tree)),
        Ok(FoundCrate::Name(name)) => {
            let name = format_ident!("{}", name.replace('-', "_"));
            Ok(quote!(::#name))
        }
        Err(error) => Err(Error::new(proc_macro2::Span::call_site(), error)),
    }
}

fn parse_domain(attributes: &[Attribute]) -> syn::Result<Path> {
    let mut domain = None;
    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("state_tree"))
    {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("domain") {
                if domain.is_some() {
                    return Err(meta.error("`domain` was specified more than once"));
                }
                domain = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("expected `domain = DomainType`"))
            }
        })?;
    }
    domain.ok_or_else(|| {
        Error::new(
            proc_macro2::Span::call_site(),
            "missing `#[state_tree(domain = DomainType)]`",
        )
    })
}

fn parse_variant(
    attributes: &[Attribute],
    variant_span: proc_macro2::Span,
) -> syn::Result<(LitStr, bool)> {
    let mut name = None;
    let mut broadcast = false;
    let mut found = false;

    for attribute in attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("state_tree"))
    {
        if found {
            return Err(Error::new(
                attribute.span(),
                "use one `state_tree` attribute per event variant",
            ));
        }
        found = true;
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                if name.is_some() {
                    return Err(meta.error("`name` was specified more than once"));
                }
                name = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            } else if meta.path.is_ident("broadcast") {
                if broadcast {
                    return Err(meta.error("`broadcast` was specified more than once"));
                }
                broadcast = true;
                Ok(())
            } else {
                Err(meta.error("expected `name = \"...\"` or `broadcast`"))
            }
        })?;
    }

    let name = name.ok_or_else(|| {
        Error::new(
            variant_span,
            "event variant requires `#[state_tree(name = \"...\")]`",
        )
    })?;
    if name.value().is_empty() {
        return Err(Error::new(
            name.span(),
            "state-tree event name cannot be empty",
        ));
    }
    Ok((name, broadcast))
}

#[cfg(test)]
mod tests {
    use quote::quote;
    use syn::{DeriveInput, parse_quote};

    use super::expand_state_tree_events_with_runtime;

    fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
        expand_state_tree_events_with_runtime(input, quote!(::bevy_state_tree))
    }

    #[test]
    fn expands_entity_and_broadcast_payloads() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation)]
            enum Events {
                #[state_tree(name = "damage")]
                Damage(Damage),
                #[state_tree(name = "reset", broadcast)]
                Reset(Reset),
            }
        };

        let output = expand(input).unwrap().to_string();

        assert!(output.contains("EntityStateTreeEvent"));
        assert!(output.contains("BroadcastStateTreeEvent"));
        assert!(output.contains("damage"));
        assert!(output.contains("reset"));
    }

    #[test]
    fn rejects_duplicate_payload_types() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation)]
            enum Events {
                #[state_tree(name = "first")]
                First(Payload),
                #[state_tree(name = "second")]
                Second(Payload),
            }
        };

        let error = expand(input).unwrap_err();

        assert!(error.to_string().contains("can appear only once"));
    }

    #[test]
    fn rejects_variants_without_one_tuple_payload() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation)]
            enum Events {
                #[state_tree(name = "damage")]
                Damage { amount: u32 },
            }
        };

        let error = expand(input).unwrap_err();

        assert!(error.to_string().contains("one unnamed payload"));
    }
}
