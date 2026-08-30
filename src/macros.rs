//! Macros used by the generated code.

/// Defines an OCPP string enumeration.
///
/// OCPP enumerations are open in practice: field devices ship values that are not in the
/// schema, and OCA adds values in errata. The generated type therefore always parses, and
/// carries an `UnknownValue` variant that [`Validate`](crate::validate::Validate) reports —
/// so the *decoder policy*, not the parser, decides whether an unknown value is fatal.
#[macro_export]
#[doc(hidden)]
macro_rules! ocpp_enum {
    (
        $(#[$attr:meta])*
        $name:ident {
            $( $(#[$vattr:meta])* $variant:ident = $wire:literal ),* $(,)?
        }
    ) => {
        $(#[$attr])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( $(#[$vattr])* $variant, )*
            /// A value that this version of OCPP does not define.
            ///
            /// Produced instead of failing, so a peer's forward-compatible extension does not
            /// take down the connection. Reported by
            /// [`Validate`](crate::validate::Validate) as
            /// [`ViolationKind::UnknownEnumValue`](crate::validate::ViolationKind::UnknownEnumValue),
            /// which the decoder turns into a `PropertyConstraintViolation` unless
            /// [`UnknownEnumValues::Preserve`](crate::decode::UnknownEnumValues::Preserve) is set.
            ///
            /// Named `UnknownValue` because `Unknown` is itself a defined value of several
            /// OCPP enumerations.
            UnknownValue(::alloc::string::String),
        }

        impl $name {
            /// Every value defined by the specification, in schema order.
            pub const VARIANTS: &'static [&'static str] = &[ $($wire),* ];

            /// The value as it appears on the wire.
            #[must_use]
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )*
                    Self::UnknownValue(value) => value.as_str(),
                }
            }

            /// Parses a wire value, keeping unrecognised input in the `UnknownValue` variant.
            #[must_use]
            pub fn from_wire(value: &str) -> Self {
                match value {
                    $( $wire => Self::$variant, )*
                    other => Self::UnknownValue(::alloc::string::ToString::to_string(other)),
                }
            }

            /// Whether this value is defined by the specification.
            #[must_use]
            pub const fn is_known(&self) -> bool {
                !matches!(self, Self::UnknownValue(_))
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::message::UnknownVariant;

            /// Strict parsing: unlike [`Self::from_wire`], an unrecognised value is an error.
            fn from_str(value: &str) -> ::core::result::Result<Self, Self::Err> {
                match Self::from_wire(value) {
                    Self::UnknownValue(_) => ::core::result::Result::Err($crate::message::UnknownVariant {
                        type_name: stringify!($name),
                    }),
                    known => ::core::result::Result::Ok(known),
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, ser: S) -> ::core::result::Result<S::Ok, S::Error> {
                ser.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(de: D) -> ::core::result::Result<Self, D::Error> {
                struct Visitor;
                impl ::serde::de::Visitor<'_> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                        write!(f, "a {} string", stringify!($name))
                    }
                    fn visit_str<E: ::serde::de::Error>(self, value: &str) -> ::core::result::Result<$name, E> {
                        ::core::result::Result::Ok($name::from_wire(value))
                    }
                }
                de.deserialize_str(Visitor)
            }
        }

        impl $crate::validate::Validate for $name {
            fn validate_at(
                &self,
                path: &mut $crate::validate::ValidationPath,
                out: &mut $crate::validate::Violations,
            ) {
                if let Self::UnknownValue(value) = self {
                    $crate::validate::unknown_enum_value(value, stringify!($name), path, out);
                }
            }
        }
    };
}
