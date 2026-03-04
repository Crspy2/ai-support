use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, DeriveInput, FnArg, ImplItem, ItemImpl, Meta, Type,
    parse_macro_input,
};

/// All recognised hook events in `{category}::{event}` form.
/// Add new entries here when introducing a new hookable category.
const VALID_HOOK_EVENTS: &[&str] = &[
    "issue::proposed",
    "issue::accepted",
    "issue::rejected",
    "issue::ended",
];

/// Derive macro that generates `impl ExtensionSchema` from a struct's fields.
///
/// Every field **must** carry a `#[description("...")]` helper attribute;
/// omitting one is a compile error.
///
/// ```ignore
/// use extensions_macros::ExtensionSchema;
///
/// #[derive(serde::Deserialize, ExtensionSchema)]
/// struct MyArgs {
///     #[description("The user's Discord snowflake ID")]
///     discord_id: String,
/// }
/// ```
#[proc_macro_derive(ExtensionSchema, attributes(description))]
pub fn derive_args_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        syn::Data::Struct(s) => &s.fields,
        _ => {
            return syn::Error::new_spanned(name, "ExtensionSchema can only be derived on structs")
                .to_compile_error()
                .into();
        }
    };

    let mut props_entries: Vec<TokenStream2> = Vec::new();
    let mut required_keys: Vec<String> = Vec::new();
    let mut errors: Vec<TokenStream2> = Vec::new();

    for field in fields {
        let field_name = match &field.ident {
            Some(i) => i.to_string(),
            None => {
                errors.push(
                    syn::Error::new_spanned(name, "ExtensionSchema does not support tuple structs")
                        .to_compile_error(),
                );
                continue;
            }
        };

        let mut desc: Option<String> = None;

        for attr in &field.attrs {
            if !attr.path().is_ident("description") {
                continue;
            }
            if desc.is_some() {
                errors.push(
                    syn::Error::new_spanned(attr, "duplicate #[description] on the same field")
                        .to_compile_error(),
                );
                continue;
            }
            match attr.parse_args::<syn::LitStr>() {
                Ok(s) => desc = Some(s.value()),
                Err(_) => errors.push(
                    syn::Error::new_spanned(
                        attr,
                        "#[description] expects a single string literal, \
                         e.g. #[description(\"My field description\")]",
                    )
                    .to_compile_error(),
                ),
            }
        }

        let desc = match desc {
            Some(d) => d,
            None => {
                errors.push(
                    syn::Error::new_spanned(
                        field,
                        format!(
                            "field `{field_name}` is missing a #[description(\"...\")] attribute"
                        ),
                    )
                    .to_compile_error(),
                );
                continue;
            }
        };

        props_entries.push(quote! {
            #field_name: { "type": "string", "description": #desc }
        });
        required_keys.push(field_name);
    }

    if !errors.is_empty() {
        return quote! { #(#errors)* }.into();
    }

    quote! {
        impl crate::extensions::traits::ExtensionSchema for #name {
            fn schema() -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { #(#props_entries),* },
                    "required": [#(#required_keys),*]
                })
            }
        }
    }
    .into()
}

/// Marks a method as a data fetcher inside an [`extension`] impl block.
///
/// Options:
/// - `cache = "startup" | "per_request" | "<n>m" | "<n>h" | "<n>d"` — cache strategy
/// - `embeddable = true | false` — if true, fetched at startup for the knowledge base
/// - `description = "..."` — shown to the AI as the tool description
#[proc_macro_attribute]
pub fn fetch(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Marks a method as an action inside an [`extension`] impl block.
///
/// Options:
/// - `description = "..."` — shown to the AI as the tool description
#[proc_macro_attribute]
pub fn action(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Marks a method as a hook handler inside an [`extension`] impl block.
///
/// The method is called whenever the named event fires anywhere in the system.
/// Failures are logged as warnings — they do not affect the caller.
///
/// # Attribute
/// ```ignore
/// #[hook(event = "issue::proposed")]
/// ```
///
/// # Method signature
/// ```ignore
/// async fn handler_name(&self, payload: ThePayloadType) -> anyhow::Result<()>
/// ```
/// The argument type must match the payload emitted for the chosen event:
///
/// | Event | Payload type |
/// |---|---|
/// | `issue::proposed` | `crate::issues::IssueProposedHook` |
/// | `issue::accepted` | `crate::issues::IssueAcceptedHook` |
/// | `issue::rejected` | `crate::issues::IssueRejectedHook` |
/// | `issue::ended`    | `crate::issues::IssueEndedHook`    |
///
/// Using an unrecognised event string is a **compile error**.
#[proc_macro_attribute]
pub fn hook(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}

struct FetchAttr {
    cache: Option<String>,
    embeddable: bool,
    description: String,
}

struct ActionAttr {
    description: String,
}

struct HookAttr {
    event: String,
}

/// Returns true for recognised cache strings: `startup`, `per_request`, or `<n>m/h/d`.
fn is_valid_cache_str(s: &str) -> bool {
    if matches!(s, "startup" | "per_request") {
        return true;
    }
    for suffix in ['m', 'h', 'd'] {
        if let Some(n) = s.strip_suffix(suffix) {
            if n.parse::<u64>().is_ok() {
                return true;
            }
        }
    }
    false
}

fn parse_fetch_attr(attr: &Attribute) -> Result<FetchAttr, TokenStream2> {
    let mut cache: Option<String> = None;
    let mut embeddable = false;
    let mut description = String::new();

    let meta_list = match &attr.meta {
        Meta::List(ml) => ml,
        Meta::Path(_) => return Ok(FetchAttr { cache, embeddable, description }),
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "#[fetch] expects a parenthesised argument list, \
                 e.g. #[fetch(cache = \"5m\", description = \"...\")]",
            )
            .to_compile_error());
        }
    };

    let nested = match meta_list.parse_args_with(
        syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
    ) {
        Ok(n) => n,
        Err(e) => return Err(e.to_compile_error()),
    };

    for item in nested {
        match item {
            Meta::NameValue(nv) => {
                let key = nv
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();

                match key.as_str() {
                    "cache" => match &nv.value {
                        syn::Expr::Lit(el) => match &el.lit {
                            syn::Lit::Str(s) => {
                                let val = s.value();
                                if !is_valid_cache_str(&val) {
                                    return Err(syn::Error::new_spanned(
                                        &nv.value,
                                        format!(
                                            "invalid cache value `{val}`; expected `startup`, \
                                             `per_request`, or a duration like `5m`, `2h`, `1d`"
                                        ),
                                    )
                                    .to_compile_error());
                                }
                                cache = Some(val);
                            }
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    &nv.value,
                                    "cache must be a string literal \
                                     (e.g. cache = \"5m\")",
                                )
                                .to_compile_error());
                            }
                        },
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "cache must be a string literal (e.g. cache = \"5m\")",
                            )
                            .to_compile_error());
                        }
                    },

                    "embeddable" => match &nv.value {
                        syn::Expr::Lit(el) => match &el.lit {
                            syn::Lit::Bool(b) => embeddable = b.value(),
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    &nv.value,
                                    "embeddable must be a boolean literal \
                                     (true or false)",
                                )
                                .to_compile_error());
                            }
                        },
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "embeddable must be a boolean literal (true or false)",
                            )
                            .to_compile_error());
                        }
                    },

                    "description" => match &nv.value {
                        syn::Expr::Lit(el) => match &el.lit {
                            syn::Lit::Str(s) => description = s.value(),
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    &nv.value,
                                    "description must be a string literal",
                                )
                                .to_compile_error());
                            }
                        },
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "description must be a string literal",
                            )
                            .to_compile_error());
                        }
                    },

                    _ => {
                        return Err(syn::Error::new_spanned(
                            &nv.path,
                            format!(
                                "unknown key `{key}`; valid keys for #[fetch] are: \
                                 cache, embeddable, description"
                            ),
                        )
                        .to_compile_error());
                    }
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    &other,
                    "expected a key = value pair \
                     (e.g. description = \"Look up account\")",
                )
                .to_compile_error());
            }
        }
    }

    Ok(FetchAttr { cache, embeddable, description })
}

fn parse_action_attr(attr: &Attribute) -> Result<ActionAttr, TokenStream2> {
    let mut description = String::new();

    let meta_list = match &attr.meta {
        Meta::List(ml) => ml,
        Meta::Path(_) => return Ok(ActionAttr { description }),
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "#[action] expects a parenthesised argument list, \
                 e.g. #[action(description = \"...\")]",
            )
            .to_compile_error());
        }
    };

    let nested = match meta_list.parse_args_with(
        syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
    ) {
        Ok(n) => n,
        Err(e) => return Err(e.to_compile_error()),
    };

    for item in nested {
        match item {
            Meta::NameValue(nv) => {
                let key = nv
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();

                match key.as_str() {
                    "description" => match &nv.value {
                        syn::Expr::Lit(el) => match &el.lit {
                            syn::Lit::Str(s) => description = s.value(),
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    &nv.value,
                                    "description must be a string literal",
                                )
                                .to_compile_error());
                            }
                        },
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "description must be a string literal",
                            )
                            .to_compile_error());
                        }
                    },
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &nv.path,
                            format!(
                                "unknown key `{key}`; \
                                 the only valid key for #[action] is: description"
                            ),
                        )
                        .to_compile_error());
                    }
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    &other,
                    "expected a key = value pair \
                     (e.g. description = \"Send password reset email\")",
                )
                .to_compile_error());
            }
        }
    }

    Ok(ActionAttr { description })
}

fn parse_hook_attr(attr: &Attribute) -> Result<HookAttr, TokenStream2> {
    let meta_list = match &attr.meta {
        Meta::List(ml) => ml,
        Meta::Path(_) => {
            return Err(syn::Error::new_spanned(
                attr,
                "#[hook] requires event = \"...\", e.g. #[hook(event = \"issue::proposed\")]",
            )
            .to_compile_error());
        }
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "#[hook] expects a parenthesised argument list, \
                 e.g. #[hook(event = \"issue::proposed\")]",
            )
            .to_compile_error());
        }
    };

    let nested = match meta_list.parse_args_with(
        syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
    ) {
        Ok(n) => n,
        Err(e) => return Err(e.to_compile_error()),
    };

    let mut event: Option<String> = None;

    for item in nested {
        match item {
            Meta::NameValue(nv) => {
                let key = nv
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();

                match key.as_str() {
                    "event" => match &nv.value {
                        syn::Expr::Lit(el) => match &el.lit {
                            syn::Lit::Str(s) => {
                                let val = s.value();
                                if !VALID_HOOK_EVENTS.contains(&val.as_str()) {
                                    let valid = VALID_HOOK_EVENTS.join(", ");
                                    return Err(syn::Error::new_spanned(
                                        &nv.value,
                                        format!(
                                            "\"{val}\" is not a recognised hook event; \
                                             valid events are: {valid}"
                                        ),
                                    )
                                    .to_compile_error());
                                }
                                event = Some(val);
                            }
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    &nv.value,
                                    "event must be a string literal",
                                )
                                .to_compile_error());
                            }
                        },
                        _ => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                "event must be a string literal",
                            )
                            .to_compile_error());
                        }
                    },
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &nv.path,
                            format!(
                                "unknown key `{key}`; the only valid key for #[hook] is: event"
                            ),
                        )
                        .to_compile_error());
                    }
                }
            }
            other => {
                return Err(syn::Error::new_spanned(
                    &other,
                    "expected a key = value pair (e.g. event = \"issue::proposed\")",
                )
                .to_compile_error());
            }
        }
    }

    match event {
        Some(e) => Ok(HookAttr { event: e }),
        None => Err(syn::Error::new_spanned(
            attr,
            "#[hook] requires event = \"...\", e.g. #[hook(event = \"issue::proposed\")]",
        )
        .to_compile_error()),
    }
}

fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(t) if t.elems.is_empty())
}

fn parse_cache(cache: &Option<String>) -> TokenStream2 {
    match cache.as_deref() {
        Some("startup") => {
            quote! { crate::extensions::traits::CacheStrategy::Startup }
        }
        Some("per_request") | None => {
            quote! { crate::extensions::traits::CacheStrategy::PerRequest }
        }
        Some(s) => {
            let secs = if let Some(n) = s.strip_suffix('m') {
                n.parse::<u64>().ok().map(|x| x * 60)
            } else if let Some(n) = s.strip_suffix('h') {
                n.parse::<u64>().ok().map(|x| x * 3600)
            } else if let Some(n) = s.strip_suffix('d') {
                n.parse::<u64>().ok().map(|x| x * 86400)
            } else {
                None
            };
            match secs {
                Some(secs) => quote! {
                    crate::extensions::traits::CacheStrategy::Ttl(
                        std::time::Duration::from_secs(#secs)
                    )
                },
                None => quote! { crate::extensions::traits::CacheStrategy::PerRequest },
            }
        }
    }
}

fn build_handler_and_schema(
    method_name: &syn::Ident,
    arg_type: &Type,
) -> (TokenStream2, TokenStream2) {
    if is_unit_type(arg_type) {
        let handler = quote! {
            Box::new(move |_args: serde_json::Value| {
                let arc_self = std::sync::Arc::clone(&arc_self);
                Box::pin(async move { arc_self.#method_name(()).await })
            })
        };
        let schema = quote! { serde_json::json!({"type": "object", "properties": {}}) };
        (handler, schema)
    } else {
        let handler = quote! {
            Box::new(move |args: serde_json::Value| {
                let arc_self = std::sync::Arc::clone(&arc_self);
                Box::pin(async move {
                    let typed: #arg_type = serde_json::from_value(args)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    arc_self.#method_name(typed).await
                })
            })
        };
        let schema =
            quote! { <#arg_type as crate::extensions::traits::ExtensionSchema>::schema() };
        (handler, schema)
    }
}

fn build_hook_handler(method_name: &syn::Ident, arg_type: &Type) -> TokenStream2 {
    quote! {
        Box::new(move |payload: serde_json::Value| {
            let arc_self = std::sync::Arc::clone(&arc_self);
            Box::pin(async move {
                let typed: #arg_type = serde_json::from_value(payload)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                arc_self.#method_name(typed).await
            })
        })
    }
}

/// Attribute macro placed on an `impl` block.
///
/// Scans for `#[fetch(...)]` / `#[action(...)]` / `#[hook(...)]` methods and emits a companion
/// `impl ExtensionTrait for T` block.  The inner attributes are left in place so the compiler
/// can expand them (requiring an explicit import).
#[proc_macro_attribute]
pub fn extension(args: TokenStream, input: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[extension] takes no arguments",
        )
        .to_compile_error()
        .into();
    }

    let impl_block = parse_macro_input!(input as ItemImpl);

    let self_ty = impl_block.self_ty.clone();
    let type_name = quote!(#self_ty).to_string().replace(' ', "");

    let mut fetch_descriptors: Vec<TokenStream2> = Vec::new();
    let mut action_descriptors: Vec<TokenStream2> = Vec::new();
    let mut hook_descriptors: Vec<TokenStream2> = Vec::new();
    let mut errors: Vec<TokenStream2> = Vec::new();

    for item in &impl_block.items {
        let ImplItem::Fn(method) = item else { continue };

        let has_fetch = method.attrs.iter().any(|a| a.path().is_ident("fetch"));
        let has_action = method.attrs.iter().any(|a| a.path().is_ident("action"));
        let has_hook = method.attrs.iter().any(|a| a.path().is_ident("hook"));

        if has_fetch {
            let fetch_attr = method
                .attrs
                .iter()
                .find(|a| a.path().is_ident("fetch"))
                .cloned()
                .unwrap();

            let parsed = match parse_fetch_attr(&fetch_attr) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };

            let method_name = method.sig.ident.clone();
            let method_name_str = method_name.to_string();
            let description = parsed.description;
            let embeddable = parsed.embeddable;
            let cache_ts = parse_cache(&parsed.cache);

            let arg_type: Type = match method.sig.inputs.iter().nth(1) {
                Some(FnArg::Typed(pt)) => (*pt.ty).clone(),
                _ => {
                    errors.push(
                        syn::Error::new_spanned(
                            &method.sig,
                            "a #[fetch] method must have exactly one typed arg after &self",
                        )
                        .to_compile_error(),
                    );
                    continue;
                }
            };

            let (handler_ts, schema_ts) = build_handler_and_schema(&method_name, &arg_type);

            fetch_descriptors.push(quote! {
                {
                    let arc_self = std::sync::Arc::clone(&arc_self);
                    crate::extensions::traits::FetchDescriptor {
                        name: #method_name_str,
                        description: #description,
                        embeddable: #embeddable,
                        cache: #cache_ts,
                        schema: #schema_ts,
                        handler: #handler_ts,
                    }
                }
            });
        } else if has_action {
            let action_attr = method
                .attrs
                .iter()
                .find(|a| a.path().is_ident("action"))
                .cloned()
                .unwrap();

            let parsed = match parse_action_attr(&action_attr) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };

            let method_name = method.sig.ident.clone();
            let method_name_str = method_name.to_string();
            let description = parsed.description;

            let arg_type: Type = match method.sig.inputs.iter().nth(1) {
                Some(FnArg::Typed(pt)) => (*pt.ty).clone(),
                _ => {
                    errors.push(
                        syn::Error::new_spanned(
                            &method.sig,
                            "an #[action] method must have exactly one typed arg after &self",
                        )
                        .to_compile_error(),
                    );
                    continue;
                }
            };

            let (handler_ts, schema_ts) = build_handler_and_schema(&method_name, &arg_type);

            action_descriptors.push(quote! {
                {
                    let arc_self = std::sync::Arc::clone(&arc_self);
                    crate::extensions::traits::ActionDescriptor {
                        name: #method_name_str,
                        description: #description,
                        schema: #schema_ts,
                        handler: #handler_ts,
                    }
                }
            });
        } else if has_hook {
            let hook_attr = method
                .attrs
                .iter()
                .find(|a| a.path().is_ident("hook"))
                .cloned()
                .unwrap();

            let parsed = match parse_hook_attr(&hook_attr) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };

            let method_name = method.sig.ident.clone();
            let event_str = parsed.event;

            let arg_type: Type = match method.sig.inputs.iter().nth(1) {
                Some(FnArg::Typed(pt)) => (*pt.ty).clone(),
                _ => {
                    errors.push(
                        syn::Error::new_spanned(
                            &method.sig,
                            "a #[hook] method must have exactly one typed arg after &self",
                        )
                        .to_compile_error(),
                    );
                    continue;
                }
            };

            let handler_ts = build_hook_handler(&method_name, &arg_type);

            hook_descriptors.push(quote! {
                {
                    let arc_self = std::sync::Arc::clone(&arc_self);
                    crate::extensions::traits::HookDescriptor {
                        event: #event_str,
                        handler: #handler_ts,
                    }
                }
            });
        }
    }

    if !errors.is_empty() {
        let combined = quote! { #(#errors)* };
        return combined.into();
    }

    let hooks_method = if hook_descriptors.is_empty() {
        quote! {}
    } else {
        quote! {
            fn hooks(self: std::sync::Arc<Self>) -> Vec<crate::extensions::traits::HookDescriptor> {
                let arc_self = self;
                vec![#(#hook_descriptors),*]
            }
        }
    };

    let trait_impl = quote! {
        impl crate::extensions::traits::ExtensionTrait for #self_ty {
            fn name(&self) -> &'static str {
                #type_name
            }

            fn fetchers(self: std::sync::Arc<Self>) -> Vec<crate::extensions::traits::FetchDescriptor> {
                let arc_self = self;
                vec![#(#fetch_descriptors),*]
            }

            fn actions(self: std::sync::Arc<Self>) -> Vec<crate::extensions::traits::ActionDescriptor> {
                let arc_self = self;
                vec![#(#action_descriptors),*]
            }

            #hooks_method
        }
    };

    quote! {
        #impl_block
        #trait_impl
    }
    .into()
}
