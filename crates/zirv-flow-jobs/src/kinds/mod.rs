//! Every job kind this build ships: one module per kind and one line in [`ALL`].

use crate::ErasedKind;

mod noop;
mod person_merge_rewrite;
mod traits_recompute;

pub(crate) static ALL: &[&dyn ErasedKind] = &[
    &noop::Noop,
    &person_merge_rewrite::PersonMergeRewrite,
    &traits_recompute::TraitsRecompute,
];
