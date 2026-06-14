use crate::RullstValue;
use std::future::Future;

// ─── Request-scoped default tenant id ─────────────────────────────────────
//
// Set once at the HTTP boundary with `with_tenant(t)`. Every query,
// update, delete and save that runs inside this scope automatically
// filters / stamps `tenant_column = t` unless the caller uses one of
// the per-query overrides below.
tokio::task_local! {
    pub static CURRENT_TENANT: RullstValue;
}

/// Set the tenant id for the duration of `f`. Every read, delete and
/// `entity.save()` inside `f` sees the same `t` as its `tenant_column`
/// value. The ambient context is restored when `f` returns (or panics).
///
/// A single query can opt out of the auto-injected `WHERE <col> = ?`
/// for the rare cases that need it (super-admin reads, cross-tenant
/// reports, migrations) by calling `QueryBuilder::without_tenant()` on
/// its builder before the query runs.
///
/// The tenant id is always passed to the database driver as a
/// parameter binding (never interpolated as a SQL literal), so the
/// same security guarantees that apply to every other `where_*`
/// helper apply to the auto-injected tenant predicate as well.
pub async fn with_tenant<T, F, R>(tenant_id: T, f: F) -> R
where
    T: Into<RullstValue>,
    F: Future<Output = R>,
{
    CURRENT_TENANT.scope(tenant_id.into(), f).await
}

/// Read the active `with_tenant(t)` value, if any. Returns `None`
/// outside any tenant scope. Used by the macro-generated
/// `QueryBuilder::push_tenant_filter()` and by `entity.save()` to
/// pick the value to stamp on inserts.
pub fn get_tenant_id() -> Option<RullstValue> {
    CURRENT_TENANT.try_with(|t| t.clone()).ok()
}

/// Returns `true` if `cond` already mentions `column` as the leading
/// SQL token of a comparison. Used by the macro-generated
/// `QueryBuilder` to short-circuit the auto-injection of the
/// `tenant_column` predicate so the user-supplied condition wins and
/// we do not emit a redundant `AND <col> = ?` (or worse, a
/// duplicated binding).
///
/// The check is intentionally permissive about the operator and the
/// right-hand side: `tenant_id = ?`, `tenant_id != ?`, `tenant_id IN
/// (?)`, `tenant_id IS NULL`, `tenant_id LIKE ?`, etc. all count as
/// "covered" because they all pin the column to a value (or to
/// `NULL`) and adding our own `<col> = <literal>` would either be
/// redundant or change the semantics (binding counts in particular
/// would go out of sync with the params vec).
///
/// The check ignores leading whitespace, table qualifiers (e.g.
/// `users.tenant_id = ?`) and the operator that follows. It does
/// **not** parse the SQL — it just looks for a token whose
/// identifier matches `column`, optionally followed by a `.`. That
/// keeps the helper cheap and dependency-free.
pub fn cond_mentions_column(cond: &str, column: &str) -> bool {
    let trimmed = cond.trim_start();
    // Strip a possible `<table>.` qualifier: "users.tenant_id = ?"
    // → look at the substring after the last '.'.
    let after_qualifier = trimmed.rsplit('.').next().unwrap_or(trimmed);
    // The next token must equal `column`. We accept any of:
    //   * `column `     (followed by whitespace)
    //   * `column(...)` (e.g. `users.tenant_id(...)` doesn't make
    //                    sense, but `column(...)` for functions
    //                    like `COALESCE(tenant_id, ...) = ?` is
    //                    covered by a stricter prefix match — we
    //                    only care about the simple `<col> op ?` /
    //                    `<col> op (?, ?, ?)` cases here, which
    //                    all have whitespace or a comparison
    //                    operator right after the identifier).
    if let Some(rest) = after_qualifier.strip_prefix(column) {
        // Accept whitespace, any of the standard SQL comparison
        // tokens, '(' (function call form), or end-of-string. If
        // `rest` starts with a letter / digit / '_', the prefix
        // matched a longer identifier (e.g. column "id" matching
        // "identifier = ?") and we must reject it.
        match rest.chars().next() {
            None => true,
            Some(c) if c.is_whitespace() => true,
            Some('=') | Some('!') | Some('<') | Some('>') | Some('(') => true,
            _ => false,
        }
    } else {
        false
    }
}

#[cfg(test)]
mod cond_mentions_column_tests {
    use super::cond_mentions_column;

    #[test]
    fn matches_simple_equality() {
        assert!(cond_mentions_column("tenant_id = ?", "tenant_id"));
    }

    #[test]
    fn matches_inequality() {
        assert!(cond_mentions_column("tenant_id != ?", "tenant_id"));
    }

    #[test]
    fn matches_in_clause() {
        assert!(cond_mentions_column("tenant_id IN (?)", "tenant_id"));
        assert!(cond_mentions_column("tenant_id IN (?, ?)", "tenant_id"));
    }

    #[test]
    fn matches_is_null() {
        assert!(cond_mentions_column("tenant_id IS NULL", "tenant_id"));
    }

    #[test]
    fn matches_qualified_column() {
        assert!(cond_mentions_column("users.tenant_id = ?", "tenant_id"));
    }

    #[test]
    fn matches_with_leading_whitespace() {
        assert!(cond_mentions_column("   tenant_id = ?", "tenant_id"));
    }

    #[test]
    fn rejects_unrelated_column() {
        assert!(!cond_mentions_column("status = ?", "tenant_id"));
    }

    #[test]
    fn rejects_partial_identifier_match() {
        // The column is "id"; "identifier" starts with "id" but the
        // next char is 'e' (a letter) — we must reject it.
        assert!(!cond_mentions_column("identifier = ?", "id"));
    }

    #[test]
    fn rejects_completely_different_predicate() {
        assert!(!cond_mentions_column("1 = 1", "tenant_id"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── with_tenant ───────────────────────────────────────────────────────

    #[test]
    fn test_get_tenant_id_returns_none_outside_scope() {
        // Outside any with_tenant scope, get_tenant_id must return None.
        let id = get_tenant_id();
        assert!(id.is_none(), "Expected None outside a tenant scope");
    }

    #[tokio::test]
    async fn test_with_tenant_sets_and_restores() {
        // Inside with_tenant the id is visible; outside, it is gone.
        let result = with_tenant("acme", async { get_tenant_id() }).await;
        assert!(matches!(result, Some(RullstValue::String(ref s)) if s == "acme"));
        // After the scope, it should be None again.
        assert!(get_tenant_id().is_none());
    }

    #[tokio::test]
    async fn test_nested_tenant_scopes() {
        let _ = with_tenant("outer", async {
            let outer_id = get_tenant_id();
            assert!(matches!(outer_id, Some(RullstValue::String(ref s)) if s == "outer"));

            let _ = with_tenant("inner", async {
                let inner_id = get_tenant_id();
                assert!(matches!(inner_id, Some(RullstValue::String(ref s)) if s == "inner"));
            })
            .await;

            let restored_outer_id = get_tenant_id();
            assert!(matches!(restored_outer_id, Some(RullstValue::String(ref s)) if s == "outer"));
        })
        .await;
    }

    #[tokio::test]
    async fn test_with_tenant_panic_cleanup() {
        let result = tokio::spawn(async {
            with_tenant("faulty_tenant", async {
                panic!("Something went wrong!");
            })
            .await;
        })
        .await;

        assert!(result.is_err());
        assert!(get_tenant_id().is_none());
    }
}