//! Every job kind this build ships: one module per kind and one line in [`ALL`].

use crate::ErasedKind;

mod noop;

pub(crate) static ALL: &[&dyn ErasedKind] = &[&noop::Noop];
