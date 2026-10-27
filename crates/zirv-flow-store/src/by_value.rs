//! The by-value erasure matching rule (#373), shared by the deletes that find a person's data
//! without a person column: import files and refused rows, rejects with no person, idempotency
//! keys.
//!
//! An identifier matches only as a whole field, ignoring case: a CSV cell, bounded by the start
//! or end of the text, a newline, a quote, or one of `,`, `;`, a tab and `|`. A needle holding
//! `@` (an email) also matches as a substring, because an address is distinctive and is the
//! common value embedded in a notes cell. A non-email identifier embedded in free text is not
//! found; that is the documented limit.

/// SQL for one regex over all needles in the `text[]` parameter `array` (for example `$2`),
/// used with `~*`. NULL when no needle is usable. With `encoded`, each needle is spelled as
/// `encode(bytea, 'escape')` spells it, to match a file read through that encoding; otherwise
/// it is matched as it is. Every needle is escaped for regex metacharacters first.
pub(crate) fn field_pattern(array: &str, encoded: bool) -> String {
    let spelled = if encoded {
        "encode(convert_to(x, 'UTF8'), 'escape')"
    } else {
        "x"
    };
    let escape =
        |value: &str| format!(r#"regexp_replace({value}, '([\\^$.|?*+()\[\]{{}}])', '\\\1', 'g')"#);
    let (plain, quoted) = (
        escape(spelled),
        escape(&format!("replace({spelled}, '\"', '\"\"')")),
    );
    // An identifier holding a delimiter, a quote or a newline would run across cells, so it
    // matches only as one correctly quoted cell (encoded file) or as a whole value (a parsed
    // cell). One made only of delimiters, quotes and whitespace matches nothing.
    let whole = if encoded {
        format!(r#"'(?:^|[\n\r,;\t|])"' || {quoted} || '"(?:$|[\n\r,;\t|])'"#)
    } else {
        format!("'^' || {plain} || '$'")
    };
    format!(
        r#"(SELECT string_agg(CASE
                WHEN position('@' IN x) > 0 THEN {plain}
                WHEN x ~ '[,;|"\t\r\n]' THEN {whole}
                ELSE '(?:^|[\n\r",;\t|])' || {plain} || '(?:$|[\n\r",;\t|])' END, '|')
          FROM unnest({array}::text[]) AS x WHERE x ~ '[^,;|"[:space:]]')"#
    )
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    async fn matches(pool: &PgPool, needles: &[&str], text: &str, encoded: bool) -> bool {
        let needles: Vec<String> = needles.iter().map(|n| (*n).to_owned()).collect();
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT $2::text ~* {}",
            field_pattern("$1", encoded)
        )))
        .bind(needles)
        .bind(text)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_needle_matches_only_as_a_whole_field(pool: PgPool) {
        for text in ["42", "x,42,y", "a;42", "\"42\"", "a\n42\n", "a\t42", "a|42"] {
            assert!(matches(&pool, &["42"], text, false).await, "{text:?}");
        }
        for text in ["1427", "x42y", "2026-10-27T11:42", "42a", "a42", "4 2"] {
            assert!(!matches(&pool, &["42"], text, false).await, "{text:?}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn matching_ignores_case_and_an_email_matches_inside_a_cell(pool: PgPool) {
        assert!(matches(&pool, &["Ext-A"], "ext-a", false).await);
        assert!(matches(&pool, &["ada@example.com"], "note: ADA@example.com!", false).await);
        assert!(!matches(&pool, &["ada"], "note: ada!", false).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn regex_metacharacters_in_a_needle_match_only_themselves(pool: PgPool) {
        for encoded in [false, true] {
            assert!(matches(&pool, &["a.b+c"], "x,a.b+c,y", encoded).await);
            assert!(!matches(&pool, &["a.b+c"], "x,aXbbc,y", encoded).await);
            assert!(!matches(&pool, &["a.b+c"], "x,a.bbc,y", encoded).await);
            assert!(matches(&pool, &["(x)[y]{2}^$"], "(x)[y]{2}^$", encoded).await);
            assert!(!matches(&pool, &["(x)[y]{2}"], "y", encoded).await);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_usable_needle_matches_nothing(pool: PgPool) {
        let none: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT 'anything' ~* {}",
            field_pattern("$1", false)
        )))
        .bind(vec![String::new()])
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(none, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_needle_with_a_delimiter_matches_only_as_one_quoted_cell(pool: PgPool) {
        for needle in ["a,b", "a;b", "a|b"] {
            let quoted = format!("x,\"{needle}\",y");
            assert!(matches(&pool, &[needle], &quoted, true).await, "{quoted}");
            assert!(matches(&pool, &[needle], &format!("\"{needle}\""), true).await);
            let split = needle.replace([',', ';', '|'], ",");
            assert!(!matches(&pool, &[needle], &split, true).await, "{split}");
            assert!(!matches(&pool, &[needle], &format!("x,{needle},y"), true).await);
            assert!(matches(&pool, &[needle], needle, false).await);
            assert!(!matches(&pool, &[needle], &format!("x,{needle}"), false).await);
        }
        assert!(!matches(&pool, &["a,b"], "a;b,\"a\",\"b\"", true).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_needle_with_a_quote_matches_only_its_doubled_quoted_cell(pool: PgPool) {
        assert!(matches(&pool, &["x\"y"], "1,\"x\"\"y\",2", true).await);
        assert!(!matches(&pool, &["x\"y"], "1,x\"y,2", true).await);
        assert!(!matches(&pool, &["x\"y"], "1,\"x\"y\",2", true).await);
        assert!(matches(&pool, &["x\"y"], "x\"y", false).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_needle_of_only_delimiters_quotes_and_whitespace_matches_nothing(pool: PgPool) {
        for needle in [",", ";;", "| |", "\"", " \t", ",\""] {
            for encoded in [false, true] {
                let hit: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT ',,;;' ~* {}",
                    field_pattern("$1", encoded)
                )))
                .bind(vec![needle.to_owned()])
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_ne!(hit, Some(true), "{needle:?}");
            }
        }
    }
}
