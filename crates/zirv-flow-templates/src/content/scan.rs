//! The plain-text scan for element names that `mj-include` and `mj-*` refusals share.

/// The local name (the text after the last `:`) of the element every `<` in `bytes` opens, for
/// each `<` in order. With `skip_slash` a `/` right after the `<` is left out first, so a closing
/// tag reads like an opening one. A name ends at whitespace, `/` or `>`.
///
/// Every `<` in one run before a delimiter shares the end of its name and its last `:`, so both
/// are found once per run and the walk is linear however many `<` a document holds.
pub(crate) fn local_names(bytes: &[u8], skip_slash: bool) -> impl Iterator<Item = &[u8]> {
    let mut at = 0;
    let mut run_end = 0;
    let mut last_colon = None;
    std::iter::from_fn(move || {
        let open = at + bytes.get(at..)?.iter().position(|b| *b == b'<')?;
        at = open + 1;
        let mut start = open + 1;
        if skip_slash && bytes.get(start) == Some(&b'/') {
            start += 1;
        }
        // `run_end` is only valid for a start inside the run it was found for.
        if start >= run_end {
            let rest = &bytes[start..];
            let end = rest
                .iter()
                .position(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'>'))
                .unwrap_or(rest.len());
            run_end = start + end;
            last_colon = rest[..end]
                .iter()
                .rposition(|b| *b == b':')
                .map(|n| start + n);
        }
        let local = match last_colon {
            Some(colon) if colon >= start => colon + 1,
            _ => start,
        };
        Some(&bytes[local..run_end])
    })
}
