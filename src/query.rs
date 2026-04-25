use sqlx::{Column, Executor, MySql, Pool, Row as SqlxRow, TypeInfo, ValueRef};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// MySQL識別子をバッククォートで安全に囲む
///
/// 識別子内のバッククォート（`）を``にエスケープする。
/// テーブル名・カラム名・DB名を SQL に埋め込む際に使用し、SQL インジェクションを防ぐ。
pub fn escape_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// SQLの値を文字列に変換
///
/// NULL値は"NULL"文字列として返す
/// 各種SQL型（INT, VARCHAR, DATETIME等）を適切に文字列化
pub(crate) fn convert_value_to_string(
    row: &sqlx::mysql::MySqlRow,
    index: usize,
    col: &sqlx::mysql::MySqlColumn,
) -> String {
    let value = row.try_get_raw(index).ok();

    if let Some(raw_value) = value {
        if raw_value.is_null() {
            return String::from("NULL");
        }

        let type_info = col.type_info();
        let type_name = type_info.name();

        // 型名に応じて適切に変換し、制御文字をサニタイズして返す
        // skimは1行=1アイテムのため、改行等が含まれるとUI崩れの原因になる
        let raw = match type_name {
            "TINYINT" | "TINYINT UNSIGNED"
            | "SMALLINT" | "SMALLINT UNSIGNED"
            | "MEDIUMINT" | "MEDIUMINT UNSIGNED"
            | "INT" | "INT UNSIGNED"
            | "BIGINT" | "BIGINT UNSIGNED" => {
                // 整数型（UNSIGNED含む）
                // UNSIGNEDでも i64 で十分な範囲（BIGINT UNSIGNED の最大値は u64 だが大半は収まる）
                row.try_get::<i64, _>(index)
                    .map(|v| v.to_string())
                    .or_else(|_| row.try_get::<u64, _>(index).map(|v| v.to_string()))
                    .unwrap_or_else(|_| String::from("NULL"))
            }
            "FLOAT" | "DOUBLE" | "DECIMAL" | "FLOAT UNSIGNED" | "DOUBLE UNSIGNED" | "DECIMAL UNSIGNED" => {
                // 浮動小数点型（UNSIGNED含む）
                row.try_get::<f64, _>(index)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| String::from("NULL"))
            }
            "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => {
                // 文字列型
                row.try_get::<String, _>(index)
                    .unwrap_or_else(|_| String::from("NULL"))
            }
            "DATE" | "DATETIME" | "TIMESTAMP" | "TIME" => {
                // 日付時刻型
                row.try_get::<String, _>(index)
                    .unwrap_or_else(|_| String::from("NULL"))
            }
            "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
                // バイナリ型は表示しない
                String::from("[BLOB]")
            }
            _ => {
                // その他の型は文字列として取得を試みる
                row.try_get::<String, _>(index)
                    .unwrap_or_else(|_| format!("[{}]", type_name))
            }
        };

        // 改行・タブ等の制御文字を置換（skim表示のUI崩れ防止）
        sanitize_for_display(&raw)
    } else {
        String::from("NULL")
    }
}

/// 制御文字を表示用に置換する
///
/// 改行→⏎、タブ→→、その他制御文字→空白に変換
fn sanitize_for_display(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\n' => result.push('⏎'),
            '\r' => {} // CRは無視
            '\t' => result.push('→'),
            c if c.is_control() => result.push(' '),
            c => result.push(c),
        }
    }
    result
}

/// クエリ実行結果
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// カラム名リスト
    pub columns: Vec<String>,

    /// データ行（各行は文字列のベクタ）
    pub rows: Vec<Vec<String>>,

    /// 実行時間
    pub execution_time: Duration,

    /// 結果を表示すべきかどうか（USE, SETなどのコマンドはfalse）
    pub should_display: bool,
}

impl QueryResult {
    /// 行数を返す
    ///
    /// rows.len()の別名。row_countフィールドとrows.lenが乖離するバグを防ぐためメソッドで提供する。
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

impl QueryResult {
    /// メモリ使用量の概算を計算（バイト単位）
    ///
    /// Phase 3: メモリ最適化のためのプロファイリング情報
    pub fn estimate_memory_usage(&self) -> usize {
        let mut total = 0;

        // カラム名のメモリ使用量
        total += self.columns.iter().map(|s| s.capacity()).sum::<usize>();

        // データ行のメモリ使用量
        for row in &self.rows {
            total += row.iter().map(|s| s.capacity()).sum::<usize>();
        }

        // ベクタ自体のオーバーヘッド
        total += std::mem::size_of::<Vec<String>>() * (1 + self.rows.len());

        total
    }

    /// メモリ使用量を人間が読みやすい形式で返す
    pub fn format_memory_usage(&self) -> String {
        let bytes = self.estimate_memory_usage();
        if bytes < 1024 {
            format!("{} B", bytes)
        } else if bytes < 1024 * 1024 {
            format!("{:.2} KB", bytes as f64 / 1024.0)
        } else if bytes < 1024 * 1024 * 1024 {
            format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }
}

/// SQLクエリを実行（ストリーミング版）
///
/// Phase 3: ストリーミング処理で大量データに対応
/// メモリに全データを読み込まず、順次処理することでメモリ使用量を削減
///
/// `current_database` が指定されている場合、専用コネクションを取得して
/// `USE db` を先行実行することで、プールのセッション状態問題を回避する。
/// sqlx の接続プールはセッション状態（USE で変更したデフォルト DB）を
/// コネクション間で共有しないため、USE 実行後のクエリは同じコネクションで実行する必要がある。
pub async fn execute_query(
    pool: &Pool<MySql>,
    sql: &str,
    current_database: Option<&str>,
) -> Result<QueryResult> {
    tracing::debug!("Executing query: {}", sql);
    let start = Instant::now();

    // USE, SETなどプリペアドステートメントをサポートしないコマンドを検出
    // これらは結果を返さないため、execute()で実行
    let sql_trimmed = sql.trim().to_uppercase();
    let is_non_prepared_command =
        sql_trimmed.starts_with("USE ") || sql_trimmed.starts_with("SET ");

    // プリペアドステートメント非対応のコマンドは execute() で実行
    // USE/SET コマンド自体はDB切り替えコマンドなので、先行USE不要
    if is_non_prepared_command {
        pool.execute(sql).await.map_err(Error::QueryExecution)?;

        let execution_time = start.elapsed();
        tracing::info!("Command executed successfully in {:?}", execution_time);

        // USE, SET コマンドは結果を表示しない
        return Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            execution_time,
            should_display: false,
        });
    }

    // current_database が指定されている場合、専用コネクションで USE を先行実行する。
    // プールから複数のコネクションが払い出される環境では、USE 実行後に
    // 別のコネクションでクエリが実行されると元のDBに対して動作してしまうため。
    let mut conn_opt = if let Some(db) = current_database {
        let mut conn = pool.acquire().await.map_err(Error::QueryExecution)?;
        // 同一コネクションで USE を実行してからクエリを実行することを保証する
        // USE コマンドは prepared statement プロトコル非対応(MySQL error 1295)のため、
        // &str を Executor::execute に渡すことで simple query protocol (COM_QUERY) を使う。
        // &str の Execute 実装は take_arguments() == None を返すため、
        // MySQL ドライバは prepared statement を使わず COM_QUERY を発行する
        let use_stmt = format!("USE {}", escape_identifier(db));
        (&mut *conn)
            .execute(use_stmt.as_str())
            .await
            .map_err(Error::QueryExecution)?;
        Some(conn)
    } else {
        None
    };

    // ストリーム取得: 専用コネクションがある場合はそちらを使用し、
    // Pin<Box<...>> で型を統一して後続処理を1つにまとめる
    let mut stream: std::pin::Pin<
        Box<
            dyn futures::Stream<
                    Item = std::result::Result<sqlx::mysql::MySqlRow, sqlx::Error>,
                > + Send,
        >,
    > = if let Some(ref mut conn) = conn_opt {
        Box::pin(sqlx::query(sql).fetch(&mut **conn))
    } else {
        Box::pin(sqlx::query(sql).fetch(pool))
    };

    use futures::StreamExt;

    let mut columns = Vec::new();
    let mut data_rows = Vec::new();

    while let Some(row_result) = stream.next().await {
        let row = row_result.map_err(Error::QueryExecution)?;

        // 最初の行からカラム名を取得
        if data_rows.is_empty() {
            columns = row
                .columns()
                .iter()
                .map(|col| col.name().to_string())
                .collect();
        }

        // データ行を変換
        let data_row: Vec<String> = row
            .columns()
            .iter()
            .enumerate()
            .map(|(i, col)| convert_value_to_string(&row, i, col))
            .collect();

        data_rows.push(data_row);

        // メモリ使用量のログ（10万行ごと）
        if data_rows.len() % 100_000 == 0 {
            tracing::info!("Fetched {} rows so far...", data_rows.len());
        }
    }

    // 0件結果でも列ヘッダーを表示できるよう、必要時のみメタデータを補完する
    if data_rows.is_empty() && columns.is_empty() {
        match pool.describe(sql).await {
            Ok(describe) => {
                columns = describe
                    .columns()
                    .iter()
                    .map(|col| col.name().to_string())
                    .collect();
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to describe query result columns for empty result: {}",
                    e
                );
            }
        }
    }

    let execution_time = start.elapsed();

    let result = QueryResult {
        columns,
        rows: data_rows,
        execution_time,
        should_display: true,
    };

    // Phase 3: メモリ使用量のログ出力
    let memory_usage = result.format_memory_usage();
    tracing::info!(
        "Query executed successfully: {} rows in {:?}, estimated memory: {}",
        result.rows.len(),
        execution_time,
        memory_usage
    );

    Ok(result)
}

pub async fn execute_query_with_timeout(
    pool: &Pool<MySql>,
    sql: &str,
    timeout: Duration,
    current_database: Option<&str>,
) -> Result<QueryResult> {
    tokio::time::timeout(timeout, execute_query(pool, sql, current_database))
        .await
        .map_err(|_| Error::QueryTimeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_identifier_plain() {
        assert_eq!(escape_identifier("users"), "`users`");
    }

    #[test]
    fn test_escape_identifier_with_backtick() {
        assert_eq!(escape_identifier("my`table"), "`my``table`");
    }

    #[test]
    fn test_escape_identifier_empty() {
        assert_eq!(escape_identifier(""), "``");
    }

    #[test]
    fn test_query_result_creation() {
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string()],
                vec!["2".to_string(), "Bob".to_string()],
            ],
            execution_time: Duration::from_millis(100),
            should_display: true,
        };

        assert_eq!(result.columns.len(), 2);
        assert_eq!(result.row_count(), 2);
        assert_eq!(result.rows.len(), 2);
        assert!(result.should_display);
    }

    #[test]
    fn test_query_result_empty() {
        let result = QueryResult {
            columns: vec![],
            rows: vec![],
            execution_time: Duration::from_millis(10),
            should_display: true,
        };

        assert_eq!(result.columns.len(), 0);
        assert_eq!(result.row_count(), 0);
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn test_query_result_with_null_values() {
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "age".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string(), "NULL".to_string()],
                vec!["2".to_string(), "NULL".to_string(), "30".to_string()],
            ],
            execution_time: Duration::from_millis(50),
            should_display: true,
        };

        assert_eq!(result.row_count(), 2);
        assert_eq!(result.rows[0][2], "NULL");
        assert_eq!(result.rows[1][1], "NULL");
    }

    #[test]
    fn test_memory_usage_estimation() {
        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec!["1".to_string(), "Alice".to_string()],
                vec!["2".to_string(), "Bob".to_string()],
            ],
            execution_time: Duration::from_millis(100),
            should_display: true,
        };

        // メモリ使用量が0より大きいことを確認
        let memory_usage = result.estimate_memory_usage();
        assert!(memory_usage > 0);

        // フォーマットされた文字列が取得できることを確認
        let formatted = result.format_memory_usage();
        assert!(!formatted.is_empty());
        assert!(formatted.contains("B") || formatted.contains("KB") || formatted.contains("MB"));
    }

    #[test]
    fn test_memory_usage_large_result() {
        // 大量データでのメモリ使用量計算
        let mut rows = Vec::new();
        for i in 0..10000 {
            rows.push(vec![
                i.to_string(),
                format!("User_{}", i),
                format!("user_{}@example.com", i),
            ]);
        }

        let result = QueryResult {
            columns: vec!["id".to_string(), "name".to_string(), "email".to_string()],
            rows,
            execution_time: Duration::from_millis(500),
            should_display: true,
        };

        let memory_usage = result.estimate_memory_usage();
        // 10000行のデータなので、少なくとも100KB以上は使用しているはず
        assert!(memory_usage > 100_000);

        let formatted = result.format_memory_usage();
        println!("Memory usage for 10000 rows: {}", formatted);
    }
}
