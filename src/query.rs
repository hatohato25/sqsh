use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
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
        //
        // type_name は MySQL サーバーから "DATETIME(6)" のように精度付きで返される場合があるため、
        // 完全一致ではなく starts_with / contains で判定する。
        let raw = if type_name.starts_with("TINYINT")
            || type_name.starts_with("SMALLINT")
            || type_name.starts_with("MEDIUMINT")
            || type_name.starts_with("INT")
            || type_name.starts_with("BIGINT")
            || type_name.eq_ignore_ascii_case("BOOLEAN")
            || type_name.eq_ignore_ascii_case("BOOL")
        {
            // 整数型（UNSIGNED含む、精度付き含む）
            // MySQL の BOOLEAN/BOOL は内部的に TINYINT(1) であり、sqlx が返す type_name が
            // "BOOLEAN" になる場合があるため、整数型と同じブランチで i64 としてデコードする。
            // UNSIGNEDでも i64 で十分な範囲（BIGINT UNSIGNED の最大値は u64 だが大半は収まる）
            row.try_get::<i64, _>(index)
                .map(|v| v.to_string())
                .or_else(|_| row.try_get::<u64, _>(index).map(|v| v.to_string()))
                .unwrap_or_else(|_| String::from("NULL"))
        } else if type_name.starts_with("FLOAT") || type_name.starts_with("DOUBLE") {
            // 浮動小数点型（UNSIGNED含む、精度付き含む）
            row.try_get::<f64, _>(index)
                .map(|v| v.to_string())
                .unwrap_or_else(|_| String::from("NULL"))
        } else if type_name.starts_with("DECIMAL") || type_name.starts_with("NUMERIC") {
            // DECIMAL/NUMERIC型: MySQLでは NUMERIC は DECIMAL の同義語。
            // sqlx の String::compatible() は DECIMAL カラムタイプ(NewDecimal/Decimal)を拒否するため、
            // try_get::<String, _>() は型不一致エラーになる。
            // DECIMAL はバイナリプロトコルで length-encoded な UTF-8 テキストとして格納されるため、
            // try_get_unchecked で型チェックをバイパスして String としてデコードできる。
            // sqlx の String::compatible() は DECIMAL/NEWDECIMAL 型を拒否するため、
            // try_get では型チェックで弾かれる。try_get_unchecked で型チェックをバイパスして
            // バイナリプロトコルの length-encoded UTF-8 テキストとして直接デコードする。
            row.try_get_unchecked::<String, _>(index)
                .unwrap_or_else(|_| {
                    row.try_get_unchecked::<f64, _>(index)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|_| String::from("NULL"))
                })
        } else if type_name == "VARCHAR"
            || type_name == "CHAR"
            || type_name.starts_with("TEXT")
            || type_name == "TINYTEXT"
            || type_name == "MEDIUMTEXT"
            || type_name == "LONGTEXT"
        {
            // 文字列型
            row.try_get::<String, _>(index)
                .unwrap_or_else(|_| String::from("NULL"))
        } else if type_name.starts_with("DATETIME") {
            // DATETIME型（"DATETIME(6)" 等の精度付きも含む）
            // NaiveDateTime → Option<NaiveDateTime> → String の順でフォールバックする。
            // nullable カラムでは Option<NaiveDateTime> が要求される場合があり、
            // text protocol では文字列として返ってくることもある。
            // 注: "DATE" より前にチェックすること（starts_with("DATE") が DATETIME にもマッチするため）
            decode_naive_datetime(row, index)
        } else if type_name.starts_with("TIMESTAMP") {
            // TIMESTAMP型: MySQL の TIMESTAMP は timezone-aware なため DateTime<Utc> でデコードする。
            // NaiveDateTime では型不一致エラー（SQL type TIMESTAMP vs Rust type DATETIME）が発生する。
            decode_timestamp(row, index)
        } else if type_name.starts_with("DATE") {
            // DATE型（精度付きも含む）
            decode_naive_date(row, index)
        } else if type_name.starts_with("TIME") {
            // TIME型（精度付きも含む）
            decode_naive_time(row, index)
        } else if type_name == "YEAR" {
            // YEAR型: MySQL の YEAR 型は 1901-2155 の範囲で i16/u16 として返ってくる
            row.try_get::<i16, _>(index)
                .map(|v| v.to_string())
                .or_else(|_| row.try_get::<u16, _>(index).map(|v| v.to_string()))
                .unwrap_or_else(|_| String::from("NULL"))
        } else if type_name.eq_ignore_ascii_case("JSON") {
            // JSON型: sqlx の "json" feature を有効にすることで serde_json::Value として
            // デコード可能になる。binary protocol での JSON カラムは String や Vec<u8> では
            // 型不一致エラーになるが、serde_json::Value は sqlx の JSON feature が
            // 適切なデコードを提供するため正常に取得できる。
            // 大文字小文字を無視して比較するのは MySQL サーバーの返す type_name が
            // "JSON" / "Json" / "json" 等、環境により異なる場合があるため。
            row.try_get::<serde_json::Value, _>(index)
                .map(|v| v.to_string())
                .unwrap_or_else(|_| String::from("NULL"))
        } else if type_name == "BLOB"
            || type_name == "TINYBLOB"
            || type_name == "MEDIUMBLOB"
            || type_name == "LONGBLOB"
            || type_name == "BINARY"
            || type_name.starts_with("VARBINARY")
        {
            // バイナリ型は表示しない
            String::from("[BLOB]")
        } else {
            // 未知の型は String として取得する
            row.try_get::<String, _>(index)
                .unwrap_or_else(|_| format!("[{}]", type_name))
        };

        // 改行・タブ等の制御文字を置換（skim表示のUI崩れ防止）
        sanitize_for_display(&raw)
    } else {
        String::from("NULL")
    }
}

/// DATETIME カラムを文字列に変換する
///
/// NaiveDateTime → Option<NaiveDateTime> → String の順にフォールバックする。
/// TIMESTAMP は timezone-aware のため decode_timestamp を使うこと。
fn decode_naive_datetime(row: &sqlx::mysql::MySqlRow, index: usize) -> String {
    // まず NaiveDateTime で試みる
    if let Ok(v) = row.try_get::<NaiveDateTime, _>(index) {
        return v.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    // nullable カラムでは Option<NaiveDateTime> が必要な場合がある
    match row.try_get::<Option<NaiveDateTime>, _>(index) {
        Ok(Some(v)) => return v.format("%Y-%m-%d %H:%M:%S").to_string(),
        Ok(None) => return String::from("NULL"),
        Err(_) => {}
    }
    // text protocol では文字列として返ってくる場合がある
    row.try_get::<String, _>(index)
        .unwrap_or_else(|_| String::from("NULL"))
}

/// TIMESTAMP カラムを文字列に変換する
///
/// MySQL の TIMESTAMP は timezone-aware なため、NaiveDateTime では型不一致エラーになる。
/// DateTime<Utc> → Option<DateTime<Utc>> → NaiveDateTime → String の順にフォールバックする。
/// NaiveDateTime フォールバックは text protocol など一部環境で TIMESTAMP が
/// timezone なしで返ってくるケースに備えるための念のための対処。
fn decode_timestamp(row: &sqlx::mysql::MySqlRow, index: usize) -> String {
    // まず DateTime<Utc> で試みる（標準的なTIMESTAMPのデコードパス）
    if let Ok(v) = row.try_get::<DateTime<Utc>, _>(index) {
        return v.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    // nullable カラムでは Option<DateTime<Utc>> が必要な場合がある
    match row.try_get::<Option<DateTime<Utc>>, _>(index) {
        Ok(Some(v)) => return v.format("%Y-%m-%d %H:%M:%S").to_string(),
        Ok(None) => return String::from("NULL"),
        Err(_) => {}
    }
    // text protocol など一部環境では NaiveDateTime として返ってくる場合がある
    if let Ok(v) = row.try_get::<NaiveDateTime, _>(index) {
        return v.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    // 最終フォールバック: 文字列として取得
    row.try_get::<String, _>(index)
        .unwrap_or_else(|_| String::from("NULL"))
}

/// DATE カラムを文字列に変換する
///
/// NaiveDate → Option<NaiveDate> → String の順にフォールバックする。
fn decode_naive_date(row: &sqlx::mysql::MySqlRow, index: usize) -> String {
    if let Ok(v) = row.try_get::<NaiveDate, _>(index) {
        return v.format("%Y-%m-%d").to_string();
    }
    match row.try_get::<Option<NaiveDate>, _>(index) {
        Ok(Some(v)) => return v.format("%Y-%m-%d").to_string(),
        Ok(None) => return String::from("NULL"),
        Err(_) => {}
    }
    row.try_get::<String, _>(index)
        .unwrap_or_else(|_| String::from("NULL"))
}

/// TIME カラムを文字列に変換する
///
/// NaiveTime → Option<NaiveTime> → String の順にフォールバックする。
fn decode_naive_time(row: &sqlx::mysql::MySqlRow, index: usize) -> String {
    if let Ok(v) = row.try_get::<NaiveTime, _>(index) {
        return v.format("%H:%M:%S").to_string();
    }
    match row.try_get::<Option<NaiveTime>, _>(index) {
        Ok(Some(v)) => return v.format("%H:%M:%S").to_string(),
        Ok(None) => return String::from("NULL"),
        Err(_) => {}
    }
    row.try_get::<String, _>(index)
        .unwrap_or_else(|_| String::from("NULL"))
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
            dyn futures::Stream<Item = std::result::Result<sqlx::mysql::MySqlRow, sqlx::Error>>
                + Send,
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

    // streamはconn_optへの可変借用を保持しているため、describeで再借用する前に
    // 明示的にdropしてボローチェッカーの制約を解除する
    drop(stream);

    // 0件結果でも列ヘッダーを表示できるよう、必要時のみメタデータを補完する
    // 専用コネクション(conn_opt)が存在する場合はそちらを使い、USEで切り替えた
    // セッション状態を維持する。プールから別のコネクションを取得すると
    // USE実行前のDBに対して describe が実行されてしまう。
    if data_rows.is_empty() && columns.is_empty() {
        let describe_result = if let Some(ref mut conn) = conn_opt {
            (&mut **conn).describe(sql).await
        } else {
            pool.describe(sql).await
        };
        match describe_result {
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

/// SQLの先頭コメント（`/* ... */` および `-- ...`）を読み飛ばし、最初の意味あるトークンを返す
///
/// claude.rs の is_write_sql から呼び出す。
fn first_meaningful_token_for_write_check(sql: &str) -> &str {
    let mut s = sql.trim();
    loop {
        if s.starts_with("/*") {
            if let Some(end) = s.find("*/") {
                s = s[end + 2..].trim_start();
            } else {
                return "";
            }
        } else if s.starts_with("--") {
            if let Some(newline) = s.find('\n') {
                s = s[newline + 1..].trim_start();
            } else {
                return "";
            }
        } else {
            break;
        }
    }
    s.split_whitespace().next().unwrap_or("")
}

/// CTE（WITH句）の後に続くSQL本体が書き込みDMLかどうかを判定する
///
/// WITH句のCTE定義を括弧のネストで追跡し、全CTE定義の終了後の先頭トークンを確認する。
/// 複数CTEのカンマ区切り（`WITH a AS (...), b AS (...)`）にも対応する。
fn cte_contains_write_op(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    let write_keywords = ["INSERT", "UPDATE", "DELETE"];

    let start = match upper.find("WITH") {
        Some(pos) => pos + 4,
        None => return false,
    };

    let bytes = upper.as_bytes();
    let len = bytes.len();
    let mut depth = 0i32;
    let mut i = start;

    while i < len {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    let remaining = upper[i..].trim_start();
                    if remaining.starts_with(',') {
                        i += upper[i..].len() - remaining.len() + 1;
                        continue;
                    }
                    let token = remaining.split_whitespace().next().unwrap_or("");
                    return write_keywords.contains(&token);
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    false
}

/// 書き込み系SQLかどうかを判定する（claude.rs から使用）
///
/// readonlyモードでブロックすべきSQL文の先頭トークンをチェックする。
/// サーバー側でもブロックされるが、エージェントコンテキストでの即時フィードバックのためクライアントでも検査する。
/// コメント（`/* */` ブロック・`--` 行）を読み飛ばし、CTE（WITH句）も正しく判定する。
pub fn is_write_sql(sql: &str) -> bool {
    let first_token = first_meaningful_token_for_write_check(sql).to_uppercase();

    if first_token == "WITH" {
        return cte_contains_write_op(sql);
    }

    matches!(
        first_token.as_str(),
        "INSERT"
            | "UPDATE"
            | "DELETE"
            | "DROP"
            | "ALTER"
            | "TRUNCATE"
            | "CREATE"
            | "REPLACE"
            | "RENAME"
            | "GRANT"
            | "REVOKE"
    )
}

/// クエリ実行（タイムアウト付き）
///
/// `current_database` が指定されている場合、専用コネクションで USE を先行実行してからクエリを実行する。
/// execute_query と同様の理由による。
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
