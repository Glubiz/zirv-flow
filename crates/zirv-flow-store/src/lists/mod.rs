//! Static lists (architecture sections 6 and 7.3): named sets of people, their memberships and
//! the history of every membership change.

mod crud;
mod members;

pub use crud::{List, create_list, delete_list, find_list, list_lists, update_list};
pub(crate) use members::move_on_merge;
pub use members::{
    ListMember, ListSource, MAX_PEOPLE_PER_CALL, MemberIdentifier, MembershipChange,
    MembershipError, MembershipOutcome, PersonList, add_members, lists_for, members,
    membership_history, remove_members,
};
