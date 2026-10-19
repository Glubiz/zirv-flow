//! The lifecycle of a flow: which action is allowed from which state, and where it leads.

use std::fmt;

/// Where a flow is in its life.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FlowState {
    /// Never published.
    Draft,
    /// Live: new people enter by the current version.
    Published,
    /// No new enrollments; the current version stays.
    Paused,
    /// Final. Nothing changes any more.
    Archived,
}

/// What an operator can do to a flow's lifecycle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FlowAction {
    Publish,
    Pause,
    Resume,
    Archive,
}

impl FlowState {
    pub const ALL: [Self; 4] = [Self::Draft, Self::Published, Self::Paused, Self::Archived];

    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Published => "published",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == text)
    }

    /// The state after `action`, or `None` when the action is not allowed here.
    ///
    /// A publish makes a first version live, and a later one replaces the current version
    /// without changing the state, so a paused flow stays paused until it is resumed.
    pub const fn after(self, action: FlowAction) -> Option<Self> {
        match (self, action) {
            (Self::Draft, FlowAction::Publish) => Some(Self::Published),
            (Self::Published | Self::Paused, FlowAction::Publish) => Some(self),
            (Self::Published, FlowAction::Pause) => Some(Self::Paused),
            (Self::Paused, FlowAction::Resume) => Some(Self::Published),
            (Self::Draft | Self::Published | Self::Paused, FlowAction::Archive) => {
                Some(Self::Archived)
            }
            _ => None,
        }
    }
}

impl fmt::Display for FlowState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FlowAction {
    pub const ALL: [Self; 4] = [Self::Publish, Self::Pause, Self::Resume, Self::Archive];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Archive => "archive",
        }
    }

    /// The past participle, for a message such as "a draft flow cannot be paused".
    pub const fn past(self) -> &'static str {
        match self {
            Self::Publish => "published",
            Self::Pause => "paused",
            Self::Resume => "resumed",
            Self::Archive => "archived",
        }
    }
}

impl fmt::Display for FlowAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_of_a_state_round_trips() {
        for state in FlowState::ALL {
            assert_eq!(FlowState::parse(state.as_str()), Some(state));
        }
        assert_eq!(FlowState::parse("live"), None);
    }

    #[test]
    fn every_allowed_transition_and_no_other() {
        use FlowAction::{Archive, Pause, Publish, Resume};
        use FlowState::{Archived, Draft, Paused, Published};
        let allowed = [
            (Draft, Publish, Published),
            (Published, Publish, Published),
            (Paused, Publish, Paused),
            (Published, Pause, Paused),
            (Paused, Resume, Published),
            (Draft, Archive, Archived),
            (Published, Archive, Archived),
            (Paused, Archive, Archived),
        ];
        for state in FlowState::ALL {
            for action in FlowAction::ALL {
                let expected = allowed
                    .iter()
                    .find(|(from, by, _)| *from == state && *by == action)
                    .map(|(_, _, to)| *to);
                assert_eq!(state.after(action), expected, "{state} {action}");
            }
        }
    }

    #[test]
    fn an_archived_flow_accepts_no_action() {
        for action in FlowAction::ALL {
            assert_eq!(FlowState::Archived.after(action), None, "{action}");
        }
    }

    #[test]
    fn only_a_published_flow_pauses_and_only_a_paused_one_resumes() {
        for state in FlowState::ALL {
            assert_eq!(
                state.after(FlowAction::Pause).is_some(),
                state == FlowState::Published
            );
            assert_eq!(
                state.after(FlowAction::Resume).is_some(),
                state == FlowState::Paused
            );
        }
    }
}
