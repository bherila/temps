use async_trait::async_trait;
use base64::Engine;
use sqlx::mysql::{MySqlArguments, MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, MySql, Row, TypeInfo};
use std::collections::{HashMap, HashSet};
use temps_query::{
    Capability, ContainerCapabilities, ContainerInfo, ContainerPath, ContainerType, DataError,
    DataRow, DataSource, DatasetSchema, EntityCountHint, EntityInfo, FieldDef, FieldType,
    Introspect, QueryOptions, QueryResult, QuerySchemaProvider, QueryStats, Queryable, Result,
};
use tracing::{debug, error};

pub struct MariaDbSource {
    pool: MySqlPool,
    database_name: String,
}

impl MariaDbSource {
    pub async fn connect(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        database: &str,
    ) -> Result<Self> {
        validate_identifier("database", database)?;

        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            urlencoding::encode(username),
            urlencoding::encode(password),
            host,
            port,
            urlencoding::encode(database)
        );

        debug!(
            "Connecting to MariaDB: {}@{}:{}/{}",
            username, host, port, database
        );

        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .map_err(|e| {
                DataError::ConnectionFailed(format!("MariaDB connection failed: {}", e))
            })?;

        Ok(Self {
            pool,
            database_name: database.to_string(),
        })
    }

    fn map_mysql_type(mysql_type: &str) -> FieldType {
        match mysql_type.to_ascii_lowercase().as_str() {
            "bool" | "boolean" => FieldType::Boolean,
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "year" => FieldType::Int32,
            "bigint" => FieldType::Int64,
            "float" => FieldType::Float32,
            "double" | "real" => FieldType::Float64,
            "decimal" | "numeric" => FieldType::String,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                FieldType::Bytes
            }
            "date" => FieldType::Date,
            "datetime" | "timestamp" | "time" => FieldType::Timestamp,
            "json" => FieldType::Json,
            _ => FieldType::String,
        }
    }

    fn row_to_datarow(row: &MySqlRow) -> Result<DataRow> {
        let mut data_row = HashMap::new();
        for (idx, column) in row.columns().iter().enumerate() {
            let value = Self::extract_value(row, idx)?;
            data_row.insert(column.name().to_string(), value);
        }
        Ok(data_row)
    }

    fn extract_value(row: &MySqlRow, idx: usize) -> Result<serde_json::Value> {
        let column = &row.columns()[idx];
        let type_name = column.type_info().name().to_ascii_lowercase();

        let value = match type_name.as_str() {
            "bool" | "boolean" => row
                .try_get::<Option<bool>, _>(idx)
                .ok()
                .flatten()
                .map(serde_json::Value::Bool)
                .unwrap_or(serde_json::Value::Null),
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "year" => row
                .try_get::<Option<i32>, _>(idx)
                .ok()
                .flatten()
                .map(|v| serde_json::Value::Number(v.into()))
                .unwrap_or(serde_json::Value::Null),
            "bigint" => row
                .try_get::<Option<i64>, _>(idx)
                .ok()
                .flatten()
                .map(|v| serde_json::Value::Number(v.into()))
                .or_else(|| {
                    row.try_get::<Option<u64>, _>(idx)
                        .ok()
                        .flatten()
                        .map(|v| serde_json::Value::Number(v.into()))
                })
                .unwrap_or(serde_json::Value::Null),
            "float" => row
                .try_get::<Option<f32>, _>(idx)
                .ok()
                .flatten()
                .and_then(|v| serde_json::Number::from_f64(v as f64))
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            "double" | "real" => row
                .try_get::<Option<f64>, _>(idx)
                .ok()
                .flatten()
                .and_then(serde_json::Number::from_f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            "json" => row
                .try_get::<Option<String>, _>(idx)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or(serde_json::Value::Null),
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => row
                .try_get::<Option<Vec<u8>>, _>(idx)
                .ok()
                .flatten()
                .map(|v| {
                    serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(v))
                })
                .unwrap_or(serde_json::Value::Null),
            _ => row
                .try_get::<Option<String>, _>(idx)
                .ok()
                .flatten()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        };

        Ok(value)
    }
}

#[async_trait]
impl DataSource for MariaDbSource {
    fn source_type(&self) -> &'static str {
        "mariadb"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Sql]
    }

    async fn list_containers(&self, path: &ContainerPath) -> Result<Vec<ContainerInfo>> {
        match path.depth() {
            0 => {
                let rows = sqlx::query(
                    r#"
                    SELECT SCHEMA_NAME
                    FROM information_schema.SCHEMATA
                    WHERE SCHEMA_NAME NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
                    ORDER BY SCHEMA_NAME
                    "#,
                )
                .fetch_all(&self.pool)
                .await
                .map_err(|e| DataError::QueryFailed(format!("Failed to list databases: {}", e)))?;

                Ok(rows
                    .iter()
                    .filter_map(|row| row.try_get::<String, _>("SCHEMA_NAME").ok())
                    .map(|name| ContainerInfo {
                        name,
                        container_type: ContainerType::Database,
                        capabilities: ContainerCapabilities {
                            can_contain_containers: false,
                            can_contain_entities: true,
                            child_container_type: None,
                            entity_type_label: Some("table".to_string()),
                            entity_count_hint: Some(EntityCountHint::Small),
                        },
                        metadata: HashMap::new(),
                    })
                    .collect())
            }
            _ => Err(DataError::InvalidQuery(format!(
                "MariaDB hierarchy only supports root/database levels. Path depth: {}",
                path.depth()
            ))),
        }
    }

    async fn get_container_info(&self, path: &ContainerPath) -> Result<ContainerInfo> {
        if path.depth() != 1 {
            return Err(DataError::InvalidQuery(format!(
                "get_container_info requires path depth 1 (database), got {}",
                path.depth()
            )));
        }

        let database_name = &path.segments[0];
        validate_identifier("database", database_name)?;

        let row = sqlx::query(
            r#"
            SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME
            FROM information_schema.SCHEMATA
            WHERE SCHEMA_NAME = ?
            "#,
        )
        .bind(database_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            DataError::QueryFailed(format!(
                "Failed to read database '{}': {}",
                database_name, e
            ))
        })?
        .ok_or_else(|| DataError::NotFound(format!("Database '{}' not found", database_name)))?;

        let name: String = row.try_get("SCHEMA_NAME").map_err(|e| {
            DataError::SerializationError(format!("Failed to read database name: {}", e))
        })?;
        let charset: Option<String> = row.try_get("DEFAULT_CHARACTER_SET_NAME").ok();
        let collation: Option<String> = row.try_get("DEFAULT_COLLATION_NAME").ok();

        let mut metadata = HashMap::new();
        if let Some(value) = charset {
            metadata.insert("charset".to_string(), serde_json::json!(value));
        }
        if let Some(value) = collation {
            metadata.insert("collation".to_string(), serde_json::json!(value));
        }

        Ok(ContainerInfo {
            name,
            container_type: ContainerType::Database,
            capabilities: ContainerCapabilities {
                can_contain_containers: false,
                can_contain_entities: true,
                child_container_type: None,
                entity_type_label: Some("table".to_string()),
                entity_count_hint: Some(EntityCountHint::Small),
            },
            metadata,
        })
    }

    async fn list_entities(&self, container_path: &ContainerPath) -> Result<Vec<EntityInfo>> {
        if container_path.depth() != 1 {
            return Err(DataError::InvalidQuery(format!(
                "list_entities requires path depth 1 (database), got {}",
                container_path.depth()
            )));
        }

        let database_name = &container_path.segments[0];
        validate_identifier("database", database_name)?;
        if database_name != &self.database_name {
            return Err(DataError::OperationNotSupported(format!(
                "Cannot list tables from database '{}' while connected to '{}'",
                database_name, self.database_name
            )));
        }

        let rows = sqlx::query(
            r#"
            SELECT TABLE_NAME, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH
            FROM information_schema.TABLES
            WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE'
            ORDER BY TABLE_NAME
            "#,
        )
        .bind(database_name)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            DataError::QueryFailed(format!(
                "Failed to list tables in database '{}': {}",
                database_name, e
            ))
        })?;

        Ok(rows
            .iter()
            .filter_map(|row| {
                let table_name = row.try_get::<String, _>("TABLE_NAME").ok()?;
                let table_rows = row
                    .try_get::<Option<u64>, _>("TABLE_ROWS")
                    .ok()
                    .flatten()
                    .and_then(|v| usize::try_from(v).ok());
                let data_length = row
                    .try_get::<Option<u64>, _>("DATA_LENGTH")
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                let index_length = row
                    .try_get::<Option<u64>, _>("INDEX_LENGTH")
                    .ok()
                    .flatten()
                    .unwrap_or(0);

                Some(EntityInfo {
                    namespace: database_name.clone(),
                    name: table_name,
                    entity_type: "table".to_string(),
                    row_count: table_rows,
                    size_bytes: Some(data_length.saturating_add(index_length)),
                    schema: None,
                    metadata: None,
                })
            })
            .collect())
    }

    async fn get_entity_info(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<EntityInfo> {
        if !self.entity_exists(container_path, entity_name).await? {
            return Err(DataError::NotFound(format!(
                "Table '{}.{}' not found",
                container_path, entity_name
            )));
        }

        let row_count = self.count(container_path, entity_name, None).await.ok();

        Ok(EntityInfo {
            namespace: container_path.segments[0].clone(),
            name: entity_name.to_string(),
            entity_type: "table".to_string(),
            row_count: row_count.and_then(|v| usize::try_from(v).ok()),
            size_bytes: None,
            schema: Some(self.get_schema(container_path, entity_name).await?),
            metadata: None,
        })
    }

    async fn get_schema(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<DatasetSchema> {
        if container_path.depth() != 1 {
            return Err(DataError::InvalidQuery(format!(
                "get_schema requires path depth 1 (database), got {}",
                container_path.depth()
            )));
        }

        let database_name = &container_path.segments[0];
        validate_identifier("database", database_name)?;
        validate_identifier("table", entity_name)?;
        if database_name != &self.database_name {
            return Err(DataError::OperationNotSupported(format!(
                "Cannot get schema from database '{}' while connected to '{}'",
                database_name, self.database_name
            )));
        }

        let rows = sqlx::query(
            r#"
            SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE, COLUMN_KEY
            FROM information_schema.COLUMNS
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
            ORDER BY ORDINAL_POSITION
            "#,
        )
        .bind(database_name)
        .bind(entity_name)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            DataError::SchemaError(format!(
                "Failed to get schema for table '{}.{}': {}",
                database_name, entity_name, e
            ))
        })?;

        if rows.is_empty() {
            return Err(DataError::NotFound(format!(
                "Table '{}.{}' not found",
                database_name, entity_name
            )));
        }

        let mut primary_key = Vec::new();
        let mut fields = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.try_get("COLUMN_NAME").map_err(|e| {
                DataError::SchemaError(format!("Failed to read column name: {}", e))
            })?;
            let data_type: String = row.try_get("DATA_TYPE").map_err(|e| {
                DataError::SchemaError(format!("Failed to read column type: {}", e))
            })?;
            let is_nullable: String = row.try_get("IS_NULLABLE").unwrap_or_else(|_| "YES".into());
            let column_key: Option<String> = row.try_get("COLUMN_KEY").ok();
            if column_key.as_deref() == Some("PRI") {
                primary_key.push(name.clone());
            }

            fields.push(FieldDef {
                name,
                field_type: Self::map_mysql_type(&data_type),
                nullable: is_nullable == "YES",
                description: None,
            });
        }

        Ok(DatasetSchema {
            fields,
            partitions: None,
            primary_key: if primary_key.is_empty() {
                None
            } else {
                Some(primary_key)
            },
        })
    }

    async fn close(&self) -> Result<()> {
        self.pool.close().await;
        Ok(())
    }
}

#[async_trait]
impl Introspect for MariaDbSource {
    async fn inspect_fields(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<Vec<FieldDef>> {
        Ok(self.get_schema(container_path, entity_name).await?.fields)
    }

    async fn field_exists(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        field: &str,
    ) -> Result<bool> {
        validate_identifier("field", field)?;
        let schema = self.get_schema(container_path, entity_name).await?;
        Ok(schema.fields.iter().any(|f| f.name == field))
    }

    async fn get_field_type(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        field: &str,
    ) -> Result<FieldType> {
        validate_identifier("field", field)?;
        let schema = self.get_schema(container_path, entity_name).await?;
        schema
            .fields
            .into_iter()
            .find(|f| f.name == field)
            .map(|f| f.field_type)
            .ok_or_else(|| {
                DataError::NotFound(format!(
                    "Field '{}' not found in table '{}'",
                    field, entity_name
                ))
            })
    }
}

#[async_trait]
impl Queryable for MariaDbSource {
    async fn query(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        filters: Option<serde_json::Value>,
        options: QueryOptions,
    ) -> Result<QueryResult> {
        let database_name = database_from_path(container_path, &self.database_name)?;
        validate_identifier("table", entity_name)?;

        let start = std::time::Instant::now();
        let schema = self.get_schema(container_path, entity_name).await?;
        let allowed_fields = schema_field_names(&schema);
        let mut sql = format!(
            "SELECT * FROM {}.{}",
            quote_identifier(database_name),
            quote_identifier(entity_name)
        );
        let filter = build_filter_clause(filters.as_ref(), &allowed_fields)?;

        if let Some(filter) = &filter {
            sql.push_str(" WHERE ");
            sql.push_str(&filter.sql);
        }

        if let Some(sort_by) = &options.sort_by {
            let sort_field = normalize_sort_field(sort_by)?;
            let sort_order = match options.sort_order.as_deref() {
                Some("desc") | Some("DESC") => "DESC",
                _ => "ASC",
            };
            sql.push_str(&format!(
                " ORDER BY {} {}",
                quote_identifier(sort_field),
                sort_order
            ));
        }

        let limit = options.limit.unwrap_or(100);
        let offset = options.offset.unwrap_or(0);
        sql.push_str(" LIMIT ? OFFSET ?");

        debug!("Executing MariaDB query: {}", sql);

        let mut query = sqlx::query(&sql);
        if let Some(filter) = &filter {
            query = bind_filter_params(query, &filter.params);
        }

        let rows = query
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                error!("MariaDB query failed: {}", e);
                DataError::QueryFailed(format!("{}\n\nQuery: {}", e, sql))
            })?;

        let data_rows: Result<Vec<DataRow>> = rows.iter().map(Self::row_to_datarow).collect();
        let data_rows = data_rows?;
        let row_count = data_rows.len();

        Ok(QueryResult {
            schema,
            rows: data_rows,
            stats: QueryStats {
                row_count,
                total_rows: None,
                execution_ms: start.elapsed().as_millis() as u64,
                has_more: row_count >= limit,
                next_cursor: None,
            },
        })
    }

    async fn count(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        filters: Option<serde_json::Value>,
    ) -> Result<u64> {
        let database_name = database_from_path(container_path, &self.database_name)?;
        validate_identifier("table", entity_name)?;

        let schema = self.get_schema(container_path, entity_name).await?;
        let allowed_fields = schema_field_names(&schema);
        let mut sql = format!(
            "SELECT COUNT(*) AS row_count FROM {}.{}",
            quote_identifier(database_name),
            quote_identifier(entity_name)
        );
        let filter = build_filter_clause(filters.as_ref(), &allowed_fields)?;

        if let Some(filter) = &filter {
            sql.push_str(" WHERE ");
            sql.push_str(&filter.sql);
        }

        let mut query = sqlx::query(&sql);
        if let Some(filter) = &filter {
            query = bind_filter_params(query, &filter.params);
        }

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DataError::QueryFailed(format!("Count query failed: {}", e)))?;

        let count = row
            .try_get::<i64, _>("row_count")
            .or_else(|_| row.try_get::<u64, _>("row_count").map(|v| v as i64))
            .map_err(|e| DataError::SerializationError(format!("Invalid count result: {}", e)))?;

        Ok(count.max(0) as u64)
    }

    async fn entity_exists(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<bool> {
        let database_name = database_from_path(container_path, &self.database_name)?;
        validate_identifier("table", entity_name)?;

        let row = sqlx::query(
            r#"
            SELECT COUNT(*) AS table_count
            FROM information_schema.TABLES
            WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE = 'BASE TABLE'
            "#,
        )
        .bind(database_name)
        .bind(entity_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DataError::QueryFailed(format!("Entity existence check failed: {}", e)))?;

        let count: i64 = row.try_get("table_count").unwrap_or(0);
        Ok(count > 0)
    }
}

impl QuerySchemaProvider for MariaDbSource {
    fn get_filter_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "title": "MariaDB Query Filters",
            "description": "Filter data with structured, parameterized conditions",
            "properties": {
                "logic": {
                    "type": "string",
                    "title": "Condition Logic",
                    "description": "How multiple conditions are combined",
                    "enum": ["and", "or"],
                    "default": "and"
                },
                "conditions": {
                    "type": "array",
                    "title": "Conditions",
                    "items": {
                        "type": "object",
                        "required": ["field", "op", "value"],
                        "properties": {
                            "field": {
                                "type": "string",
                                "title": "Column"
                            },
                            "op": {
                                "type": "string",
                                "title": "Operator",
                                "enum": ["eq", "ne", "gt", "gte", "lt", "lte", "like", "in"],
                                "default": "eq"
                            },
                            "value": {
                                "title": "Value",
                                "description": "Scalar value for comparisons, or an array when op is in"
                            }
                        },
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        })
    }

    fn get_sort_schema(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<serde_json::Value> {
        let schema_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { self.get_schema(container_path, entity_name).await })
        });

        let schema = schema_result?;
        let field_names: Vec<String> = schema.fields.iter().map(|f| f.name.clone()).collect();

        Ok(serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "title": "Sort Options",
            "description": "Specify how to sort query results",
            "properties": {
                "sort_by": {
                    "type": "string",
                    "title": "Sort By",
                    "description": "Field to sort by",
                    "enum": field_names,
                    "x-ui-widget": "select"
                },
                "sort_order": {
                    "type": "string",
                    "title": "Sort Order",
                    "description": "Sort direction",
                    "enum": ["asc", "desc"],
                    "default": "asc",
                    "x-ui-widget": "select"
                }
            }
        }))
    }
}

fn database_from_path<'a>(
    container_path: &'a ContainerPath,
    connected_database: &'a str,
) -> Result<&'a str> {
    if container_path.depth() != 1 {
        return Err(DataError::InvalidQuery(format!(
            "MariaDB table operations require path depth 1 (database), got {}",
            container_path.depth()
        )));
    }

    let database_name = container_path.segments[0].as_str();
    validate_identifier("database", database_name)?;
    if database_name != connected_database {
        return Err(DataError::OperationNotSupported(format!(
            "Cannot query database '{}' while connected to '{}'",
            database_name, connected_database
        )));
    }
    Ok(database_name)
}

fn quote_identifier(value: &str) -> String {
    format!("`{}`", value)
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(DataError::InvalidQuery(format!(
            "{} cannot be empty",
            label
        )));
    }
    if value.len() > 63 {
        return Err(DataError::InvalidQuery(format!(
            "{} '{}' exceeds 63 character limit",
            label, value
        )));
    }

    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(DataError::InvalidQuery(format!(
            "{} cannot be empty",
            label
        )));
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(DataError::InvalidQuery(format!(
            "{} '{}' must start with a letter or underscore",
            label, value
        )));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(DataError::InvalidQuery(format!(
            "{} '{}' contains invalid characters. Only ASCII letters, digits, and underscores are allowed",
            label, value
        )));
    }

    Ok(())
}

fn normalize_sort_field(sort_by: &str) -> Result<&str> {
    let trimmed = sort_by.trim().trim_start_matches('/');
    validate_identifier("sort field", trimmed)?;
    Ok(trimmed)
}

#[derive(Debug, Clone, PartialEq)]
enum FilterParam {
    Bool(bool),
    F64(f64),
    I64(i64),
    Null,
    String(String),
}

#[derive(Debug, Clone, PartialEq)]
struct FilterClause {
    sql: String,
    params: Vec<FilterParam>,
}

fn schema_field_names(schema: &DatasetSchema) -> HashSet<String> {
    schema
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect()
}

fn build_filter_clause(
    filters: Option<&serde_json::Value>,
    allowed_fields: &HashSet<String>,
) -> Result<Option<FilterClause>> {
    let Some(filters) = filters else {
        return Ok(None);
    };

    if filters.get("where").is_some() {
        return Err(DataError::InvalidQuery(
            "Raw SQL WHERE filters are not supported for MariaDB; use structured conditions"
                .to_string(),
        ));
    }

    let Some(conditions) = filters.get("conditions") else {
        return Ok(None);
    };
    let conditions = conditions.as_array().ok_or_else(|| {
        DataError::InvalidQuery("MariaDB filter 'conditions' must be an array".to_string())
    })?;
    if conditions.is_empty() {
        return Ok(None);
    }

    let logic = filters
        .get("logic")
        .and_then(|value| value.as_str())
        .unwrap_or("and")
        .to_ascii_lowercase();
    let joiner = match logic.as_str() {
        "and" => " AND ",
        "or" => " OR ",
        _ => {
            return Err(DataError::InvalidQuery(
                "MariaDB filter 'logic' must be 'and' or 'or'".to_string(),
            ));
        }
    };

    let mut sql_parts = Vec::with_capacity(conditions.len());
    let mut params = Vec::new();

    for condition in conditions {
        let field = condition
            .get("field")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                DataError::InvalidQuery(
                    "MariaDB filter condition requires a string 'field'".to_string(),
                )
            })?;
        validate_identifier("filter field", field)?;
        if !allowed_fields.contains(field) {
            return Err(DataError::InvalidQuery(format!(
                "Filter field '{}' does not exist on the selected MariaDB table",
                field
            )));
        }

        let op = condition
            .get("op")
            .and_then(|value| value.as_str())
            .unwrap_or("eq")
            .to_ascii_lowercase();
        let value = condition.get("value").ok_or_else(|| {
            DataError::InvalidQuery("MariaDB filter condition requires a 'value'".to_string())
        })?;
        let ident = quote_identifier(field);

        match op.as_str() {
            "eq" | "=" => {
                if value.is_null() {
                    sql_parts.push(format!("{ident} IS NULL"));
                } else {
                    sql_parts.push(format!("{ident} = ?"));
                    params.push(filter_param(value)?);
                }
            }
            "ne" | "!=" | "<>" => {
                if value.is_null() {
                    sql_parts.push(format!("{ident} IS NOT NULL"));
                } else {
                    sql_parts.push(format!("{ident} <> ?"));
                    params.push(filter_param(value)?);
                }
            }
            "gt" | ">" | "gte" | ">=" | "lt" | "<" | "lte" | "<=" | "like" => {
                let sql_op = match op.as_str() {
                    "gt" | ">" => ">",
                    "gte" | ">=" => ">=",
                    "lt" | "<" => "<",
                    "lte" | "<=" => "<=",
                    "like" => "LIKE",
                    _ => unreachable!(),
                };
                if value.is_null() {
                    return Err(DataError::InvalidQuery(format!(
                        "MariaDB filter operator '{}' cannot compare against null",
                        op
                    )));
                }
                sql_parts.push(format!("{ident} {sql_op} ?"));
                params.push(filter_param(value)?);
            }
            "in" => {
                let values = value.as_array().ok_or_else(|| {
                    DataError::InvalidQuery(
                        "MariaDB 'in' filter value must be an array".to_string(),
                    )
                })?;
                if values.is_empty() {
                    return Err(DataError::InvalidQuery(
                        "MariaDB 'in' filter requires at least one value".to_string(),
                    ));
                }
                sql_parts.push(format!(
                    "{ident} IN ({})",
                    vec!["?"; values.len()].join(", ")
                ));
                for value in values {
                    if value.is_null() {
                        return Err(DataError::InvalidQuery(
                            "MariaDB 'in' filter values cannot be null".to_string(),
                        ));
                    }
                    params.push(filter_param(value)?);
                }
            }
            _ => {
                return Err(DataError::InvalidQuery(format!(
                    "Unsupported MariaDB filter operator '{}'",
                    op
                )));
            }
        }
    }

    Ok(Some(FilterClause {
        sql: sql_parts.join(joiner),
        params,
    }))
}

fn filter_param(value: &serde_json::Value) -> Result<FilterParam> {
    if let Some(value) = value.as_bool() {
        return Ok(FilterParam::Bool(value));
    }
    if let Some(value) = value.as_i64() {
        return Ok(FilterParam::I64(value));
    }
    if let Some(value) = value.as_f64() {
        return Ok(FilterParam::F64(value));
    }
    if let Some(value) = value.as_str() {
        return Ok(FilterParam::String(value.to_string()));
    }
    if value.is_null() {
        return Ok(FilterParam::Null);
    }
    Err(DataError::InvalidQuery(
        "MariaDB filter values must be strings, numbers, booleans, or null".to_string(),
    ))
}

fn bind_filter_params<'q>(
    mut query: sqlx::query::Query<'q, MySql, MySqlArguments>,
    params: &'q [FilterParam],
) -> sqlx::query::Query<'q, MySql, MySqlArguments> {
    for param in params {
        query = match param {
            FilterParam::Bool(value) => query.bind(*value),
            FilterParam::F64(value) => query.bind(*value),
            FilterParam::I64(value) => query.bind(*value),
            FilterParam::Null => query.bind(None::<String>),
            FilterParam::String(value) => query.bind(value),
        };
    }
    query
}

pub(crate) fn is_mariadb_compatible_image(image: &str) -> bool {
    let lower = image.to_ascii_lowercase();
    lower.contains("mariadb") || lower.split(['/', ':']).any(|part| part == "mysql")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mysql_types() {
        assert_eq!(MariaDbSource::map_mysql_type("int"), FieldType::Int32);
        assert_eq!(MariaDbSource::map_mysql_type("bigint"), FieldType::Int64);
        assert_eq!(MariaDbSource::map_mysql_type("varchar"), FieldType::String);
        assert_eq!(
            MariaDbSource::map_mysql_type("datetime"),
            FieldType::Timestamp
        );
        assert_eq!(MariaDbSource::map_mysql_type("json"), FieldType::Json);
        assert_eq!(MariaDbSource::map_mysql_type("blob"), FieldType::Bytes);
    }

    #[test]
    fn validates_identifiers() {
        assert!(validate_identifier("database", "app_prod").is_ok());
        assert!(validate_identifier("database", "1bad").is_err());
        assert!(validate_identifier("database", "bad-name").is_err());
        assert!(validate_identifier("database", "bad`name").is_err());
    }

    #[test]
    fn builds_structured_filter_clause_with_bound_params() {
        let allowed_fields =
            HashSet::from(["status".to_string(), "age".to_string(), "id".to_string()]);
        let filter = serde_json::json!({
            "logic": "and",
            "conditions": [
                {"field": "status", "op": "eq", "value": "active"},
                {"field": "age", "op": "gte", "value": 18},
                {"field": "id", "op": "in", "value": [1, 2, 3]}
            ]
        });

        let clause = build_filter_clause(Some(&filter), &allowed_fields)
            .expect("structured filter should be valid")
            .expect("filter clause should be present");

        assert_eq!(
            clause.sql,
            "`status` = ? AND `age` >= ? AND `id` IN (?, ?, ?)"
        );
        assert_eq!(
            clause.params,
            vec![
                FilterParam::String("active".to_string()),
                FilterParam::I64(18),
                FilterParam::I64(1),
                FilterParam::I64(2),
                FilterParam::I64(3),
            ]
        );
    }

    #[test]
    fn rejects_raw_where_and_unknown_filter_fields() {
        let allowed_fields = HashSet::from(["status".to_string()]);
        let raw_filter = serde_json::json!({
            "where": "EXISTS(SELECT 1 FROM other_db.secret_table)"
        });
        let unknown_field_filter = serde_json::json!({
            "conditions": [{"field": "other_table_secret", "op": "eq", "value": "x"}]
        });

        assert!(build_filter_clause(Some(&raw_filter), &allowed_fields).is_err());
        assert!(build_filter_clause(Some(&unknown_field_filter), &allowed_fields).is_err());
    }

    #[test]
    fn detects_mariadb_compatible_images() {
        assert!(is_mariadb_compatible_image("mariadb:lts"));
        assert!(is_mariadb_compatible_image("library/mysql:8.4"));
        assert!(is_mariadb_compatible_image("mysql:8"));
        assert!(!is_mariadb_compatible_image("postgres:18"));
    }
}
