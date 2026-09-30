use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, LitStr, Path, parse_macro_input, spanned::Spanned};

#[proc_macro_derive(StateTreeEvent, attributes(state_tree))]
/// Declares one external event payload for a state-tree domain.
pub fn derive_state_tree_event(input: TokenStream) -> TokenStream {
    expand_state_tree_event(parse_macro_input!(input as DeriveInput))
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn expand_state_tree_event(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    expand_state_tree_event_with_runtime(input, runtime_crate()?)
}

fn expand_state_tree_event_with_runtime(
    input: DeriveInput,
    runtime: proc_macro2::TokenStream,
) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "state-tree event payloads cannot be generic",
        ));
    }
    if !matches!(input.data, Data::Struct(_) | Data::Enum(_)) {
        return Err(Error::new(
            input.span(),
            "StateTreeEvent can only be derived for a struct or enum",
        ));
    }

    let options = parse_options(&input)?;
    let payload = &input.ident;
    let domain = options.domain;
    let authored_name = options.name;
    let event_data = if let Some(projector) = options.data {
        quote! {
            fn event_data(
                &self,
            ) -> ::core::result::Result<
                ::core::option::Option<#runtime::LuauValue>,
                #runtime::StateTreeEventDataError,
            > {
                #projector(self).map(::core::option::Option::Some)
            }
        }
    } else {
        quote! {}
    };
    let routing_impl = if options.broadcast {
        quote! {
            impl #runtime::BroadcastStateTreeEvent<#domain> for #payload {}
        }
    } else {
        quote! {
            impl #runtime::EntityStateTreeEvent<#domain> for #payload {}
        }
    };

    Ok(quote! {
        impl #runtime::StateTreeEvent<#domain> for #payload {
            const AUTHORED_NAME: &'static str = #authored_name;

            #event_data
        }

        #routing_impl
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

struct Options {
    domain: Path,
    name: LitStr,
    broadcast: bool,
    data: Option<Path>,
}

fn parse_options(input: &DeriveInput) -> syn::Result<Options> {
    let mut domain = None;
    let mut name = None;
    let mut broadcast = false;
    let mut data = None;
    let mut found = false;

    for attribute in input
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("state_tree"))
    {
        if found {
            return Err(Error::new(
                attribute.span(),
                "use one `state_tree` attribute per event payload",
            ));
        }
        found = true;
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("domain") {
                if domain.is_some() {
                    return Err(meta.error("`domain` was specified more than once"));
                }
                domain = Some(meta.value()?.parse()?);
                Ok(())
            } else if meta.path.is_ident("name") {
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
            } else if meta.path.is_ident("data") {
                if data.is_some() {
                    return Err(meta.error("`data` was specified more than once"));
                }
                data = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error(
                    "expected `domain = DomainType`, `name = \"...\"`, `broadcast`, or `data = path`",
                ))
            }
        })?;
    }

    let domain = domain.ok_or_else(|| {
        Error::new(
            input.ident.span(),
            "missing `domain` in `#[state_tree(...)]`",
        )
    })?;
    let name = name
        .ok_or_else(|| Error::new(input.ident.span(), "missing `name` in `#[state_tree(...)]`"))?;
    if name.value().is_empty() {
        return Err(Error::new(
            name.span(),
            "state-tree event name cannot be empty",
        ));
    }

    Ok(Options {
        domain,
        name,
        broadcast,
        data,
    })
}

#[cfg(test)]
mod tests {
    use quote::quote;
    use syn::{DeriveInput, parse_quote};

    use super::expand_state_tree_event_with_runtime;

    fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
        expand_state_tree_event_with_runtime(input, quote!(::bevy_state_tree))
    }

    #[test]
    fn expands_entity_payload() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation, name = "damage")]
            struct Damage {
                amount: u32,
            }
        };

        let output = expand(input).unwrap().to_string();

        assert!(output.contains("StateTreeEvent < Simulation > for Damage"));
        assert!(output.contains("EntityStateTreeEvent < Simulation > for Damage"));
        assert!(output.contains("AUTHORED_NAME"));
        assert!(output.contains("damage"));
        assert!(!output.contains("BroadcastStateTreeEvent"));
    }

    #[test]
    fn expands_broadcast_payload_with_data_projector() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(
                domain = Simulation,
                name = "reset",
                broadcast,
                data = project_reset
            )]
            enum Reset {
                Soft,
                Hard,
            }
        };

        let output = expand(input).unwrap().to_string();

        assert!(output.contains("BroadcastStateTreeEvent < Simulation > for Reset"));
        assert!(output.contains("project_reset (self)"));
        assert!(output.contains("Option :: Some"));
        assert!(!output.contains("EntityStateTreeEvent"));
    }

    #[test]
    fn rejects_generic_payloads() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation, name = "payload")]
            struct Payload<T>(T);
        };

        let error = expand(input).unwrap_err();

        assert!(error.to_string().contains("cannot be generic"));
    }

    #[test]
    fn rejects_unions() {
        let input: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation, name = "payload")]
            union Payload {
                integer: u32,
                float: f32,
            }
        };

        let error = expand(input).unwrap_err();

        assert!(error.to_string().contains("struct or enum"));
    }

    #[test]
    fn requires_domain_and_name() {
        let missing_domain: DeriveInput = parse_quote! {
            #[state_tree(name = "payload")]
            struct Payload;
        };
        let missing_name: DeriveInput = parse_quote! {
            #[state_tree(domain = Simulation)]
            struct Payload;
        };

        assert!(
            expand(missing_domain)
                .unwrap_err()
                .to_string()
                .contains("missing `domain`")
        );
        assert!(
            expand(missing_name)
                .unwrap_err()
                .to_string()
                .contains("missing `name`")
        );
    }
}
