//! One declarative table per instruction enum.
//!
//! Each of [`Query`](super::query::Query), [`Command`](super::commands::Command),
//! and [`Register`](super::register::Register) declares its names in a single
//! place via [`instruction_catalog!`]. The macro derives everything that must
//! stay in lockstep — the `ALL` catalog, the canonical `name()`, the accepted
//! input spellings, `FromStr`, and `Display` — so the parser and the generated
//! documentation can never disagree about which strings are valid.
//!
//! Wire encoding (payloads, parsers, register indices, command verbs) is a
//! separate concern and stays in each enum's own module.

/// Declare an instruction enum together with its name catalog.
///
/// Per variant you give the canonical wire name, any additional accepted
/// aliases, and a one-line description. **All name literals must be written in
/// normalized form** (ASCII uppercase, `-` written as `_`), because `FromStr`
/// matches them against [`normalize`](crate::protocol::payload_helpers::normalize)d
/// input. That makes lookups case-insensitive and `-`/`_`-insensitive for free.
macro_rules! instruction_catalog {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $Enum:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident {
                    name: $canon:literal,
                    aliases: [ $($alias:literal),* $(,)? ],
                    doc: $doc:literal $(,)?
                }
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis enum $Enum {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
        }

        impl $Enum {
            /// Every built-in variant, in catalog order.
            pub const ALL: &'static [$Enum] = &[$($Enum::$variant),+];

            /// The canonical uppercase name (e.g. `RUNNING_STATE`).
            pub fn name(self) -> &'static str {
                match self { $($Enum::$variant => $canon),+ }
            }

            /// A one-line, human-readable description of this instruction.
            pub fn description(self) -> &'static str {
                match self { $($Enum::$variant => $doc),+ }
            }

            /// Every spelling `FromStr` accepts for this variant (canonical name
            /// first, then aliases), in normalized `UPPER_SNAKE` form. Powers
            /// documentation and type-stub generation.
            pub fn accepted(self) -> &'static [&'static str] {
                match self { $($Enum::$variant => &[$canon $(, $alias)*]),+ }
            }
        }

        impl ::std::str::FromStr for $Enum {
            type Err = $crate::protocol::instructions::UnknownInstruction;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                match $crate::protocol::payload_helpers::normalize(s).as_str() {
                    $( $canon $(| $alias)* => ::core::result::Result::Ok($Enum::$variant), )+
                    _ => ::core::result::Result::Err(
                        $crate::protocol::instructions::UnknownInstruction(s.to_string())
                    ),
                }
            }
        }

        impl ::core::fmt::Display for $Enum {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.name())
            }
        }
    };
}

pub(crate) use instruction_catalog;
