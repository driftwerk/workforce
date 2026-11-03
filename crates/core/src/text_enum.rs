/// An enum kept as text: `as_str` and `parse` by the given words, serde in
/// lowercase variant names (the desk's stored enums share it).
#[macro_export]
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn parse(s: &str) -> eyre::Result<Self> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(eyre::eyre!(concat!("unknown ", stringify!($name), " {:?}"), other)),
                }
            }
        }
    };
}
