use zebra_chain::amount::{self, Amount, Constraint, COIN};

/// A ZEC value read from JSON that is not a whole number of zatoshis or is out of range.
#[derive(Debug, thiserror::Error)]
pub enum ZecParseError {
    /// The value had a fractional number of zatoshis.
    #[error("loss of precision parsing ZEC value: floating point had fractional zatoshis")]
    FractionalZatoshis,
    /// The value was outside the amount's range.
    #[error(transparent)]
    OutOfRange(#[from] amount::Error),
}

/// An [`Amount`] that serializes as a double-precision ZEC value, accurate to the zatoshi for the small calculations this interface makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "f64")]
#[serde(into = "f64")]
#[serde(bound = "C: Constraint + Clone")]
pub struct Zec<C: Constraint>(Amount<C>);

impl<C: Constraint> Zec<C> {
    /// Returns the lossy `f64` ZEC value, which consensus-critical code must never use.
    pub fn lossy_zec(&self) -> f64 {
        // Exact: f64 has 53 bits of precision, MAX_MONEY needs fewer than 51 and COIN fewer than 27.
        let zats = self.0.zatoshis() as f64;
        let coin = COIN as f64;
        zats / coin
    }

    /// Converts a lossy `f64` ZEC value back to an amount, refusing fractional zatoshis.
    pub fn from_lossy_zec(lossy_zec: f64) -> Result<Self, ZecParseError> {
        let zats = lossy_zec * COIN as f64;
        if zats != zats.trunc() {
            return Err(ZecParseError::FractionalZatoshis);
        }
        Ok(Self(Amount::try_from(zats as i64)?))
    }
}

impl<C: Constraint> From<Zec<C>> for f64 {
    fn from(zec: Zec<C>) -> f64 {
        zec.lossy_zec()
    }
}

impl<C: Constraint> TryFrom<f64> for Zec<C> {
    type Error = ZecParseError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::from_lossy_zec(value)
    }
}

impl<C: Constraint> From<Amount<C>> for Zec<C> {
    fn from(amount: Amount<C>) -> Self {
        Self(amount)
    }
}

impl<C: Constraint> From<Zec<C>> for Amount<C> {
    fn from(zec: Zec<C>) -> Amount<C> {
        zec.0
    }
}
