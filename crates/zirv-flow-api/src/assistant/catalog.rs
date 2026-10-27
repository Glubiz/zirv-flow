//! Bounded, untrusted names from workspace catalogs and external schemas.

use zirv_flow_ai::Content;

pub(crate) const MAX_NAME_CHARS: usize = 200;

pub(crate) fn name(value: &str) -> String {
    value
        .replace("<untrusted_data>", "")
        .replace("</untrusted_data>", "")
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '<' | '>'))
        .take(MAX_NAME_CHARS)
        .collect()
}

/// Headings and omission counts are product text; every rendered entry is data.
pub(crate) fn names<T>(
    parts: &mut Vec<Content>,
    title: &str,
    entries: impl IntoIterator<Item = T>,
    cap: usize,
    render: impl Fn(T) -> String,
) {
    parts.push(Content::Schema(format!("{title}:\n")));
    let mut text = String::new();
    let mut total = 0;
    for (index, entry) in entries.into_iter().enumerate() {
        if index < cap {
            text.push_str(&render(entry));
            text.push('\n');
        }
        total = index + 1;
    }
    if total == 0 {
        parts.push(Content::Schema("(none)\n".to_owned()));
    } else {
        parts.push(Content::Untrusted(text));
    }
    if total > cap {
        parts.push(Content::Schema(format!("{} more omitted.\n", total - cap)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_filter_controls_and_fences_and_bound_unicode_length() {
        let original = format!(
            "<untrusted_data>A\n\r\t\0</untrusted_data>{}",
            "ø".repeat(300)
        );
        let filtered = name(&original);
        assert_eq!(filtered, format!("A{}", "ø".repeat(MAX_NAME_CHARS - 1)));
        assert!(original.contains("</untrusted_data>"));
    }

    #[test]
    fn names_cap_count_and_keep_only_structure_trusted() {
        let mut parts = Vec::new();
        names(&mut parts, "Names", ["first", "second", "third"], 1, name);
        assert_eq!(
            parts,
            vec![
                Content::Schema("Names:\n".into()),
                Content::Untrusted("first\n".into()),
                Content::Schema("2 more omitted.\n".into()),
            ]
        );
    }
}
