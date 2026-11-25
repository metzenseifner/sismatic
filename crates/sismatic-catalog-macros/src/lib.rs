//! `#[catalog]`: turn an ordinary, fieldless enum into an instruction catalog.
//!
//! # One declaration per instruction enum
//!
//! Each of `Query`, `Command`, `Register` and `Setting` in `sismatic-core`
//! declares its names in a single place: the enum itself. The macro derives
//! everything that must stay in lockstep — the `ALL` catalog, the canonical
//! `name()`, the accepted input spellings, `description()`, `FromStr`, and
//! `Display` — so the parser and the generated documentation (the HTTP docs,
//! the Python stub) can never disagree about which strings are valid.
//!
//! Wire encoding (payloads, parsers, register indices, command verbs) is a
//! separate concern and stays in each enum's own module, as ordinary `impl`
//! blocks next to the declaration.
//!
//! # The surface
//!
//! The declaration is plain Rust plus two helper attributes:
//!
//! ```ignore
//! /// A built-in recorder command.
//! #[catalog]
//! pub enum Command {
//!     /// Start recording.
//!     #[name("STARTRECORDING")]
//!     #[alias("START")]
//!     Start,
//! }
//! ```
//!
//! * `/// text` — the variant's **first paragraph** (lines trimmed, joined with
//!   single spaces, up to the first blank `///` line) becomes `description()`.
//!   It is still the variant's rustdoc, so the two can never drift apart. Plain
//!   `//` comments between variants are notes for the reader and are *not* part
//!   of any description.
//! * `#[name("…")]` — the canonical wire name. It **replaces** the default; it
//!   never adds the default as an extra spelling. The default is the variant in
//!   `UPPER_SNAKE_CASE`, with `_` before every uppercase letter but none before a
//!   digit: `RunningState` → `RUNNING_STATE`, but `Stream1State` →
//!   `STREAM1_STATE` and `HTTPPort` → `H_T_T_P_PORT`. Write `#[name]` whenever
//!   the default is not what the wire calls it.
//! * `#[alias("…", …)]` — further accepted spellings. May be repeated; the
//!   spellings concatenate in source order.
//!
//! `accepted()` is the canonical name followed by the aliases in source order,
//! and is exactly the set `FromStr` matches. `ALL` is declaration order.
//!
//! The macro adds `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`; do not derive
//! those yourself. `Copy` is load-bearing: the generated methods take `self` by
//! value, and `Display` calls `self.name()` through `&self`.
//!
//! # Normalization
//!
//! `FromStr` upper-cases its input and turns `-` into `_` before matching
//! (`sismatic_core::protocol::payload_helpers::normalize`), which makes every
//! lookup case-insensitive and `-`/`_`-insensitive for free. A spelling is
//! therefore only reachable if it is *itself* in that normalized form, and the
//! macro rejects any that is not, at compile time. A proc-macro crate cannot
//! call into the crate it is expanding in, so the rule is duplicated here as
//! `normalize`; the two definitions must stay textually identical, and the
//! `every_advertised_spelling_resolves_in_every_catalog` test in sismatic-core
//! catches any divergence.
//!
//! # Only for sismatic-core
//!
//! The generated code names `crate::protocol::payload_helpers::normalize` and
//! `crate::protocol::instructions::UnknownInstruction`. A proc macro has no
//! `$crate`, and `crate::` is correct only because every catalog lives inside
//! `sismatic-core`. Using `#[catalog]` from any other crate fails to resolve
//! those paths.
//!
//! The expansion adds only `impl` blocks (inherent, `FromStr`, `Display`), no
//! free items, so it can sit next to anything. Every path in it is absolute
//! (`::core::…`) and its locals are `mixed_site`, so nothing in scope at the
//! call site can capture or shadow them. Every generated `impl` carries
//! `#[allow(deprecated)]`, so a `#[deprecated]` variant warns its users and not
//! the catalog itself.
//!
//! # Errors
//!
//! Every mistake in a declaration is reported in one compile, one message per
//! mistake, and the enum and its impls are still emitted so that code using
//! them does not fail a second time with unrelated errors. The cases and the
//! first line of each message (verified by hand; there is no compile-fail
//! harness in this workspace):
//!
//! | Mistake | Message |
//! |---|---|
//! | misspelled helper, `#[alais("X")]` | ``unknown catalog attribute `#[alais]`; did you mean `#[alias]`?`` |
//! | non-normalized spelling, `#[alias("start-rec")]` | `"start-rec" can never match: input is upper-cased and `-` becomes `_` before lookup. Write "START_REC"` |
//! | duplicate spelling across or within variants | ``"STOP" is already accepted by `Stop`; `Pause` would never be reached by it``, plus `"STOP" first used here` at the first spelling |
//! | variant without `///` | ``variant `Start` has no description. Add a `/// ...` line above it: its first paragraph becomes `description()` `` |
//! | variant with fields, `Start(u8)` | ``catalog variants carry no data: an instruction is addressed by its name alone. Remove the fields from `Start` `` |
//! | `#[catalog]` on a struct, fn, … | ``#[catalog]` goes on an enum: each variant is one instruction`` |
//!
//! A duplicate is the one mistake with two spans, and so with two messages: the
//! second spelling (the one that can never be reached) and the first.

use std::collections::HashMap;

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{
    Error, Expr, Fields, Ident, Item, Lit, LitStr, Meta, Token, parse_macro_input,
    punctuated::Punctuated, spanned::Spanned,
};

// ---------------------------------------------------------------- helpers

/// The rule `FromStr` applies to its input before lookup.
///
/// Compile-time copy of `sismatic_core::protocol::payload_helpers::normalize`,
/// which the generated `FromStr` calls at run time. Keep the two textually
/// identical; see the crate docs.
fn normalize(s: &str) -> String {
    s.to_ascii_uppercase().replace('-', "_")
}

/// The default canonical name: `RunningState` → `RUNNING_STATE`.
///
/// A `_` goes before every uppercase letter except the first, and never before
/// a digit, so `Stream1State` → `STREAM1_STATE`.
fn upper_snake(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 4);
    for (i, ch) in ident.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

/// `description()`: the first paragraph of the doc lines (already trimmed),
/// joined with single spaces. Leading blank lines are skipped, so a `/** … */`
/// comment that opens on its own line works too.
fn first_paragraph(lines: &[String]) -> String {
    lines
        .iter()
        .skip_while(|l| l.is_empty())
        .take_while(|l| !l.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Levenshtein distance, for "did you mean" suggestions.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = (diag + usize::from(ca != cb)).min(row[j] + 1).min(up + 1);
            diag = up;
        }
    }
    row[b.len()]
}

/// The helper attributes this macro consumes.
const HELPERS: [&str; 2] = ["name", "alias"];

/// Collect every error instead of stopping at the first one.
fn push(errors: &mut Option<Error>, e: Error) {
    match errors {
        Some(all) => all.combine(e),
        None => *errors = Some(e),
    }
}

/// One spelling and where it was written (for error spans).
struct Spelling {
    text: String,
    span: Span,
}

struct Entry {
    variant: Ident,
    /// A misspelled helper was reported on this variant, so its name and
    /// aliases are probably not what the author meant: skip follow-up checks.
    suspect: bool,
    name: Spelling,
    aliases: Vec<Spelling>,
    description: String,
}

// ---------------------------------------------------------------- the macro

/// Turn a fieldless enum into an instruction catalog. See the crate docs.
#[proc_macro_attribute]
pub fn catalog(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = proc_macro2::TokenStream::from(args);
    let mut item = parse_macro_input!(item as Item);
    let mut errors: Option<Error> = None;

    if !args.is_empty() {
        push(
            &mut errors,
            Error::new_spanned(&args, "`#[catalog]` takes no arguments"),
        );
    }

    let Item::Enum(e) = &mut item else {
        let at = match &item {
            Item::Struct(s) => s.struct_token.span,
            Item::Union(u) => u.union_token.span,
            Item::Fn(f) => f.sig.fn_token.span,
            other => other.span(),
        };
        push(
            &mut errors,
            Error::new(
                at,
                "`#[catalog]` goes on an enum: each variant is one instruction",
            ),
        );
        // Emit the item untouched next to the error, so its users do not fail
        // a second time on a missing type.
        let errors = errors.map(|e| e.to_compile_error());
        return quote! { #errors #item }.into();
    };

    // ---- read the declaration out of each variant, stripping our helpers
    let mut entries = Vec::new();
    for v in &mut e.variants {
        if !matches!(v.fields, Fields::Unit) {
            push(
                &mut errors,
                Error::new_spanned(
                    &v.fields,
                    format!(
                        "catalog variants carry no data: an instruction is addressed by its \
                         name alone. Remove the fields from `{}`",
                        v.ident
                    ),
                ),
            );
            // Reported once; emit a unit variant so later code stays consistent.
            v.fields = Fields::Unit;
        }

        let mut name: Option<Spelling> = None;
        let mut aliases = Vec::new();
        let mut doc_lines: Vec<String> = Vec::new();
        let mut keep = Vec::new();
        let mut suspect = false;
        for attr in std::mem::take(&mut v.attrs) {
            let path = attr.path();
            if path.is_ident("name") {
                match attr.parse_args::<LitStr>() {
                    Ok(lit) if name.is_some() => push(
                        &mut errors,
                        Error::new(
                            lit.span(),
                            format!(
                                "`{}` already has a name; a variant has exactly one canonical \
                                 name. Use `#[alias(...)]` for other spellings",
                                v.ident
                            ),
                        ),
                    ),
                    Ok(lit) => {
                        name = Some(Spelling {
                            text: lit.value(),
                            span: lit.span(),
                        });
                    }
                    Err(err) => push(&mut errors, err),
                }
            } else if path.is_ident("alias") {
                match attr.parse_args_with(Punctuated::<LitStr, Token![,]>::parse_terminated) {
                    Ok(list) => aliases.extend(list.iter().map(|l| Spelling {
                        text: l.value(),
                        span: l.span(),
                    })),
                    Err(err) => push(&mut errors, err),
                }
            } else {
                if path.is_ident("doc") {
                    if let Meta::NameValue(nv) = &attr.meta
                        && let Expr::Lit(l) = &nv.value
                        && let Lit::Str(s) = &l.lit
                    {
                        // `/** … */` arrives as one attribute holding newlines.
                        // `split`, not `lines`: a blank `///` is an empty string,
                        // and `"".lines()` yields nothing, which would erase the
                        // paragraph break that ends the description.
                        doc_lines.extend(s.value().split('\n').map(|l| l.trim().to_string()));
                    }
                } else if let Some(id) = path.get_ident() {
                    // A near-miss of one of ours is a typo, not a foreign
                    // attribute: rustc would only say "cannot find attribute".
                    let id = id.to_string();
                    if let Some(ours) = HELPERS.into_iter().find(|ours| distance(&id, ours) <= 2) {
                        push(
                            &mut errors,
                            Error::new_spanned(
                                path,
                                format!(
                                    "unknown catalog attribute `#[{id}]`; did you mean `#[{ours}]`?"
                                ),
                            ),
                        );
                        suspect = true;
                        continue; // reported: do not also leave it for rustc
                    }
                }
                keep.push(attr); // doc, cfg, deprecated, … stay on the variant
            }
        }
        v.attrs = keep;

        let description = first_paragraph(&doc_lines);
        if description.is_empty() {
            push(
                &mut errors,
                Error::new_spanned(
                    &v.ident,
                    format!(
                        "variant `{}` has no description. Add a `/// ...` line above it: its \
                         first paragraph becomes `description()`",
                        v.ident
                    ),
                ),
            );
        }
        let name = name.unwrap_or_else(|| Spelling {
            text: upper_snake(&v.ident.to_string()),
            span: v.ident.span(),
        });
        entries.push(Entry {
            variant: v.ident.clone(),
            suspect,
            name,
            aliases,
            description,
        });
    }

    // ---- the normalization contract, and no spelling used twice
    let mut seen: HashMap<String, (Ident, Span)> = HashMap::new();
    for entry in entries.iter().filter(|e| !e.suspect) {
        for sp in std::iter::once(&entry.name).chain(&entry.aliases) {
            let fixed = normalize(&sp.text);
            if fixed != sp.text {
                push(
                    &mut errors,
                    Error::new(
                        sp.span,
                        format!(
                            "\"{}\" can never match: input is upper-cased and `-` becomes `_` \
                             before lookup. Write \"{fixed}\"",
                            sp.text
                        ),
                    ),
                );
            }
            if let Some((owner, first)) = seen.get(&fixed) {
                push(
                    &mut errors,
                    Error::new(
                        sp.span,
                        format!(
                            "\"{fixed}\" is already accepted by `{owner}`; `{}` would never be \
                             reached by it",
                            entry.variant
                        ),
                    ),
                );
                push(
                    &mut errors,
                    Error::new(*first, format!("\"{fixed}\" first used here")),
                );
            } else {
                seen.insert(fixed, (entry.variant.clone(), sp.span));
            }
        }
    }

    // ---- generate. Emitted even when there are errors, so that code which
    //      uses the enum does not fail a second time with unrelated messages.
    // Owned copies: `item` (which `e` borrows mutably) is quoted whole below.
    let ty = e.ident.clone();
    let generics = e.generics.clone();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let vars: Vec<_> = entries.iter().map(|x| x.variant.clone()).collect();
    let names: Vec<_> = entries.iter().map(|x| x.name.text.as_str()).collect();
    let docs: Vec<_> = entries.iter().map(|x| x.description.as_str()).collect();
    let aliases: Vec<Vec<&str>> = entries
        .iter()
        .map(|x| x.aliases.iter().map(|a| a.text.as_str()).collect())
        .collect();
    // Locals the call site can never see or shadow: mixed-site hygiene.
    let input = Ident::new("input", Span::mixed_site());
    let f = Ident::new("f", Span::mixed_site());

    let errors = errors.map(|e| e.to_compile_error());
    quote! {
        #errors
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy,
                 ::core::cmp::PartialEq, ::core::cmp::Eq)]
        #item

        #[allow(deprecated)]
        impl #impl_generics #ty #ty_generics #where_clause {
            /// Every built-in variant, in catalog order.
            pub const ALL: &'static [Self] = &[#(Self::#vars),*];

            /// The canonical uppercase name (e.g. `RUNNING_STATE`).
            pub fn name(self) -> &'static str {
                match self { #(Self::#vars => #names),* }
            }

            /// A one-line, human-readable description of this instruction.
            pub fn description(self) -> &'static str {
                match self { #(Self::#vars => #docs),* }
            }

            /// Every spelling `FromStr` accepts for this variant (canonical name
            /// first, then aliases), in normalized `UPPER_SNAKE` form. Powers
            /// documentation and type-stub generation.
            pub fn accepted(self) -> &'static [&'static str] {
                match self { #(Self::#vars => &[#names #(, #aliases)*]),* }
            }
        }

        #[allow(deprecated)]
        impl #impl_generics ::core::str::FromStr for #ty #ty_generics #where_clause {
            type Err = crate::protocol::instructions::UnknownInstruction;
            fn from_str(#input: &str) -> ::core::result::Result<Self, Self::Err> {
                match crate::protocol::payload_helpers::normalize(#input).as_str() {
                    #( #names #(| #aliases)* => ::core::result::Result::Ok(Self::#vars), )*
                    _ => ::core::result::Result::Err(
                        crate::protocol::instructions::UnknownInstruction(
                            ::std::string::ToString::to_string(#input),
                        ),
                    ),
                }
            }
        }

        #[allow(deprecated)]
        impl #impl_generics ::core::fmt::Display for #ty #ty_generics #where_clause {
            fn fmt(&self, #f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                #f.write_str(self.name())
            }
        }
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_upper_cases_and_maps_dash() {
        assert_eq!(normalize("running-state"), "RUNNING_STATE");
        assert_eq!(normalize("Stream_1-Name"), "STREAM_1_NAME");
        assert_eq!(normalize("ALREADY_OK"), "ALREADY_OK");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn upper_snake_splits_on_capitals_not_digits() {
        assert_eq!(upper_snake("RunningState"), "RUNNING_STATE");
        assert_eq!(upper_snake("Start"), "START");
        assert_eq!(upper_snake("Stream1State"), "STREAM1_STATE");
        assert_eq!(upper_snake("SshPort"), "SSH_PORT");
        assert_eq!(upper_snake("HTTPPort"), "H_T_T_P_PORT");
    }

    #[test]
    fn upper_snake_output_is_normalized() {
        for id in ["RunningState", "Stream1State", "RTMPStream1PublishURL", "X"] {
            let s = upper_snake(id);
            assert_eq!(normalize(&s), s);
        }
    }

    fn para(lines: &[&str]) -> String {
        first_paragraph(&lines.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn first_paragraph_stops_at_blank_line() {
        assert_eq!(para(&["Start recording."]), "Start recording.");
        assert_eq!(
            para(&["Stop", "recording.", "", "Not part of it."]),
            "Stop recording."
        );
        // `/** ... */` opening on its own line.
        assert_eq!(para(&["", "Block", "doc.", ""]), "Block doc.");
        assert_eq!(para(&[]), "");
        assert_eq!(para(&["", ""]), "");
    }

    #[test]
    fn distance_is_levenshtein() {
        assert_eq!(distance("", ""), 0);
        assert_eq!(distance("alias", "alias"), 0);
        assert_eq!(distance("alais", "alias"), 2); // transposition = 2 edits
        assert_eq!(distance("nam", "name"), 1);
        assert_eq!(distance("names", "name"), 1);
        assert_eq!(distance("aliases", "alias"), 2);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
    }

    /// Attributes found on real catalogs must never be mistaken for typos.
    #[test]
    fn common_attributes_are_not_near_misses() {
        for foreign in [
            "doc",
            "cfg",
            "allow",
            "deprecated",
            "expect",
            "serde",
            "default",
        ] {
            for ours in HELPERS {
                assert!(distance(foreign, ours) > 2, "{foreign} vs {ours}");
            }
        }
    }
}
