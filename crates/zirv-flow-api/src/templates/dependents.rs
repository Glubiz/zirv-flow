//! The publish check of a snippet or theme version against the live templates that render with it
//! (architecture 11.4): each such template's current published version is validated as template
//! publish validates it, with the candidate version in place of the published one. A version that
//! would make a blocking issue of a template is refused before it is published, because a send of
//! that template would fail at render time.

use zirv_flow_core::WorkspaceId;
use zirv_flow_render::RenderPool;
use zirv_flow_store::{Candidate, LiveTemplate};
use zirv_flow_templates::{Issue, content_type_of};

use super::inputs::{Subject, resolve_with};
use super::publish::publish_issues;
use super::{field_catalog, stored_document};
use crate::AppState;
use crate::problem::{ApiError, BrokenTemplateProblem};

/// The most templates one publish validates, in template id order. Past it the publish goes ahead
/// on the result for these, so a large workspace cannot stall a publish.
pub(crate) const MAX_CHECKED: usize = 200;

/// The templates among `live` that `candidate` would stop from sending, each with the codes of its
/// blocking issues. Warnings never count. Each template is validated on its own through the render
/// pool, holding no connection while the render host works.
pub(crate) async fn broken_by(
    state: &AppState,
    render: &RenderPool,
    workspace: WorkspaceId,
    live: Vec<LiveTemplate>,
    candidate: &Candidate,
) -> Result<Vec<BrokenTemplateProblem>, ApiError> {
    let mut broken = Vec::new();
    if live.is_empty() {
        return Ok(broken);
    }
    let catalog = {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, workspace).await?,
        )
    };
    for template in live.into_iter().take(MAX_CHECKED) {
        let (doc, inputs) = {
            let mut connection = state
                .pool
                .acquire()
                .await
                .map_err(|_| ApiError::internal())?;
            let Some(version) =
                zirv_flow_store::find_template(&mut *connection, workspace, template.id)
                    .await?
                    .and_then(|found| found.current_version)
            else {
                continue;
            };
            let Some(version) = zirv_flow_store::find_template_version(
                &mut *connection,
                workspace,
                template.id,
                version,
            )
            .await?
            else {
                continue;
            };
            // A version that no longer parses, or whose content type is unsupported, cannot send
            // anyway, so it is no reason to refuse this publish.
            let Ok(doc) = stored_document(&version.document) else {
                continue;
            };
            if content_type_of(&doc).is_err() {
                continue;
            }
            let now = state.clock.now(&mut connection).await?;
            let inputs = resolve_with(
                &mut connection,
                workspace,
                &doc,
                Subject::Nobody,
                state.public_url(),
                now,
                Some(candidate),
            )
            .await?;
            (doc, inputs)
        };
        let issues = publish_issues(render, &doc, &catalog, &inputs).await?;
        let codes = blocking_codes(&issues);
        if !codes.is_empty() {
            broken.push(BrokenTemplateProblem {
                id: template.id,
                name: template.name,
                codes,
            });
        }
    }
    Ok(broken)
}

/// The codes of the blocking issues, in order; a warning is not one.
fn blocking_codes(issues: &[Issue]) -> Vec<String> {
    issues
        .iter()
        .filter(|issue| issue.is_blocking())
        .map(|issue| issue.code.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use zirv_flow_templates::code;

    use super::*;

    #[test]
    fn only_blocking_issues_count_and_a_warning_never_refuses() {
        let issues = [
            Issue::new(code::IMAGE_WITHOUT_ALT, "/blocks/block/i"),
            Issue::new(code::MISSING_UNSUBSCRIBE, "/blocks"),
        ];

        assert_eq!(blocking_codes(&issues), ["missing_unsubscribe"]);
        assert!(blocking_codes(&issues[..1]).is_empty());
    }

    #[test]
    fn the_bound_is_two_hundred_templates() {
        assert_eq!(MAX_CHECKED, 200);
    }
}
