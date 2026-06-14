use crate::Error;

/// Allowlist of SQL comparison/join operators accepted in raw clause builders.
const ALLOWED_OPERATORS: &[&str] = &["=", "!=", "<>", "<", ">", "<=", ">="];

/// Validates a SQL identifier (column or table name) to prevent SQL injection.
/// Allows alphanumeric characters, underscores, hyphens and a single dot
/// for qualified names like `table.column`.
pub fn validate_identifier(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::Internal(
            "SQL identifier cannot be empty".to_string(),
        ));
    }
    // At most one dot is allowed (for `table.column` notation),
    // and it must not be the first or last character.
    let dot_count = name.chars().filter(|&c| c == '.').count();
    if dot_count > 1 {
        return Err(Error::Internal(format!(
            "Invalid SQL identifier '{}': at most one dot is allowed",
            name
        )));
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err(Error::Internal(format!(
            "Invalid SQL identifier '{}': must not start or end with a dot",
            name
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(Error::Internal(format!(
            "Invalid SQL identifier '{}': only alphanumeric characters, underscores, hyphens and dots are allowed",
            name
        )));
    }
    Ok(())
}

/// Validates a table name to prevent SQL injection.
/// Wraps `validate_identifier` but disallows dots (table names have no qualifier).
fn validate_table_name(table_name: &str) -> Result<(), Error> {
    if table_name.contains('.') {
        return Err(Error::Internal(format!(
            "Invalid table name '{}': dots are not allowed in table names",
            table_name
        )));
    }
    validate_identifier(table_name)
}

/// Safe values allowed for a column DEFAULT clause.
///
/// Accepting a raw `&str` would allow DDL injection through the DEFAULT
/// position. This enum restricts callers to known-safe literals.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnDefault {
    /// `CURRENT_TIMESTAMP` — standard SQL timestamp literal.
    CurrentTimestamp,
    /// `NULL` — explicit SQL null default.
    Null,
    /// A non-negative integer literal (e.g. `0`, `1`).
    Integer(i64),
    /// A non-negative real literal (e.g. `0.0`).
    Float(f64),
    /// A string literal that will be single-quoted and escaped.
    /// Only printable ASCII excluding `'` and `\` is accepted.
    Text(String),
}

impl ColumnDefault {
    /// Renders the default value as a safe SQL fragment.
    pub fn to_sql(&self) -> String {
        match self {
            ColumnDefault::CurrentTimestamp => "CURRENT_TIMESTAMP".to_string(),
            ColumnDefault::Null => "NULL".to_string(),
            ColumnDefault::Integer(n) => n.to_string(),
            ColumnDefault::Float(f) => format!("{f}"),
            // Single-quote the string and escape any embedded single-quotes
            // via SQL standard doubling (''), which is safe on every driver.
            ColumnDefault::Text(s) => format!("'{}'", s.replace('\'', "''")),
        }
    }
}

pub struct Column {
    pub name: String,
    pub col_type: String,
    pub is_nullable: bool,
    pub is_primary_key: bool,
    pub is_auto_increment: bool,
    pub default_value: Option<ColumnDefault>,
}

impl Column {
    /// Creates a new column, validating `name` against SQL identifier rules.
    ///
    /// # Panics
    /// Panics if `name` fails identifier validation. Column names are always
    /// developer-supplied compile-time literals — an invalid name is a bug,
    /// not a runtime condition.
    pub fn new(name: &str, col_type: &str) -> Self {
        validate_identifier(name)
            .unwrap_or_else(|e| panic!("Invalid column name {:?}: {}", name, e));
        Self {
            name: name.to_string(),
            col_type: col_type.to_string(),
            is_nullable: true,
            is_primary_key: false,
            is_auto_increment: false,
            default_value: None,
        }
    }

    pub fn not_null(&mut self) -> &mut Self {
        self.is_nullable = false;
        self
    }

    pub fn nullable(&mut self) -> &mut Self {
        self.is_nullable = true;
        self
    }

    /// Sets a safe DEFAULT value using the [`ColumnDefault`] enum.
    ///
    /// The old `&str` overload has been removed to prevent DDL injection
    /// through unescaped DEFAULT clauses.
    pub fn default(&mut self, val: ColumnDefault) -> &mut Self {
        self.default_value = Some(val);
        self
    }

    pub fn primary(&mut self) -> &mut Self {
        self.is_primary_key = true;
        self
    }
}

pub struct Blueprint {
    pub columns: Vec<Column>,
}

impl Default for Blueprint {
    fn default() -> Self {
        Self::new()
    }
}

impl Blueprint {
    pub fn new() -> Self {
        Self { columns: vec![] }
    }

    pub fn id(&mut self) -> &mut Column {
        self.columns.push(Column {
            name: "id".to_string(),
            col_type: "INTEGER".to_string(),
            is_nullable: false,
            is_primary_key: true,
            is_auto_increment: true,
            default_value: None,
        });
        self.columns
            .last_mut()
            .expect("BUG: columns is empty after push")
    }

    fn add_column(&mut self, name: &str, col_type: &str) -> &mut Column {
        let col = Column::new(name, col_type);
        self.columns.push(col);
        self.columns
            .last_mut()
            .expect("BUG: columns is empty after push")
    }

    pub fn string(&mut self, name: &str) -> &mut Column {
        self.add_column(name, "TEXT")
    }

    pub fn integer(&mut self, name: &str) -> &mut Column {
        self.add_column(name, "INTEGER")
    }

    pub fn float(&mut self, name: &str) -> &mut Column {
        self.add_column(name, "REAL")
    }

    pub fn boolean(&mut self, name: &str) -> &mut Column {
        self.add_column(name, "INTEGER")
    }

    pub fn timestamps(&mut self) {
        let mut created = Column::new("created_at", "TEXT");
        created.default(ColumnDefault::CurrentTimestamp);
        self.columns.push(created);

        let mut updated = Column::new("updated_at", "TEXT");
        updated.default(ColumnDefault::CurrentTimestamp);
        self.columns.push(updated);
    }

    pub fn soft_deletes(&mut self) {
        let col = Column::new("deleted_at", "TEXT");
        self.columns.push(col);
        self.columns
            .last_mut()
            .expect("BUG: columns is empty after push")
            .nullable();
    }

    pub fn build(&self) -> Result<String, Error> {
        let mut defs = vec![];
        for col in &self.columns {
            // Defensive re-validation: column names must always be safe
            // identifiers regardless of how the Column was constructed.
            validate_identifier(&col.name)?;
            let mut def = format!("{} {}", col.name, col.col_type);
            if col.is_primary_key {
                def.push_str(" PRIMARY KEY");
            }
            if col.is_auto_increment {
                def.push_str(" AUTOINCREMENT");
            }
            if !col.is_nullable && !col.is_primary_key {
                def.push_str(" NOT NULL");
            }
            if let Some(default) = &col.default_value {
                def.push_str(&format!(" DEFAULT {}", default.to_sql()));
            }
            defs.push(def);
        }
        Ok(defs.join(",\n    "))
    }
}

pub struct Schema;

impl Schema {
    pub async fn create<F>(table_name: &str, callback: F) -> Result<(), Error>
    where
        F: FnOnce(&mut Blueprint),
    {
        validate_table_name(table_name)?;

        let mut blueprint = Blueprint::new();
        callback(&mut blueprint);

        // build() now returns Result so any column-name or default issues
        // surface as errors rather than producing malformed SQL.
        let columns_sql = blueprint.build()?;
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {} (\n    {}\n);",
            table_name, columns_sql
        );

        let pool = crate::Orm::pool();
        let mut query_builder = sqlx::query_builder::QueryBuilder::new("");
        query_builder.push(&sql);
        query_builder.build().execute(pool).await?;

        Ok(())
    }

    pub async fn drop_if_exists(table_name: &str) -> Result<(), Error> {
        validate_table_name(table_name)?;

        let sql = format!("DROP TABLE IF EXISTS {};", table_name);
        let pool = crate::Orm::pool();
        let mut query_builder = sqlx::query_builder::QueryBuilder::new("");
        query_builder.push(&sql);
        query_builder.build().execute(pool).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
pub trait Migration: Send + Sync {
    fn name(&self) -> &'static str;
    async fn up(&self) -> Result<(), Error>;
    async fn down(&self) -> Result<(), Error>;
}

pub async fn run_artisan_with_args(
    args: &[String],
    migrations: Vec<Box<dyn Migration>>,
    seeders: Vec<Box<dyn crate::Seeder>>,
) -> Result<(), Error> {
    if args.len() < 2 {
        println!("Rullst ORM Artisan CLI");
        println!("Usage:");
        println!("  make:migration <name>   Generate a new migration");
        println!("  migrate                  Run all pending migrations");
        println!("  migrate:rollback         Rollback the last batch of migrations");
        println!("  status                   Show migrations status");
        println!("  db:seed                  Populate the database with seeders");
        return Ok(());
    }

    let command = &args[1];
    match command.as_str() {
        "make:migration" => {
            if args.len() < 3 {
                println!("Error: migration name is required.");
                return Ok(());
            }
            let name = &args[2];
            create_migration_files(name)?;
        }
        "migrate" | "db:migrate" => {
            run_migrations(migrations).await?;
        }
        "migrate:rollback" | "db:rollback" => {
            rollback_migrations(migrations).await?;
        }
        "status" | "db:status" => {
            status_migrations(migrations).await?;
        }
        "db:seed" => {
            println!("Seeding database...");
            crate::Orm::seed(seeders).await?;
            println!("Database seeded successfully!");
        }
        _ => {
            println!("Unknown command: {}", command);
        }
    }
    Ok(())
}

pub async fn run_artisan(
    migrations: Vec<Box<dyn Migration>>,
    seeders: Vec<Box<dyn crate::Seeder>>,
) -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    run_artisan_with_args(&args, migrations, seeders).await
}

async fn status_migrations(migrations: Vec<Box<dyn Migration>>) -> Result<(), Error> {
    let pool = crate::Orm::pool();
    let driver = crate::Orm::driver();

    let table_exists = match driver {
        "postgres" | "mysql" => {
            let query_str =
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'migrations'";
            let row: (i64,) = sqlx::query_as(query_str).fetch_one(pool).await?;
            row.0 > 0
        }
        _ => {
            let query_str =
                "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='migrations'";
            let row: (i64,) = sqlx::query_as(query_str).fetch_one(pool).await?;
            row.0 > 0
        }
    };

    let executed_set = if table_exists {
        let executed: Vec<(String,)> = sqlx::query_as("SELECT migration FROM migrations")
            .fetch_all(pool)
            .await?;
        executed
            .into_iter()
            .map(|(m,)| m)
            .collect::<std::collections::HashSet<String>>()
    } else {
        std::collections::HashSet::new()
    };

    let name_header = "Migration Name";
    let status_header = "Status";
    println!("{name_header:<40} | {status_header}");
    println!("{}", "-".repeat(55));
    for m in migrations {
        let name = m.name();
        let status = if executed_set.contains(name) {
            "Applied"
        } else {
            "Pending"
        };
        println!("{:<40} | {}", name, status);
    }

    Ok(())
}

fn create_migration_files(name: &str) -> Result<(), Error> {
    validate_table_name(name)?;
    use std::fs;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time went backwards")
        .as_secs()
        .to_string();
    let snake_name = name.to_lowercase().replace("-", "_");
    let file_name = format!("m{}_{}", now, snake_name);

    fs::create_dir_all("src/migrations")
        .map_err(|e| Error::Internal(format!("Failed to create migrations directory: {}", e)))?;

    let new_file_path = format!("src/migrations/{}.rs", file_name);
    let migration_code = format!(
        r#"use rullst_orm::schema::{{Schema, Blueprint, Migration}};
use rullst_orm::async_trait;

pub struct MigrationImpl;

#[async_trait]
impl Migration for MigrationImpl {{
    fn name(&self) -> &'static str {{
        "m{timestamp}_{name}"
    }}

    async fn up(&self) -> Result<(), crate::Error> {{
        Schema::create("{name}", |table| {{
            table.id();
            table.timestamps();
        }}).await
    }}

    async fn down(&self) -> Result<(), crate::Error> {{
        Schema::drop_if_exists("{name}").await
    }}
}}
"#,
        timestamp = now,
        name = snake_name
    );

    fs::write(&new_file_path, migration_code)
        .map_err(|e| Error::Internal(format!("Failed to write migration file: {}", e)))?;
    println!("Created migration file: {}", new_file_path);

    regenerate_migrations_mod()?;

    Ok(())
}

fn regenerate_migrations_mod() -> Result<(), Error> {
    use std::fs;
    let paths = fs::read_dir("src/migrations")
        .map_err(|e| Error::Internal(format!("Failed to read migrations dir: {}", e)))?;

    let mut modules = vec![];
    for path in paths {
        let path = path.map_err(|e| Error::Internal(e.to_string()))?.path();
        if let Some(ext) = path.extension()
            && ext == "rs"
            && let Some(stem) = path.file_stem()
        {
            let stem_str = stem.to_string_lossy().to_string();
            if stem_str != "mod" && stem_str.starts_with('m') {
                modules.push(stem_str);
            }
        }
    }
    modules.sort();

    let mut mod_content = String::new();
    mod_content.push_str("// Generated by Rullst ORM Artisan. Do not edit manually.\n\n");
    for m in &modules {
        mod_content.push_str(&format!("pub mod {};\n", m));
    }
    mod_content
        .push_str("\npub fn get_migrations() -> Vec<Box<dyn rullst_orm::schema::Migration>> {\n");
    mod_content.push_str("    vec![\n");
    for m in &modules {
        mod_content.push_str(&format!("        Box::new({}::MigrationImpl),\n", m));
    }
    mod_content.push_str("    ]\n");
    mod_content.push_str("}\n");

    fs::write("src/migrations/mod.rs", mod_content)
        .map_err(|e| Error::Internal(format!("Failed to write mod.rs: {}", e)))?;
    println!("Regenerated src/migrations/mod.rs");

    Ok(())
}

async fn run_migrations(migrations: Vec<Box<dyn Migration>>) -> Result<(), Error> {
    let pool = crate::Orm::pool();
    let driver = crate::Orm::driver();

    let query_str = match driver {
        "postgres" => {
            "CREATE TABLE IF NOT EXISTS migrations (
                id SERIAL PRIMARY KEY,
                migration VARCHAR(255) NOT NULL,
                batch INTEGER NOT NULL
            )"
        }
        "mysql" => {
            "CREATE TABLE IF NOT EXISTS migrations (
                id INT AUTO_INCREMENT PRIMARY KEY,
                migration VARCHAR(255) NOT NULL,
                batch INT NOT NULL
            )"
        }
        _ => {
            "CREATE TABLE IF NOT EXISTS migrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                migration TEXT NOT NULL,
                batch INTEGER NOT NULL
            )"
        }
    };

    sqlx::query(query_str).execute(pool).await?;

    let executed: Vec<(String,)> = sqlx::query_as("SELECT migration FROM migrations")
        .fetch_all(pool)
        .await?;
    let executed_set: std::collections::HashSet<String> =
        executed.into_iter().map(|(m,)| m).collect();

    let batch_row: (Option<i32>,) = sqlx::query_as("SELECT MAX(batch) FROM migrations")
        .fetch_one(pool)
        .await?;
    let next_batch = batch_row.0.unwrap_or(0) + 1;

    let mut count = 0;
    let mut successful_migrations = vec![];
    for m in migrations {
        let name = m.name();
        if !executed_set.contains(name) {
            println!("Migrating: {}", name);
            m.up().await?;
            successful_migrations.push(name);
            println!("Migrated:  {}", name);
            count += 1;
        }
    }

    if count > 0 {
        let mut query_builder =
            sqlx::query_builder::QueryBuilder::new("INSERT INTO migrations (migration, batch) ");
        query_builder.push_values(successful_migrations, |mut b, name| {
            b.push_bind(name).push_bind(next_batch);
        });
        query_builder.build().execute(pool).await?;
    } else {
        println!("Nothing to migrate.");
    }

    Ok(())
}

async fn rollback_migrations(migrations: Vec<Box<dyn Migration>>) -> Result<(), Error> {
    let pool = crate::Orm::pool();
    let driver = crate::Orm::driver();

    let table_exists = match driver {
        "postgres" | "mysql" => {
            let query_str =
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'migrations'";
            let row: (i64,) = sqlx::query_as(query_str).fetch_one(pool).await?;
            row.0 > 0
        }
        _ => {
            let query_str =
                "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='migrations'";
            let row: (i64,) = sqlx::query_as(query_str).fetch_one(pool).await?;
            row.0 > 0
        }
    };

    if !table_exists {
        println!("Nothing to rollback.");
        return Ok(());
    }

    let batch_row: (Option<i32>,) = sqlx::query_as("SELECT MAX(batch) FROM migrations")
        .fetch_one(pool)
        .await?;

    let last_batch = match batch_row.0 {
        Some(b) if b > 0 => b,
        _ => {
            println!("Nothing to rollback.");
            return Ok(());
        }
    };

    let to_rollback: Vec<(String,)> =
        sqlx::query_as("SELECT migration FROM migrations WHERE batch = ? ORDER BY id DESC")
            .bind(last_batch)
            .fetch_all(pool)
            .await?;

    let mut rollback_map = std::collections::HashMap::new();
    for m in migrations {
        rollback_map.insert(m.name().to_string(), m);
    }

    for (name,) in to_rollback {
        if let Some(m) = rollback_map.get(&name) {
            println!("Rolling back: {}", name);
            m.down().await?;
            sqlx::query("DELETE FROM migrations WHERE migration = ?")
                .bind(&name)
                .execute(pool)
                .await?;
            println!("Rolled back:  {}", name);
        } else {
            println!(
                "Warning: migration {} found in database but not in compiled binary.",
                name
            );
        }
    }

    Ok(())
}

pub struct JoinClause {
    pub table: String,
    pub conditions: Vec<String>,
    pub bindings: Vec<crate::RullstValue>,
    pub errors: Vec<crate::Error>,
}

impl JoinClause {
    pub fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
            conditions: vec![],
            bindings: vec![],
            errors: vec![],
        }
    }

    /// Adds a column-to-column JOIN condition.
    ///
    /// This prevents SQL injection — column names should always be hardcoded, never
    /// derived from user input. Returns errors internally rather than panicking.
    pub fn on(&mut self, first: &str, operator: &str, second: &str) -> &mut Self {
        if let Err(e) = validate_identifier(first) {
            self.errors.push(crate::Error::Validation(format!(
                "JoinClause::on — invalid identifier for `first`: {:?}",
                e
            )));
        }
        if let Err(e) = validate_identifier(second) {
            self.errors.push(crate::Error::Validation(format!(
                "JoinClause::on — invalid identifier for `second`: {:?}",
                e
            )));
        }
        if !ALLOWED_OPERATORS.contains(&operator) {
            self.errors.push(crate::Error::Validation(format!(
                "JoinClause::on — invalid operator '{}'. Allowed: {:?}",
                operator, ALLOWED_OPERATORS
            )));
        }
        self.conditions
            .push(format!("{} {} {}", first, operator, second));
        self
    }

    pub fn on_eq<T: Into<crate::RullstValue>>(&mut self, column: &str, value: T) -> &mut Self {
        if let Err(e) = validate_identifier(column) {
            self.errors.push(crate::Error::Validation(format!(
                "JoinClause::on_eq — invalid identifier for `column`: {:?}",
                e
            )));
        }
        self.conditions.push(format!("{} = ?", column));
        self.bindings.push(value.into());
        self
    }

    pub fn to_sql(&self) -> String {
        self.conditions.join(" AND ")
    }
}

pub trait SubqueryBuilder {
    /// Renders the subquery as SQL and pushes any bindings it
    /// needs onto its own `bindings` vec. Takes `&mut self`
    /// because the tenant filter injection path pushes a binding
    /// during SQL construction (see `push_tenant_filter` in the
    /// macro). The owner of the subquery is expected to clone
    /// before passing it in, mirroring the rest of the builder
    /// API.
    fn to_sql(&mut self) -> String;
    fn bindings(&self) -> &Vec<crate::RullstValue>;
}

pub static QUERY_LOGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn enable_query_log() {
    QUERY_LOGGING.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn disable_query_log() {
    QUERY_LOGGING.store(false, std::sync::atomic::Ordering::SeqCst);
}

pub fn is_query_log_enabled() -> bool {
    QUERY_LOGGING.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enable_disable_query_log() {
        disable_query_log();
        assert!(!is_query_log_enabled());
        enable_query_log();
        assert!(is_query_log_enabled());
        disable_query_log();
        assert!(!is_query_log_enabled());
    }

    #[test]
    fn test_join_clause() {
        let mut jc = JoinClause::new("users");
        jc.on("users.id", "=", "posts.user_id");
        assert_eq!(jc.to_sql(), "users.id = posts.user_id");
    }

    #[test]
    fn test_validate_table_name() {
        assert!(validate_table_name("users").is_ok());
        assert!(validate_table_name("user_posts").is_ok());
        assert!(validate_table_name("DROP TABLE users").is_err());
        assert!(validate_table_name("../../../etc/shadow").is_err());
        // dots not allowed in table names
        assert!(validate_table_name("users.id").is_err());
    }

    #[test]
    fn test_validate_identifier() {
        assert!(validate_identifier("users").is_ok());
        assert!(validate_identifier("users.id").is_ok());
        assert!(validate_identifier("user_posts").is_ok());
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("users.posts.id").is_err()); // two dots
        assert!(validate_identifier("DROP TABLE users").is_err());
        assert!(validate_identifier("id; DROP TABLE users--").is_err());
        // Leading/trailing dot edge cases — all now rejected
        assert!(validate_identifier(".").is_err()); // bare dot: starts AND ends with dot
        assert!(validate_identifier(".users").is_err()); // leading dot
        assert!(validate_identifier("users.").is_err()); // trailing dot
        assert!(validate_identifier("user name").is_err()); // Spaces not allowed
        assert!(validate_identifier("admin'--").is_err()); // Quotes not allowed
        assert!(validate_identifier("users()").is_err()); // Parentheses not allowed
        assert!(validate_identifier("a*b").is_err()); // Asterisk not allowed
    }

    #[test]
    fn test_join_clause_on_invalid_operator() {
        let mut jc = JoinClause::new("posts");
        jc.on("posts.user_id", "OR 1=1 --", "users.id");
        assert!(!jc.errors.is_empty());
        assert!(jc.errors[0].to_string().contains("invalid operator"));
    }

    #[test]
    fn test_join_clause_on_invalid_column() {
        let mut jc = JoinClause::new("posts");
        jc.on("users.id; DROP TABLE users--", "=", "posts.user_id");
        assert!(!jc.errors.is_empty());
        assert!(jc.errors[0].to_string().contains("invalid identifier"));
    }

    #[test]
    fn test_timestamps_adds_columns() {
        let mut bp = Blueprint::new();
        bp.timestamps();
        assert_eq!(bp.columns.len(), 2);
        assert_eq!(bp.columns[0].name, "created_at");
        assert_eq!(bp.columns[1].name, "updated_at");
        assert_eq!(
            bp.columns[0].default_value,
            Some(ColumnDefault::CurrentTimestamp)
        );
        assert_eq!(
            bp.columns[1].default_value,
            Some(ColumnDefault::CurrentTimestamp)
        );
    }

    #[test]
    fn test_soft_deletes_adds_nullable_column() {
        let mut bp = Blueprint::new();
        bp.soft_deletes();
        assert_eq!(bp.columns.len(), 1);
        assert_eq!(bp.columns[0].name, "deleted_at");
        assert!(bp.columns[0].is_nullable);
    }

    #[test]
    fn test_blueprint_build_produces_valid_sql() {
        let mut bp = Blueprint::new();
        bp.id();
        bp.string("name").not_null();
        bp.integer("age");
        let sql = bp.build().expect("build should succeed for valid columns");
        assert!(sql.contains("id INTEGER PRIMARY KEY"));
        assert!(sql.contains("name TEXT NOT NULL"));
        assert!(sql.contains("age INTEGER"));
    }

    #[test]
    fn test_column_default_to_sql_escaping() {
        let default_text = ColumnDefault::Text("O'Reilly".to_string());
        assert_eq!(default_text.to_sql(), "'O''Reilly'");
    }

    #[test]
    fn test_validate_identifier_multiple_dots() {
        assert!(validate_identifier("table.column").is_ok()); // one dot
        assert!(validate_identifier("schema.table.column").is_err()); // multiple dots
    }

    #[test]
    fn test_column_default_sql_rendering() {
        assert_eq!(
            ColumnDefault::CurrentTimestamp.to_sql(),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(ColumnDefault::Null.to_sql(), "NULL");
        assert_eq!(ColumnDefault::Integer(42).to_sql(), "42");
        assert_eq!(ColumnDefault::Float(1.23).to_sql(), "1.23");
        assert_eq!(ColumnDefault::Text("hello".to_string()).to_sql(), "'hello'");
        // SQL injection via embedded quote must be escaped
        assert_eq!(ColumnDefault::Text("it's".to_string()).to_sql(), "'it''s'");
    }

    #[test]
    fn test_join_clause_on_eq_binds_value() {
        let mut jc = JoinClause::new("orders");
        jc.on_eq("orders.user_id", 42i32);
        assert_eq!(jc.to_sql(), "orders.user_id = ?");
        assert_eq!(jc.bindings.len(), 1);
    }

    #[test]
    fn test_join_clause_multiple_conditions() {
        let mut jc = JoinClause::new("posts");
        jc.on("posts.user_id", "=", "users.id");
        jc.on("posts.status", ">", "users.min_status");
        assert_eq!(
            jc.to_sql(),
            "posts.user_id = users.id AND posts.status > users.min_status"
        );
    }

    #[test]
    fn test_column_builder_methods() {
        let mut col = Column::new("age", "INTEGER");
        assert_eq!(col.name, "age");
        assert_eq!(col.col_type, "INTEGER");
        assert!(col.is_nullable); // default is true
        assert!(!col.is_primary_key);
        assert!(!col.is_auto_increment);
        assert_eq!(col.default_value, None);

        col.not_null();
        assert!(!col.is_nullable);

        col.nullable();
        assert!(col.is_nullable);

        col.primary();
        assert!(col.is_primary_key);

        col.default(ColumnDefault::Integer(18));
        assert_eq!(col.default_value, Some(ColumnDefault::Integer(18)));
    }

    #[tokio::test]
    async fn test_db_migration_error_state_invalid_blueprint() {
        let result = Schema::create("invalid; DROP TABLE users", |bp| {
            bp.id();
        })
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_drop_if_exists_invalid_table() {
        let result = Schema::drop_if_exists("invalid; name").await;
        assert!(result.is_err());
        assert!(matches!(result, Err(crate::Error::Internal(_))));
    }
}
