//! Static lists (architecture sections 6 and 7.3): named sets of people, their memberships and
//! the history of every membership change.

mod crud;
mod members;

pub use crud::{List, create_list, delete_list, find_list, list_lists, update_list};
pub use members::{
    ListCounts, ListMember, ListSource, ListWrite, MAX_PEOPLE_PER_CALL, MemberIdentifier,
    MembershipChange, MembershipError, MembershipOutcome, PersonList, add_members,
    add_members_into, lists_for, members, membership_history, remove_members,
};
pub(crate) use members::{move_on_merge, remove_members_into};
