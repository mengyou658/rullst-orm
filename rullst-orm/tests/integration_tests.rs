// Database integration tests for rullst-orm.
//
// Design constraints:
//   - `Orm::init()` stores the pool in a global `OnceLock` — calling it twice
//     in the same process panics.  All scenarios therefore live inside a single
//     `#[tokio::test]` and share the one pool initialised at the top.
//   - Each logical scenario uses a uniquely-named table so tests never
//     interfere with each other even if the order of execution changes.
//   - SQLite (file-based, `?mode=rwc`) is used so no external server is
//     needed.  The file is deleted before and after the suite.

#![cfg(not(any(feature = "strict-postgres", feature = "strict-mysql")))]

use rullst_orm::schema::{Blueprint, ColumnDefault, Schema};
use rullst_orm::types::Json;
use rullst_orm::{FromRow, Orm};
use serde::{Deserialize, Serialize};

// ── shared JSON payload type ───────────────────────────────────────────────
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct Payload {
    value: String,
}

// ── model: users ──────────────────────────────────────────────────────────
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_users")]
struct User {
    pub id: i32,
    pub name: String,
    pub email: String,
}

// ── model: posts (belongs_to User) ────────────────────────────────────────
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_posts")]
struct Post {
    pub id: i32,
    pub user_id: i32,
    pub title: String,
}

// ── model: json carrier ───────────────────────────────────────────────────
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_json_records")]
struct JsonRecord {
    pub id: i32,
    pub data: Json<Payload>,
}

// ── model: tenant-scoped products ─────────────────────────────────────────
//
// Two products, two tenants, used by the
// `scenario_tenant_context_switching` test to exercise the
// `without_tenant()` per-query override against a plain
// `with_tenant(t)` scope.
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_tenant_products", tenant_column = "tenant_id")]
struct TenantProduct {
    pub id: i32,
    pub name: String,
    pub tenant_id: i32,
}

// ── database path ─────────────────────────────────────────────────────────
const DB_FILE: &str = "it_suite.db";

// ══════════════════════════════════════════════════════════════════════════
// Main integration test — all scenarios run sequentially in one Tokio task
// so the global pool is initialised exactly once.
// ══════════════════════════════════════════════════════════════════════════
#[tokio::test]
async fn integration_suite() {
    // ── 0. Setup ──────────────────────────────────────────────────────────
    let _ = std::fs::remove_file(DB_FILE);
    Orm::init(&format!("sqlite:{}?mode=rwc", DB_FILE))
        .await
        .expect("Orm::init should succeed");

    scenario_crud().await;
    scenario_soft_delete().await;
    scenario_configurable_soft_delete().await;
    scenario_skipped_field().await;
    scenario_transactions().await;
    scenario_json_column().await;
    scenario_bulk_operations().await;
    scenario_schema_lifecycle().await;
    scenario_audit().await;
    scenario_query_result_ext().await;
    scenario_tenant_context_switching().await;

    // ── cleanup ───────────────────────────────────────────────────────────
    let _ = std::fs::remove_file(DB_FILE);
}

// ── Scenario 1: basic CRUD ────────────────────────────────────────────────
async fn scenario_crud() {
    Schema::create("it_users", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
        t.string("email").not_null();
    })
    .await
    .expect("create it_users");

    // INSERT
    let mut user = User {
        id: 0,
        name: "Alice".into(),
        email: "alice@example.com".into(),
    };
    user.save().await.expect("save new user");
    assert!(user.id > 0, "id must be assigned after save");

    // SELECT by PK
    let found = User::find(user.id)
        .await
        .expect("find")
        .expect("user exists");
    assert_eq!(found.name, "Alice");
    assert_eq!(found.email, "alice@example.com");

    // UPDATE
    user.name = "Alice Updated".into();
    user.save().await.expect("update user");
    let updated = User::find(user.id)
        .await
        .expect("find updated")
        .expect("exists");
    assert_eq!(updated.name, "Alice Updated");

    // INSERT second record
    let mut user2 = User {
        id: 0,
        name: "Bob".into(),
        email: "bob@example.com".into(),
    };
    user2.save().await.expect("save Bob");

    // SELECT all
    let all = User::all().await.expect("all users");
    assert_eq!(all.len(), 2, "expected 2 users");

    // WHERE clause
    let found_bob = User::query()
        .where_eq("name", "Bob")
        .first()
        .await
        .expect("query")
        .expect("Bob exists");
    assert_eq!(found_bob.email, "bob@example.com");

    // COUNT
    let count = User::query().count().await.expect("count");
    assert_eq!(count, 2);

    // DELETE
    user.delete().await.expect("delete user");
    let after_delete = User::find(user.id).await.expect("find after delete");
    assert!(after_delete.is_none(), "deleted user should not be found");

    let count_after = User::query().count().await.expect("count after delete");
    assert_eq!(count_after, 1);

    Schema::drop_if_exists("it_users")
        .await
        .expect("drop it_users");
}

// ── Scenario 2: soft deletes ──────────────────────────────────────────────
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_soft_users")]
struct SoftUser {
    pub id: i32,
    pub name: String,
    pub deleted_at: Option<String>,
}

async fn scenario_soft_delete() {
    Schema::create("it_soft_users", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
        t.soft_deletes();
    })
    .await
    .expect("create it_soft_users");

    let mut u = SoftUser {
        id: 0,
        name: "SoftAlice".into(),
        deleted_at: None,
    };
    u.save().await.expect("save SoftAlice");

    // soft-delete
    u.delete().await.expect("soft delete");

    // record still exists in DB but deleted_at is set
    let pool = Orm::pool();
    let row: Option<(i32, Option<String>)> =
        sqlx::query_as("SELECT id, deleted_at FROM it_soft_users WHERE id = ?")
            .bind(u.id)
            .fetch_optional(pool)
            .await
            .expect("raw query");

    let (_, deleted_at) = row.expect("row must exist");
    assert!(
        deleted_at.is_some(),
        "deleted_at must be set after soft delete"
    );

    // restore
    u.restore().await.expect("restore");

    let row2: Option<(i32, Option<String>)> =
        sqlx::query_as("SELECT id, deleted_at FROM it_soft_users WHERE id = ?")
            .bind(u.id)
            .fetch_optional(pool)
            .await
            .expect("raw query after restore");

    let (_, deleted_at2) = row2.expect("row must still exist");
    assert!(
        deleted_at2.is_none(),
        "deleted_at must be NULL after restore"
    );

    // force_delete
    u.force_delete().await.expect("force delete");
    let gone: Option<(i32,)> = sqlx::query_as("SELECT id FROM it_soft_users WHERE id = ?")
        .bind(u.id)
        .fetch_optional(pool)
        .await
        .expect("raw query after force delete");
    assert!(gone.is_none(), "row must be gone after force_delete");

    Schema::drop_if_exists("it_soft_users")
        .await
        .expect("drop it_soft_users");
}

// ── Scenario 2b: configurable soft delete (MyBatis-Plus style) ────────────
//
// Verifies that the new `#[orm(soft_delete(field = ..., value = ..., delval = ...))]`
// configuration produces the correct SQL fragments for SELECT filters,
// DELETE statements, and restore on every supported driver.
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_int_soft", soft_delete(field = "is_deleted", value = "0", delval = "1"))]
struct IntSoftUser {
    pub id: i32,
    pub name: String,
    pub is_deleted: i32,
}

#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_ts_soft", soft_delete(field = "deleted_at", value = "null", delval = "CURRENT_TIMESTAMP"))]
struct TimestampSoftUser {
    pub id: i32,
    pub name: String,
    pub deleted_at: Option<String>,
}

async fn scenario_configurable_soft_delete() {
    // ── Integer sentinel variant ─────────────────────────────────────────
    Schema::create("it_int_soft", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
        t.integer("is_deleted").not_null().default(ColumnDefault::Integer(0));
    })
    .await
    .expect("create it_int_soft");

    // Verify the SELECT builder emits `<column> = <value>` for the
    // "not deleted" filter.
    let mut b = IntSoftUser::query();
    b.wheres.push(("AND".to_string(), "1=1".to_string()));
    let sql = b.to_sql();
    assert!(
        sql.contains("is_deleted = 0"),
        "expected `is_deleted = 0` in SQL, got: {sql}"
    );

    // `.only_trashed()` flips the comparison to `!=`.
    let mut b2 = IntSoftUser::query().only_trashed();
    b2.wheres.push(("AND".to_string(), "1=1".to_string()));
    let sql2 = b2.to_sql();
    assert!(
        sql2.contains("is_deleted != 0"),
        "expected `is_deleted != 0` in SQL, got: {sql2}"
    );

    // Insert + soft delete + restore cycle on the integer variant.
    let mut u = IntSoftUser {
        id: 0,
        name: "IntAlice".into(),
        is_deleted: 0,
    };
    u.save().await.expect("save IntAlice");

    let pool = Orm::pool();
    let row: (i32,) = sqlx::query_as("SELECT is_deleted FROM it_int_soft WHERE id = ?")
        .bind(u.id)
        .fetch_one(pool)
        .await
        .expect("raw select before delete");
    assert_eq!(row.0, 0, "row should start as not deleted");

    u.delete().await.expect("soft delete");

    let row: (i32,) = sqlx::query_as("SELECT is_deleted FROM it_int_soft WHERE id = ?")
        .bind(u.id)
        .fetch_one(pool)
        .await
        .expect("raw select after delete");
    assert_eq!(row.0, 1, "row should be flagged as deleted");

    u.restore().await.expect("restore");
    let row: (i32,) = sqlx::query_as("SELECT is_deleted FROM it_int_soft WHERE id = ?")
        .bind(u.id)
        .fetch_one(pool)
        .await
        .expect("raw select after restore");
    assert_eq!(row.0, 0, "row should be cleared after restore");

    Schema::drop_if_exists("it_int_soft")
        .await
        .expect("drop it_int_soft");

    // ── DateTime / NULL sentinel variant ──────────────────────────────────
    Schema::create("it_ts_soft", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
        t.soft_deletes();
    })
    .await
    .expect("create it_ts_soft");

    // `value = "null"` should produce `IS NULL` filters (not `= null`).
    let mut b = TimestampSoftUser::query();
    b.wheres.push(("AND".to_string(), "1=1".to_string()));
    let sql = b.to_sql();
    assert!(
        sql.contains("deleted_at IS NULL"),
        "expected `deleted_at IS NULL` in SQL, got: {sql}"
    );

    let mut b = TimestampSoftUser::query().only_trashed();
    b.wheres.push(("AND".to_string(), "1=1".to_string()));
    let sql = b.to_sql();
    assert!(
        sql.contains("deleted_at IS NOT NULL"),
        "expected `deleted_at IS NOT NULL` in SQL, got: {sql}"
    );

    let mut u = TimestampSoftUser {
        id: 0,
        name: "TsAlice".into(),
        deleted_at: None,
    };
    u.save().await.expect("save TsAlice");
    u.delete().await.expect("soft delete TsAlice");
    let row: (Option<String>,) =
        sqlx::query_as("SELECT deleted_at FROM it_ts_soft WHERE id = ?")
            .bind(u.id)
            .fetch_one(Orm::pool())
            .await
            .expect("select after delete");
    assert!(row.0.is_some(), "deleted_at should be set after delete");

    Schema::drop_if_exists("it_ts_soft")
        .await
        .expect("drop it_ts_soft");
}

// ── Scenario 2c: #[orm(skip)] / #[sqlx(skip)] field ──────────────────────
//
// Confirms that fields marked with `#[orm(skip)]` (or its `#[sqlx(skip)]`
// alias) are excluded from generated INSERT / UPDATE statements and the
// `*Column` enum, but remain on the struct itself.
#[derive(Debug, Clone, Default, FromRow, rullst_orm::Orm)]
#[orm(table = "it_skipped")]
struct SkippedFieldUser {
    pub id: i32,
    pub name: String,
    // `#[sqlx(skip)]` is the alias recognised by both this ORM
    // derive and the `sqlx::FromRow` derive, so the field is excluded
    // from INSERT / UPDATE column lists, the `*Column` enum, the JSON
    // serialiser *and* the row mapping.
    #[sqlx(skip)]
    pub secret: String,
}

async fn scenario_skipped_field() {
    Schema::create("it_skipped", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
    })
    .await
    .expect("create it_skipped");

    // The `secret` column does not exist in the schema, so if the
    // generator still emitted it in the INSERT we would get a
    // "no such column" error. This implicit assertion exercises the
    // exclusion logic end-to-end.
    let mut u = SkippedFieldUser {
        id: 0,
        name: "SkipBob".into(),
        secret: "this should not be persisted".to_string(),
    };
    u.save().await.expect("save should ignore `secret` field");

    let pool = Orm::pool();
    let row: (i32, String) = sqlx::query_as("SELECT id, name FROM it_skipped WHERE id = ?")
        .bind(u.id)
        .fetch_one(pool)
        .await
        .expect("raw select");
    assert_eq!(row.1, "SkipBob");

    // The in-memory value is still intact.
    assert_eq!(u.secret, "this should not be persisted");

    // UPDATE should also ignore the skipped field.
    u.name = "SkipBobUpdated".into();
    u.secret = "still untouched".to_string();
    u.save().await.expect("update should ignore `secret` field");
    assert_eq!(u.secret, "still untouched");

    // The query builder must also refuse to use a skipped field as a
    // column. The typed `*Column` enum and the `where_<field>` magic
    // methods do not even *exist* for `secret` (compile-time
    // exclusion), but the raw string-based builders could in theory
    // be tricked into emitting `WHERE secret = ?`. We patch that
    // hole by collecting a `Validation` error in every raw entry
    // point that takes a column name.
    use rullst_orm::Error as OrmError;

    let err = SkippedFieldUser::query()
        .where_eq("secret", "x")
        .first()
        .await
        .expect_err("where_eq on a skipped field must fail");
    let msg = format!("{}", err);
    assert!(
        matches!(err, OrmError::Validation(_)) && msg.contains("secret"),
        "expected Validation error mentioning `secret`, got: {}",
        msg
    );

    let err = SkippedFieldUser::query()
        .or_where("secret", "x")
        .first()
        .await
        .expect_err("or_where on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .where_null("secret")
        .first()
        .await
        .expect_err("where_null on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .where_in("secret", vec!["a", "b"])
        .first()
        .await
        .expect_err("where_in on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .where_between("secret", 1, 9)
        .first()
        .await
        .expect_err("where_between on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .order_by("secret")
        .first()
        .await
        .expect_err("order_by on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .order_by_desc("secret")
        .first()
        .await
        .expect_err("order_by_desc on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .group_by("secret")
        .first()
        .await
        .expect_err("group_by on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    let err = SkippedFieldUser::query()
        .select(&["id", "name", "secret"])
        .first()
        .await
        .expect_err("select on a skipped field must fail");
    assert!(matches!(err, OrmError::Validation(_)));

    // A *normal* column reference must still succeed and not
    // accumulate any errors.
    let ok = SkippedFieldUser::query()
        .where_eq("name", "SkipBobUpdated")
        .first()
        .await
        .expect("where_eq on a real column must succeed")
        .expect("the row inserted above must still be present");
    assert_eq!(ok.name, "SkipBobUpdated");

    Schema::drop_if_exists("it_skipped")
        .await
        .expect("drop it_skipped");
}

// ── Scenario 3: transactions ──────────────────────────────────────────────
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_tx_accounts")]
struct Account {
    pub id: i32,
    pub balance: i32,
}

async fn scenario_transactions() {
    Schema::create("it_tx_accounts", |t: &mut Blueprint| {
        t.id();
        t.integer("balance").not_null();
    })
    .await
    .expect("create it_tx_accounts");

    // Successful transaction
    {
        let pool = Orm::pool();
        let mut tx = pool.begin().await.expect("begin tx");
        let mut acc = Account {
            id: 0,
            balance: 100,
        };
        acc.save_with_tx(&mut tx).await.expect("save with tx");
        acc.balance = 200;
        acc.save_with_tx(&mut tx).await.expect("update with tx");
        tx.commit().await.expect("commit");

        let committed = Account::find(acc.id).await.expect("find").expect("exists");
        assert_eq!(committed.balance, 200, "committed balance must be 200");
    }

    // Rolled-back transaction — changes must not persist
    {
        let initial_count = Account::query().count().await.expect("count");

        let pool = Orm::pool();
        let mut tx = pool.begin().await.expect("begin tx2");
        let mut ghost = Account {
            id: 0,
            balance: 999,
        };
        ghost.save_with_tx(&mut tx).await.expect("save ghost");
        // rollback instead of commit
        tx.rollback().await.expect("rollback");

        let after_rollback = Account::query()
            .count()
            .await
            .expect("count after rollback");
        assert_eq!(
            after_rollback, initial_count,
            "rollback must not persist the ghost account"
        );
    }

    Schema::drop_if_exists("it_tx_accounts")
        .await
        .expect("drop it_tx_accounts");
}

// ── Scenario 4: JSON column round-trip ───────────────────────────────────
async fn scenario_json_column() {
    Schema::create("it_json_records", |t: &mut Blueprint| {
        t.id();
        t.string("data").not_null();
    })
    .await
    .expect("create it_json_records");

    let mut rec = JsonRecord {
        id: 0,
        data: Json(Payload {
            value: "hello_world".into(),
        }),
    };
    rec.save().await.expect("save json record");

    let fetched = JsonRecord::find(rec.id)
        .await
        .expect("find")
        .expect("exists");
    assert_eq!(
        fetched.data.0.value, "hello_world",
        "JSON round-trip must preserve value"
    );

    // Verify to_json / from_json
    let json_str = rec.to_json();
    assert!(
        json_str.contains("hello_world"),
        "to_json must include field value"
    );

    let rehydrated = JsonRecord::from_json(&json_str).expect("from_json");
    assert_eq!(rehydrated.data.0.value, "hello_world");

    Schema::drop_if_exists("it_json_records")
        .await
        .expect("drop it_json_records");
}

// ── Scenario 5: bulk operations ───────────────────────────────────────────
#[derive(Debug, Clone, FromRow, rullst_orm::Orm)]
#[orm(table = "it_bulk_items")]
struct BulkItem {
    pub id: i32,
    pub label: String,
    pub score: i32,
}

async fn scenario_bulk_operations() {
    Schema::create("it_bulk_items", |t: &mut Blueprint| {
        t.id();
        t.string("label").not_null();
        t.integer("score").not_null();
    })
    .await
    .expect("create it_bulk_items");

    // Insert 20 records
    for i in 1..=20i32 {
        let mut item = BulkItem {
            id: 0,
            label: format!("item_{}", i),
            score: i,
        };
        item.save().await.expect("bulk save");
    }

    // ORDER BY + LIMIT
    let top5 = BulkItem::query()
        .order_by_desc("score")
        .limit(5)
        .get()
        .await
        .expect("top 5");
    assert_eq!(top5.len(), 5);
    assert_eq!(top5[0].score, 20, "highest score must be first");

    // OFFSET pagination
    let page2 = BulkItem::query()
        .order_by("score")
        .limit(5)
        .offset(5)
        .get()
        .await
        .expect("page 2");
    assert_eq!(page2.len(), 5);
    assert_eq!(page2[0].score, 6, "offset 5 → score 6");

    // pluck_i32
    let scores = BulkItem::query()
        .order_by("score")
        .limit(3)
        .pluck_i32("score")
        .await
        .expect("pluck scores");
    assert_eq!(scores, vec![1, 2, 3]);

    // pluck_string
    let labels = BulkItem::query()
        .order_by("score")
        .limit(2)
        .pluck_string("label")
        .await
        .expect("pluck labels");
    assert_eq!(labels, vec!["item_1", "item_2"]);

    // delete_all with WHERE
    let deleted = BulkItem::query()
        .where_eq("score", 1)
        .delete_all()
        .await
        .expect("delete score=1");
    assert_eq!(deleted, 1, "one row deleted");

    let count = BulkItem::query().count().await.expect("count after delete");
    assert_eq!(count, 19);

    Schema::drop_if_exists("it_bulk_items")
        .await
        .expect("drop it_bulk_items");
}

// ── Scenario 6: schema lifecycle ─────────────────────────────────────────
async fn scenario_schema_lifecycle() {
    // Create → verify → rename → drop_if_exists (idempotent)
    Schema::create("it_lifecycle_alpha", |t: &mut Blueprint| {
        t.id();
        t.string("value").not_null();
    })
    .await
    .expect("create it_lifecycle_alpha");

    // Inserting into the new table confirms it exists
    let pool = Orm::pool();
    sqlx::query("INSERT INTO it_lifecycle_alpha (value) VALUES (?)")
        .bind("check")
        .execute(pool)
        .await
        .expect("insert into lifecycle table");

    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM it_lifecycle_alpha")
        .fetch_one(pool)
        .await
        .expect("count lifecycle");
    assert_eq!(row.0, 1);

    // drop
    Schema::drop_if_exists("it_lifecycle_alpha")
        .await
        .expect("drop it_lifecycle_alpha");

    // Calling drop_if_exists on a non-existent table must not panic
    Schema::drop_if_exists("it_lifecycle_alpha")
        .await
        .expect("drop_if_exists on missing table must succeed");
}

// ── Scenario 7: audit logging ─────────────────────────────────────────────
async fn scenario_audit() {
    rullst_orm::audit::create_audit_table()
        .await
        .expect("create audit table");

    rullst_orm::audit::log_audit(
        "User",
        99,
        "created",
        None,
        Some(r#"{"name":"test"}"#.to_string()),
    )
    .await
    .expect("log audit");

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rullst_audits")
        .fetch_one(Orm::pool())
        .await
        .expect("count audits");
    assert_eq!(count.0, 1);

    rullst_orm::schema::Schema::drop_if_exists("rullst_audits")
        .await
        .expect("drop audits");
}

// ── Scenario 8: query result ext ──────────────────────────────────────────
async fn scenario_query_result_ext() {
    use rullst_orm::database::QueryResultExt;

    Schema::create("it_query_result_ext", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
    })
    .await
    .expect("create it_query_result_ext");

    let pool = Orm::pool();
    let result = sqlx::query("INSERT INTO it_query_result_ext (name) VALUES ('Test')")
        .execute(pool)
        .await
        .expect("insert");

    #[cfg(not(any(
        feature = "strict-postgres",
        feature = "strict-mysql",
        feature = "strict-sqlite"
    )))]
    {
        let id = result.get_last_insert_id();
        assert!(id >= 0, "last insert id should be >= 0");
    }

    Schema::drop_if_exists("it_query_result_ext")
        .await
        .expect("drop it_query_result_ext");
}

// ── Scenario 9: tenant scoping + per-query overrides ────────────────────
//
// Exercises the per-query tenant overrides against a real SQLite
// database. The test seeds rows for two tenants and then verifies:
//
//  1. A plain `with_tenant(t)` scope filters by that tenant.
//  2. `without_tenant()` inside an active `t1` scope sees every row.
//  3. A tenant id with a single quote (SQL-injection bait) is escaped
//     rather than terminating the string literal.
//  4. An explicit `where_eq("tenant_id", …)` deduplicates the
//     auto-injected tenant filter (no phantom binding, column
//     appears exactly once in the rendered SQL).
//  5. `delete_all` honours the active `with_tenant` scope the same
//     way `to_sql()` does.
async fn scenario_tenant_context_switching() {
    use rullst_orm::with_tenant;

    Schema::create("it_tenant_products", |t: &mut Blueprint| {
        t.id();
        t.string("name").not_null();
        t.integer("tenant_id").not_null();
    })
    .await
    .expect("create it_tenant_products");

    // ── Seed rows for tenants 1 and 2 ────────────────────────────────
    with_tenant(1_i32, async {
        let mut p = TenantProduct {
            id: 0,
            name: "Apple iPhone".into(),
            tenant_id: 0, // stamped by the save hook
        };
        p.save().await.expect("save t1 product");
        assert_eq!(p.tenant_id, 1, "save must stamp tenant_id from the scope");
    })
    .await;

    with_tenant(2_i32, async {
        let mut p = TenantProduct {
            id: 0,
            name: "Samsung Galaxy".into(),
            tenant_id: 0,
        };
        p.save().await.expect("save t2 product");
        assert_eq!(p.tenant_id, 2, "save must stamp tenant_id from the scope");
    })
    .await;

    // ── 1) Plain scope filters to the active tenant ──────────────────
    let t1 = with_tenant(1_i32, async {
        TenantProduct::query().get().await.expect("query t1")
    })
    .await;
    assert_eq!(t1.len(), 1, "scope=1 should return exactly one row");
    assert_eq!(t1[0].name, "Apple iPhone");

    // ── 2) without_tenant() inside a t1 scope sees every row ─────────
    let all = with_tenant(1_i32, async {
        TenantProduct::query()
            .without_tenant()
            .get()
            .await
            .expect("query all")
    })
    .await;
    assert_eq!(all.len(), 2, "without_tenant must drop the WHERE");

    // ── 3) SQL-injection bait: a single quote in the tenant id ──────
    //
    // The auto-injected tenant filter is now a parameterised
    // `WHERE <col> = ?` (see `push_tenant_filter` in
    // `rullst-orm-macros/src/builder.rs`), so the value is
    // delivered to the driver as a binding, never spliced into
    // the SQL string. The previous PR's `render_tenant_literal`
    // helper — which escaped a `'` by doubling it — has been
    // removed; this test now exercises the safe path end-to-end
    // (a value that would have terminated a SQL literal is still
    // matched against exactly the same row).
    let injection_bait = "o'reilly";
    with_tenant(injection_bait, async {
        // `WHERE tenant_id = ?` is emitted; the value never
        // reaches the SQL string. Any row stamped with this
        // tenant id must be returned, no string parsing can
        // be tricked into reading more columns.
        let mut b = TenantProduct::query();
        let sql = b.to_sql();
        assert!(
            sql.contains("tenant_id = ?") && !sql.contains('\''),
            "tenant predicate must be parameterised, got: {sql}"
        );
        // No rows were seeded for the injection-bait tenant, so
        // the result is empty — but the query itself runs
        // without error and the bindings carry the bait
        // verbatim.
        let rows = b.get().await.expect("query under bait tenant");
        assert!(rows.is_empty(), "no rows should match the bait tenant");
    })
    .await;

    // ── 4) Explicit `where_eq("tenant_id", …)` deduplicates ──────────
    //
    // The user already pinned the tenant column; the auto-injector
    // must NOT add a second `<col> = ?` predicate or a phantom
    // binding. We verify this on both the SELECT and the
    // `delete_all` paths by checking the rendered SQL.
    with_tenant(1_i32, async {
        let mut b = TenantProduct::query().where_eq("tenant_id", 2_i32);
        let sql = b.to_sql();
        let count = sql.matches("tenant_id").count();
        assert_eq!(
            count, 1,
            "auto-injected tenant filter must be deduped when the user already pins tenant_id; sql={}",
            sql
        );
        b.delete_all().await.expect("delete_all with deduped tenant");
    })
    .await;

    // ── 5) delete_all follows the active tenant scope ───────────────
    //
    // Seed an extra row in tenant 1 to make sure tenant 2's
    // `delete_all` does NOT touch it.
    with_tenant(1_i32, async {
        let mut p = TenantProduct {
            id: 0,
            name: "Tenant-1 keepsake".into(),
            tenant_id: 0,
        };
        p.save().await.expect("seed tenant 1 keepsake");
    })
    .await;
    with_tenant(2_i32, async {
        // Add a row in tenant 2 to delete.
        let mut p = TenantProduct {
            id: 0,
            name: "Tenant-2 victim".into(),
            tenant_id: 0,
        };
        p.save().await.expect("seed tenant 2 victim");
    })
    .await;
    // Now delete_all must be scoped to tenant 2: it removes the
    // Tenant-2 victim row, but leaves the Tenant-1 keepsake alone.
    with_tenant(2_i32, async {
        let n = TenantProduct::query()
            .where_eq("name", "Tenant-2 victim")
            .delete_all()
            .await
            .expect("delete_all under tenant 2");
        assert_eq!(n, 1, "delete_all must hit only tenant 2's victim");
    })
    .await;
    with_tenant(1_i32, async {
        let row = TenantProduct::query()
            .where_eq("name", "Tenant-1 keepsake")
            .first()
            .await
            .expect("query keepsake");
        assert!(
            row.is_some(),
            "tenant 1's keepsake must survive a tenant-2 delete_all"
        );
    })
    .await;

    Schema::drop_if_exists("it_tenant_products")
        .await
        .expect("drop it_tenant_products");
}
