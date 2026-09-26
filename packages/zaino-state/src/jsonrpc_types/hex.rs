use serde::Serializer;

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

/// Writes each byte string in `items` as a lowercase hex string.
pub(super) fn hexvec<S, T>(items: &[T], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: AsRef<[u8]>,
{
    serializer.collect_seq(items.iter().map(hex::encode))
}
