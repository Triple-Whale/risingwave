// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use bae::FromAttributes;
use proc_macro2::TokenStream;
use proc_macro_error::{abort, OptionExt, ResultExt};
use quote::{format_ident, quote, quote_spanned};
use syn::DeriveInput;

#[derive(FromAttributes)]
struct Parameter {
    pub rename: Option<syn::LitStr>,
    pub default: syn::Expr,
    pub flags: Option<syn::LitStr>,
    pub check_hook: Option<syn::Expr>,
}

pub(crate) fn derive_config(input: DeriveInput) -> TokenStream {
    let syn::Data::Struct(syn::DataStruct { fields, .. }) = input.data else {
        abort!(input, "Only struct is supported");
    };

    let mut default_fields = vec![];
    let mut struct_impl_set = vec![];
    let mut struct_impl_get = vec![];
    let mut struct_impl_reset = vec![];
    let mut set_match_branches = vec![];
    let mut get_match_branches = vec![];
    let mut reset_match_branches = vec![];
    let mut show_all_list = vec![];

    for field in fields {
        let field_ident = field.ident.expect_or_abort("Field need to be named");
        let ty = field.ty;

        let mut doc_list = vec![];
        for attr in &field.attrs {
            if attr.path.is_ident("doc") {
                let meta = attr.parse_meta().expect_or_abort("Failed to parse meta");
                if let syn::Meta::NameValue(val) = meta {
                    if let syn::Lit::Str(desc) = val.lit {
                        doc_list.push(desc.value().trim().to_string());
                    }
                }
            }
        }

        let description: TokenStream = format!("r#\"{}\"#", doc_list.join(" ")).parse().unwrap();

        let attr =
            Parameter::from_attributes(&field.attrs).expect_or_abort("Failed to parse attribute");
        let Parameter {
            rename,
            default,
            flags,
            check_hook: check_hook_name,
        } = attr;

        let entry_name = if let Some(rename) = rename {
            if !(rename.value().is_ascii() && rename.value().to_ascii_lowercase() == rename.value())
            {
                abort!(rename, "Expect `rename` to be an ascii lower case string");
            }
            quote! {#rename}
        } else {
            let ident = format_ident!("{}", field_ident.to_string().to_lowercase());
            quote! {stringify!(#ident)}
        };

        let flags = flags.map(|f| f.value()).unwrap_or_default();
        let flags: Vec<_> = flags.split('|').map(|str| str.trim()).collect();

        default_fields.push(quote_spanned! {
            field_ident.span()=>
            #field_ident: #default.into(),
        });

        let set_func_name = format_ident!("set_{}_str", field_ident);
        let set_t_func_name = format_ident!("set_{}", field_ident);
        let set_t_inner_func_name = format_ident!("set_{}_inner", field_ident);
        let set_t_func_doc: TokenStream =
            format!("r#\"Set parameter {} by a typed value.\"#", entry_name)
                .parse()
                .unwrap();
        let set_func_doc: TokenStream = format!("r#\"Set parameter {} by a string.\"#", entry_name)
            .parse()
            .unwrap();

        let gen_set_func_name = if flags.contains(&"SETTER") {
            set_t_inner_func_name.clone()
        } else {
            set_t_func_name.clone()
        };

        let check_hook = if let Some(check_hook_name) = check_hook_name {
            quote! {
                #check_hook_name(&val).map_err(|e| {
                    SessionConfigError::InvalidValue {
                        entry: #entry_name,
                        value: val.to_string(),
                        source: anyhow::anyhow!(e),
                    }
                })?;
            }
        } else {
            quote! {}
        };

        let report_hook = if flags.contains(&"REPORT") {
            quote! {
                if self.#field_ident != val {
                    reporter.report_status(#entry_name, val.to_string());
                }
            }
        } else {
            quote! {}
        };

        struct_impl_set.push(quote! {
            #[doc = #set_func_doc]
            pub fn #set_func_name(
                &mut self,
                val: &str,
                reporter: &mut impl ConfigReporter
            ) -> SessionConfigResult<()> {
                let val_t = <#ty as ::std::str::FromStr>::from_str(val).map_err(|e| {
                    SessionConfigError::InvalidValue {
                        entry: #entry_name,
                        value: val.to_string(),
                        source: anyhow::anyhow!(e),
                    }
                })?;

                self.#set_t_func_name(val_t, reporter)?;
                Ok(())
            }

            #[doc = #set_t_func_doc]
            pub fn #gen_set_func_name(
                &mut self,
                val: #ty,
                reporter: &mut impl ConfigReporter
            ) -> SessionConfigResult<()> {
                #check_hook
                #report_hook

                self.#field_ident = val;
                Ok(())
            }

        });

        let reset_func_name = format_ident!("reset_{}", field_ident);
        struct_impl_reset.push(quote! {

        #[allow(clippy::useless_conversion)]
        pub fn #reset_func_name(&mut self, reporter: &mut impl ConfigReporter) {
                let val = #default;
                #report_hook
                self.#field_ident = val.into();
            }
        });

        let get_func_name = format_ident!("{}_str", field_ident);
        let get_t_func_name = format_ident!("{}", field_ident);
        let get_func_doc: TokenStream =
            format!("r#\"Get a value string of parameter {} \"#", entry_name)
                .parse()
                .unwrap();
        let get_t_func_doc: TokenStream =
            format!("r#\"Get a typed value of parameter {} \"#", entry_name)
                .parse()
                .unwrap();

        struct_impl_get.push(quote! {
            #[doc = #get_func_doc]
            pub fn #get_func_name(&self) -> String {
                self.#get_t_func_name().to_string()
            }

            #[doc = #get_t_func_doc]
            pub fn #get_t_func_name(&self) -> #ty {
                self.#field_ident.clone()
            }

        });

        get_match_branches.push(quote! {
            #entry_name => Ok(self.#get_func_name()),
        });

        set_match_branches.push(quote! {
            #entry_name => self.#set_func_name(&value, reporter),
        });

        reset_match_branches.push(quote! {
            #entry_name => Ok(self.#reset_func_name(reporter)),
        });

        if !flags.contains(&"NO_SHOW_ALL") {
            show_all_list.push(quote! {
                VariableInfo {
                    name: #entry_name.to_string(),
                    setting: self.#field_ident.to_string(),
                    description : #description.to_string(),
                },

            });
        }
    }

    let struct_ident = input.ident;
    quote! {
        impl Default for #struct_ident {
            #[allow(clippy::useless_conversion)]
            fn default() -> Self {
                Self {
                    #(#default_fields)*
                }
            }
        }

        impl #struct_ident {
            fn new() -> Self {
                Default::default()
            }

            #(#struct_impl_get)*

            #(#struct_impl_set)*

            #(#struct_impl_reset)*

            /// Set a parameter given it's name and value string.
            pub fn set(&mut self, key_name: &str, value: String, reporter: &mut impl ConfigReporter) -> SessionConfigResult<()> {
                match key_name.to_ascii_lowercase().as_ref() {
                    #(#set_match_branches)*
                    _ => Err(SessionConfigError::UnrecognizedEntry(key_name.to_string())),
                }
            }

            /// Get a parameter by it's name.
            pub fn get(&self, key_name: &str) -> SessionConfigResult<String> {
                match key_name.to_ascii_lowercase().as_ref() {
                    #(#get_match_branches)*
                    _ => Err(SessionConfigError::UnrecognizedEntry(key_name.to_string())),
                }
            }

            /// Reset a parameter by it's name.
            pub fn reset(&mut self, key_name: &str, reporter: &mut impl ConfigReporter) -> SessionConfigResult<()> {
                match key_name.to_ascii_lowercase().as_ref() {
                    #(#reset_match_branches)*
                    _ => Err(SessionConfigError::UnrecognizedEntry(key_name.to_string())),
                }
            }

            /// Show all parameters.
            pub fn show_all(&self) -> Vec<VariableInfo> {
                vec![
                    #(#show_all_list)*
                ]
            }
        }
    }
}
