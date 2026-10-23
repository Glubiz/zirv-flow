//! What a mail server's refusal means (architecture 10.2, OQ-12). Bounce handling is synchronous
//! only: a permanent failure of the recipient at `RCPT` or `DATA` is a hard bounce, which the send
//! pipeline records and suppresses. A permanent failure for any other reason, a temporary failure
//! and a refusal at any earlier step are not, so a misconfigured relay or a rejected message can
//! never suppress a person.

use super::super::error::{DestinationError, Refusal};
use super::client::{Enhanced, Reply};

/// The step of the conversation a reply answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stage {
    Greeting,
    Ehlo,
    StartTls,
    Auth,
    MailFrom,
    RcptTo,
    Data,
    /// The end of the message: the server has seen it all.
    DataEnd,
}

/// What a reply that is not the expected success means at `stage`.
pub(super) fn refusal(stage: Stage, reply: &Reply) -> DestinationError {
    match reply.code {
        400..=499 => DestinationError::Refused(Refusal::Deferred),
        500..=599 if stage == Stage::Auth => DestinationError::Auth,
        500..=599 if recipient_failure(stage, reply.code, reply.enhanced) => {
            DestinationError::Refused(Refusal::HardBounce)
        }
        500..=599 => DestinationError::Refused(Refusal::Rejected),
        // A success or a continuation where none was expected: the server is not speaking the
        // conversation this client holds.
        _ => DestinationError::Other("the mail server sent an unexpected reply".to_owned()),
    }
}

/// Whether a permanent failure at `stage` says the recipient cannot receive mail.
///
/// With an enhanced status code the subject decides: `5.1.x` is an address problem, except
/// `5.1.7` and `5.1.8`, which name the sender. Anything else (`5.2.x` a full mailbox, `5.3.x`
/// the system, `5.7.x` policy) leaves the person alone. Without one, only the replies that
/// every server uses for an unknown or unacceptable mailbox at `RCPT` count: 550, 551 and 553.
fn recipient_failure(stage: Stage, code: u16, enhanced: Option<Enhanced>) -> bool {
    if !matches!(stage, Stage::RcptTo | Stage::Data | Stage::DataEnd) {
        return false;
    }
    match enhanced {
        Some(enhanced) => {
            enhanced.class == 5 && enhanced.subject == 1 && !matches!(enhanced.detail, 7 | 8)
        }
        None => stage == Stage::RcptTo && matches!(code, 550 | 551 | 553),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::error::Disposition;
    use super::*;

    fn reply(code: u16, enhanced: Option<(u8, u16, u16)>) -> Reply {
        Reply {
            code,
            enhanced: enhanced.map(|(class, subject, detail)| Enhanced {
                class,
                subject,
                detail,
            }),
            lines: Vec::new(),
        }
    }

    #[test]
    fn the_classification_table() {
        use Refusal::{Deferred, HardBounce, Rejected};
        use Stage::{Auth, Data, DataEnd, Ehlo, Greeting, MailFrom, RcptTo, StartTls};
        type Row = (Stage, u16, Option<(u8, u16, u16)>, DestinationError);
        let table: [Row; 22] = [
            // The recipient does not exist: hard bounce.
            (
                RcptTo,
                550,
                Some((5, 1, 1)),
                DestinationError::Refused(HardBounce),
            ),
            (RcptTo, 550, None, DestinationError::Refused(HardBounce)),
            (RcptTo, 553, None, DestinationError::Refused(HardBounce)),
            (RcptTo, 551, None, DestinationError::Refused(HardBounce)),
            (
                RcptTo,
                550,
                Some((5, 1, 2)),
                DestinationError::Refused(HardBounce),
            ),
            (
                Data,
                550,
                Some((5, 1, 1)),
                DestinationError::Refused(HardBounce),
            ),
            (
                DataEnd,
                550,
                Some((5, 1, 1)),
                DestinationError::Refused(HardBounce),
            ),
            // Not about the recipient: fails, no suppression.
            (
                RcptTo,
                554,
                Some((5, 7, 1)),
                DestinationError::Refused(Rejected),
            ),
            (
                RcptTo,
                550,
                Some((5, 7, 1)),
                DestinationError::Refused(Rejected),
            ),
            (
                RcptTo,
                552,
                Some((5, 2, 2)),
                DestinationError::Refused(Rejected),
            ),
            (RcptTo, 554, None, DestinationError::Refused(Rejected)),
            (Data, 554, None, DestinationError::Refused(Rejected)),
            (
                DataEnd,
                552,
                Some((5, 3, 4)),
                DestinationError::Refused(Rejected),
            ),
            (
                MailFrom,
                550,
                Some((5, 1, 7)),
                DestinationError::Refused(Rejected),
            ),
            (
                MailFrom,
                550,
                Some((5, 1, 1)),
                DestinationError::Refused(Rejected),
            ),
            (Greeting, 554, None, DestinationError::Refused(Rejected)),
            (Ehlo, 550, None, DestinationError::Refused(Rejected)),
            (StartTls, 454, None, DestinationError::Refused(Deferred)),
            // Authentication.
            (Auth, 535, Some((5, 7, 8)), DestinationError::Auth),
            (Auth, 535, None, DestinationError::Auth),
            // Temporary.
            (
                RcptTo,
                451,
                Some((4, 7, 1)),
                DestinationError::Refused(Deferred),
            ),
            (Greeting, 421, None, DestinationError::Refused(Deferred)),
        ];
        for (stage, code, enhanced, expected) in table {
            assert_eq!(
                refusal(stage, &reply(code, enhanced)),
                expected,
                "{stage:?} {code} {enhanced:?}"
            );
        }
    }

    #[test]
    fn only_a_hard_bounce_and_an_auth_failure_are_terminal_and_a_deferral_retries() {
        assert_eq!(
            refusal(Stage::RcptTo, &reply(451, None)).disposition(),
            Disposition::Retryable
        );
        assert_eq!(
            refusal(Stage::RcptTo, &reply(550, Some((5, 1, 1)))).disposition(),
            Disposition::Terminal
        );
        assert_eq!(
            refusal(Stage::MailFrom, &reply(554, None)).disposition(),
            Disposition::Terminal
        );
    }

    #[test]
    fn a_reply_that_should_not_be_there_is_not_a_refusal_of_the_message() {
        let error = refusal(Stage::Data, &reply(250, None));

        assert!(matches!(error, DestinationError::Other(_)));
    }
}
