pub type BetaCount = usize;

pub const BETA_LIMIT: usize = 3;

pub static BETA_NAME: &str = "beta";

mod inner;

macro_rules! beta_macro {
    () => {};
}
