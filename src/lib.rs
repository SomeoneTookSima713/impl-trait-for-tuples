extern crate proc_macro;

use proc_macro2::{Span, TokenStream};

use std::iter::repeat;

use syn::{
    parse_macro_input, parse_quote,
    spanned::Spanned,
    visit::{self, Visit},
    Error, FnArg, Generics, Ident, Index, ItemTrait, LitInt, Pat, Result, ReturnType, Signature,
    TraitItem, TraitItemMethod, Type,
};

use quote::quote;

#[proc_macro_attribute]
pub fn impl_for_tuples(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let definition = parse_macro_input!(input as ItemTrait);
    let count = parse_macro_input!(args as LitInt);

    impl_for_tuples_impl(definition, count)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn impl_for_tuples_impl(definition: ItemTrait, count: LitInt) -> Result<TokenStream> {
    check_trait_declaration(&definition)?;

    let tuple_elements = (0usize..count.base10_parse()?)
        .map(|i| generate_tuple_element_ident(i))
        .collect::<Vec<_>>();

    let impls = (0..=tuple_elements.len())
        .filter(|i| *i != 1)
        .map(|i| generate_tuple_impl(&definition, &tuple_elements[0..i]));

    Ok(quote!(
        #definition
        #( #impls )*
    ))
}

fn check_trait_declaration(trait_decl: &ItemTrait) -> Result<()> {
    let mut visitor = CheckTraitDeclaration { errors: Vec::new() };
    visit::visit_item_trait(&mut visitor, trait_decl);

    match visitor.errors.pop() {
        Some(init) => Err(visitor.errors.into_iter().fold(init, |mut old, new| {
            old.combine(new);
            old
        })),
        None => Ok(()),
    }
}

/// Checks that the given trait declaration corresponds to the expected format.
struct CheckTraitDeclaration {
    /// Stores all errors that are found.
    errors: Vec<Error>,
}

const CHECK_ERROR_MSG: &str =
    "Not supported by full-automatic tuple implementation. \
     Use semi-automatic tuple implementation for more control of the implementation.";

impl<'ast> Visit<'ast> for CheckTraitDeclaration {
    fn visit_trait_item(&mut self, ti: &'ast TraitItem) {
        match ti {
            TraitItem::Method(m) => visit::visit_trait_item_method(self, m),
            _ => self.errors.push(Error::new(ti.span(), CHECK_ERROR_MSG)),
        }
    }

    fn visit_return_type(&mut self, rt: &'ast ReturnType) {
        match rt {
            ReturnType::Default => {}
            ReturnType::Type(_, _) => self.errors.push(Error::new(rt.span(), CHECK_ERROR_MSG)),
        }
    }
}

fn generate_tuple_impl(definition: &ItemTrait, tuple_elements: &[Ident]) -> TokenStream {
    let name = &definition.ident;
    let unsafety = &definition.unsafety;
    let generics = generate_generics(definition, tuple_elements);
    let ty_generics = definition.generics.split_for_impl().1;
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let fns = definition.items.iter().filter_map(|i| match i {
        TraitItem::Method(method) => Some(generate_delegate_method(method, tuple_elements)),
        _ => None,
    });

    quote!(
        #[allow(unused)]
        #unsafety impl #impl_generics #name #ty_generics for ( #( #tuple_elements ),* ) #where_clause {
            #( #fns )*
        }
    )
}

/// Collects all non-reference argument types.
struct CollectNonReferenceArgTypes {
    result: Vec<Type>,
}

impl<'ast> Visit<'ast> for CollectNonReferenceArgTypes {
    fn visit_type(&mut self, ty: &'ast Type) {
        if !is_reference_type(ty) {
            self.result.push(ty.clone());
        }
    }
}

fn generate_generics(definition: &ItemTrait, tuple_elements: &[Ident]) -> Generics {
    let mut generics = definition.generics.clone();
    let name = &definition.ident;
    let ty_generics = definition.generics.split_for_impl().1;

    // Make sure that all non-reference types implement `Clone`.
    let mut visitor = CollectNonReferenceArgTypes { result: Vec::new() };
    definition
        .items
        .iter()
        .for_each(|item| visit::visit_trait_item(&mut visitor, item));
    visitor.result.dedup();
    {
        let where_clause = generics.make_where_clause();
        visitor
            .result
            .into_iter()
            .for_each(|ty| where_clause.predicates.push(parse_quote!(#ty: Clone)));
    }

    tuple_elements.iter().for_each(|tuple_element| {
        generics
            .params
            .push(parse_quote!(#tuple_element : #name #ty_generics));
    });

    generics
}

fn generate_tuple_element_ident(num: usize) -> Ident {
    Ident::new(&format!("TupleElement{}", num), Span::call_site())
}

fn generate_delegate_method(method: &TraitItemMethod, tuple_elements: &[Ident]) -> TokenStream {
    let name = repeat(&method.sig.ident);
    let self_arg = method
        .sig
        .inputs
        .first()
        .map(|a| match a {
            FnArg::Receiver(_) => true,
            _ => false,
        })
        .unwrap_or(false);
    let (sig, arg_names, arg_clones) = update_signature_and_extract_arg_infos(method.sig.clone());
    let arg_names_per_tuple = repeat(&arg_names);
    let arg_clones_per_tuple = repeat(&arg_clones);
    let tuple_access = tuple_elements.iter().enumerate().map(|(i, te)| {
        if self_arg {
            let index = Index::from(i);
            quote!(self.#index.)
        } else {
            quote!(#te::)
        }
    });

    let mut res = method.clone();
    res.sig = sig;
    res.semi_token = None;
    res.default = Some(parse_quote!(
        { #( #tuple_access #name ( #( #arg_names_per_tuple #arg_clones_per_tuple ),* ); )* }
    ));

    quote!( #res )
}

/// Update `Signature` by replacing all wild card argument names with unique identifiers, collect all
/// argument names and if we need to clone an argument.
fn update_signature_and_extract_arg_infos(
    mut sig: Signature,
) -> (Signature, Vec<Ident>, Vec<TokenStream>) {
    let mut unique_id = 0;
    let mut arg_clone = Vec::new();
    let mut push_arg_clone = |is_ref: bool| {
        if is_ref {
            arg_clone.push(quote!())
        } else {
            arg_clone.push(quote!(.clone()))
        }
    };

    let arg_names = sig
        .inputs
        .iter_mut()
        .filter_map(|a| match a {
            FnArg::Typed(ref mut arg) => match &mut *arg.pat {
                Pat::Ident(p) => {
                    push_arg_clone(is_reference_type(&arg.ty));
                    Some(p.ident.clone())
                }
                Pat::Wild(_) => {
                    push_arg_clone(is_reference_type(&arg.ty));
                    let ident = Ident::new(
                        &format!("tuple_element_unique_ident_{}", unique_id),
                        Span::call_site(),
                    );
                    unique_id += 1;
                    let ident2 = ident.clone();

                    arg.pat = parse_quote!(#ident);
                    Some(ident2)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();

    (sig, arg_names, arg_clone)
}

fn is_reference_type(ty: &Type) -> bool {
    match ty {
        Type::Reference(_) => true,
        _ => false,
    }
}
