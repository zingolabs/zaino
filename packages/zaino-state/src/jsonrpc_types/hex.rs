/// Serializes and deserializes an `Option<T: ToHex>` as a hex string, or null when absent.
pub mod opthex {
    use hex::{FromHex, ToHex};
    use serde::{de, Deserialize, Deserializer, Serializer};

    /// Writes `data` as a hex string, or null when it is absent.
    pub fn serialize<S, T>(data: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: ToHex,
    {
        match data {
            Some(data) => {
                let s = data.encode_hex::<String>();
                serializer.serialize_str(&s)
            }
            None => serializer.serialize_none(),
        }
    }

    /// Reads a hex string into `T`, or null into `None`.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromHex,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        match opt {
            Some(s) => T::from_hex(&s)
                .map(Some)
                .map_err(|_e| de::Error::custom("failed to convert hex string")),
            None => Ok(None),
        }
    }
}

/// Serializes and deserializes a `[u8; N]` as a hex string.
pub mod arrayhex {
    use serde::{Deserializer, Serializer};
    use std::fmt;

    /// Writes `data` as a hex string.
    pub fn serialize<S, const N: usize>(data: &[u8; N], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_string = hex::encode(data);
        serializer.serialize_str(&hex_string)
    }

    /// Reads a hex string of exactly `N` bytes.
    pub fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HexArrayVisitor<const N: usize>;

        impl<const N: usize> serde::de::Visitor<'_> for HexArrayVisitor<N> {
            type Value = [u8; N];

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "a hex string representing exactly {N} bytes")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let vec = hex::decode(v).map_err(E::custom)?;
                vec.clone().try_into().map_err(|_| {
                    E::invalid_length(vec.len(), &format!("expected {N} bytes").as_str())
                })
            }
        }

        deserializer.deserialize_str(HexArrayVisitor::<N>)
    }
}
