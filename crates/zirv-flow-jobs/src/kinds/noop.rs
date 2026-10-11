//! `maintenance.noop`: does nothing. It exercises the queue end to end.

use serde_json::Value;

use crate::{JobContext, JobError, JobKind};

pub(crate) struct Noop;

impl JobKind for Noop {
    const KIND: &'static str = "maintenance.noop";

    async fn run(&self, _ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_name_is_maintenance_noop() {
        assert_eq!(Noop::KIND, "maintenance.noop");
    }
}
