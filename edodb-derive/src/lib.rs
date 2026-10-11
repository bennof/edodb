use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Attribute, Data, DeriveInput, Fields, LitStr};

#[proc_macro_derive(DbModel, attributes(db))]
pub fn derive_db_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(|e| e.to_compile_error()).into()
}
/// Column names and values come from the serde JSON of the model, so serde options
/// that rename, drop or reshape fields would silently break the mapping.
const FORBIDDEN_CONTAINER_SERDE: &[&str] = &["rename_all", "rename_all_fields", "transparent", "tag",
    "content", "untagged", "into", "from", "try_from", "remote"];
const FORBIDDEN_FIELD_SERDE: &[&str] = &["rename", "skip", "skip_serializing", "skip_deserializing",
    "skip_serializing_if", "flatten", "with", "serialize_with", "deserialize_with", "getter"];
fn reject_serde(attrs: &[Attribute], forbidden: &[&str]) -> syn::Result<()> {
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            if let Some(name) = forbidden.iter().find(|f| meta.path.is_ident(f)) {
                return Err(meta.error(format!("#[serde({name})] is not supported with DbModel: column names must match the Rust field names")));
            }
            // Skip the option's value, e.g. `default = "..."` or `bound(...)`.
            if meta.input.peek(syn::Token![=]) { meta.value()?.parse::<syn::Expr>()?; }
            else if meta.input.peek(syn::token::Paren) {
                let content; syn::parenthesized!(content in meta.input);
                content.parse::<proc_macro2::TokenStream>()?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let mut table = name.to_string().to_lowercase();
    reject_serde(&input.attrs, FORBIDDEN_CONTAINER_SERDE)?;
    for attr in &input.attrs {
        if attr.path().is_ident("db") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("table") {
                    table = meta.value()?.parse::<LitStr>()?.value(); Ok(())
                } else { Err(meta.error("unknown db option")) }
            })?;
        }
    }
    let Data::Struct(data) = &input.data else { return Err(syn::Error::new_spanned(name,"DbModel requires struct")); };
    let Fields::Named(fields) = &data.fields else { return Err(syn::Error::new_spanned(name,"DbModel requires named fields")); };
    let mut columns = Vec::new();
    let mut field_types = Vec::new();
    let mut primary_count = 0;
    for field in &fields.named {
        let ident = field.ident.as_ref().unwrap();
        reject_serde(&field.attrs, FORBIDDEN_FIELD_SERDE)?;
        let column_name = syn::ext::IdentExt::unraw(ident).to_string();
        let ty = &field.ty;
        field_types.push(ty);
        let mut pk = false;
        let mut unique = false;
        for attr in &field.attrs {
            if attr.path().is_ident("db") {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("primary_key") { pk=true; Ok(()) }
                    else if meta.path.is_ident("unique") { unique=true; Ok(()) }
                    else { Err(meta.error("unknown field db option")) }
                })?;
            }
        }
        if pk { primary_count += 1; }
        columns.push(quote! {
            ::edodb::Column {
                name: #column_name,
                kind: <#ty as ::edodb::FieldType>::KIND,
                nullable: <#ty as ::edodb::FieldType>::NULLABLE,
                primary_key: #pk,
                unique: #unique,
            }
        });
    }
    if primary_count != 1 { return Err(syn::Error::new_spanned(name,"exactly one #[db(primary_key)] required")); }
    // Generic models: every field type must be a FieldType, and the concrete type must
    // satisfy Model's serde/thread bounds (serde derives add `T: Serialize` etc. itself).
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut where_clause = where_clause.cloned().unwrap_or_else(|| syn::parse_quote!(where));
    if !input.generics.params.is_empty() {
        for ty in field_types { where_clause.predicates.push(syn::parse_quote!(#ty: ::edodb::FieldType)); }
        where_clause.predicates.push(syn::parse_quote!(
            #name #ty_generics: ::edodb::__private::serde::Serialize
                + ::edodb::__private::serde::de::DeserializeOwned + ::core::marker::Send + ::core::marker::Sync
        ));
    }
    Ok(quote! {
        impl #impl_generics ::edodb::Model for #name #ty_generics #where_clause {
            const SCHEMA: ::edodb::Schema = ::edodb::Schema {
                table: #table,
                columns: &[#(#columns),*],
            };
        }
    })
}
